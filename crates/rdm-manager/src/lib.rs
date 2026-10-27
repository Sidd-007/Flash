//! Download manager: a persistent queue of downloads on top of `rdm-core`.
//!
//! [`Manager`] keeps every download in SQLite, runs up to `max_active` at a
//! time, honours the schedule window and speed limits, and broadcasts
//! [`Event`]s for a UI to render.

mod db;
mod settings;
mod stats;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rdm_core::{Checksum, HlsJob, HlsRequest, Job, JobInfo, Outcome, Progress, RateLimiter, Request, VideoJob, VideoRequest};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Serialize;
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

pub use db::{Download, Group, Kind, Status};
pub use stats::{Day, Stats};
pub use rdm_core::SegmentView;
pub use rdm_core::video::find_tool;
pub use settings::{Category, Schedule, Settings, Theme};

use db::{Db, HistoryEntry, NewDownload};

const TICK: Duration = Duration::from_millis(500);
/// Automatic retries after network failures before giving up.
const MAX_AUTO_RETRIES: u32 = 20;
/// First wait before an automatic retry; doubles each time up to `MAX_RETRY_WAIT`.
const FIRST_RETRY_WAIT: Duration = Duration::from_secs(10);
const MAX_RETRY_WAIT: Duration = Duration::from_secs(300);
/// Ticks between writes of live byte counts to the database.
const PERSIST_EVERY: u32 = 10;

/// What to add. Each URL may be a batch pattern like `file[01-20].zip`.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct AddRequest {
    pub urls: Vec<String>,
    /// Folder to save into; `None` = the default folder (and categories apply).
    pub dir: Option<PathBuf>,
    /// Only used when exactly one URL is added.
    pub file_name: Option<String>,
    pub connections: Option<usize>,
    /// Bytes/s, 0 = unlimited.
    pub speed_limit: Option<u64>,
    pub checksum: Option<String>,
    pub headers: Vec<(String, String)>,
    /// `None` = detect from the URL.
    pub kind: Option<Kind>,
    /// Video downloads: "best", "1080", "720", "audio". `None` = the default.
    pub quality: Option<String>,
    /// `false` adds it paused.
    pub start: bool,
}

/// A download plus its live state, as shown in a UI.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadView {
    #[serde(flatten)]
    pub download: Download,
    /// Bytes/s.
    pub speed: f64,
    pub active_connections: usize,
    /// HLS: (segments done, segments total).
    pub parts: Option<(u64, u64)>,
    pub category: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LiveStat {
    pub id: i64,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub speed: f64,
    pub active_connections: usize,
    pub parts: Option<(u64, u64)>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A download was added or changed state.
    Changed { download: Box<DownloadView> },
    Removed { id: i64 },
    /// Live numbers for running downloads, every half second.
    Progress { items: Vec<LiveStat>, total_speed: f64 },
    /// A download finished, successfully or not.
    Finished { id: i64, name: String, path: Option<PathBuf>, ok: bool, error: Option<String> },
}

/// Something already downloaded (or in the list) that a new download matches.
#[derive(Debug, Clone, Serialize)]
pub struct Duplicate {
    pub id: i64,
    pub name: String,
    pub status: Status,
    pub path: Option<PathBuf>,
    pub total: Option<u64>,
    /// Matched by URL rather than by name and size.
    pub same_url: bool,
}

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

struct Inner {
    db: Mutex<Db>,
    settings: RwLock<Settings>,
    active: Mutex<HashMap<i64, Arc<Active>>>,
    global_limit: Arc<RateLimiter>,
    client: reqwest::Client,
    events: broadcast::Sender<Event>,
    wake: Notify,
}

/// Why a running download is being cancelled.
#[derive(Debug, Clone, Copy, PartialEq)]
enum StopReason {
    Pause,
    /// The schedule window closed: back to the queue.
    Requeue,
    Remove { delete_file: bool },
}

struct Active {
    cancel: CancellationToken,
    reason: Mutex<StopReason>,
    limiter: Arc<RateLimiter>,
    manual: bool,
    progress: Mutex<Option<Arc<Progress>>>,
    total: Mutex<Option<u64>>,
    speed: Mutex<Speed>,
    started: Instant,
}

struct Speed {
    /// Bytes already counted towards speed and traffic.
    bytes: u64,
    at: Instant,
    peak: f64,
    /// Exponential moving average, bytes/s.
    value: f64,
}

impl Active {
    fn stop(&self, reason: StopReason) {
        *self.reason.lock().unwrap() = reason;
        self.cancel.cancel();
    }

    fn progress(&self) -> Option<Arc<Progress>> {
        self.progress.lock().unwrap().clone()
    }
}

impl Manager {
    /// Opens (or creates) the database and starts the queue. Must be called
    /// inside a tokio runtime.
    pub fn open(db_path: &Path) -> Result<Self> {
        Self::with_db(Db::open(db_path)?, None)
    }

    /// An in-memory manager, for tests.
    pub fn in_memory(settings: Settings) -> Result<Self> {
        Self::with_db(Db::in_memory()?, Some(settings))
    }

    fn with_db(db: Db, settings: Option<Settings>) -> Result<Self> {
        let settings = settings.or(db.load_settings()?).unwrap_or_default().sanitized();
        db.save_settings(&settings)?;
        db.requeue_running()?;

        let inner = Arc::new(Inner {
            global_limit: Arc::new(RateLimiter::new(settings.speed_limit)),
            settings: RwLock::new(settings),
            db: Mutex::new(db),
            active: Mutex::new(HashMap::new()),
            client: rdm_core::default_client()?,
            events: broadcast::channel(256).0,
            wake: Notify::new(),
        });
        tokio::spawn(pump(Arc::downgrade(&inner)));
        tokio::spawn(ticker(Arc::downgrade(&inner)));
        Ok(Self { inner })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    pub fn settings(&self) -> Settings {
        self.inner.settings.read().unwrap().clone()
    }

    pub fn update_settings(&self, settings: Settings) -> Result<Settings> {
        let settings = settings.sanitized();
        self.inner.db.lock().unwrap().save_settings(&settings)?;
        self.inner.global_limit.set_rate(settings.speed_limit);
        *self.inner.settings.write().unwrap() = settings.clone();
        self.inner.wake.notify_one();
        Ok(settings)
    }

    /// Adds downloads and returns their ids.
    pub fn add(&self, req: AddRequest) -> Result<Vec<i64>> {
        let mut urls = Vec::new();
        for raw in req.urls.iter().map(|u| u.trim()).filter(|u| !u.is_empty()) {
            for url in rdm_core::pattern::expand(raw)? {
                let parsed = url::Url::parse(&url).with_context(|| format!("invalid URL: {url}"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    bail!("only http and https URLs are supported: {url}");
                }
                urls.push(url);
            }
        }
        if urls.is_empty() {
            bail!("no URL given");
        }
        if let Some(sum) = &req.checksum {
            sum.parse::<Checksum>()?;
        }
        header_map(&req.headers)?;

        let settings = self.settings();
        let single = urls.len() == 1;
        let db = self.inner.db.lock().unwrap();
        let mut ids = Vec::new();
        for url in urls {
            let kind = req.kind.unwrap_or_else(|| detect_kind(&url));
            if kind == Kind::Video && rdm_core::video::find_tool("yt-dlp").is_none() {
                bail!("video sites need yt-dlp. Install it with: brew install yt-dlp");
            }
            let id = db.insert(&NewDownload {
                kind,
                dir: req.dir.clone().unwrap_or_else(|| settings.download_dir.clone()),
                file_name: req.file_name.clone().filter(|n| single && !n.trim().is_empty()),
                status: if req.start { Status::Queued } else { Status::Paused },
                connections: req.connections.unwrap_or(settings.connections).clamp(1, 32),
                speed_limit: req.speed_limit.unwrap_or(0),
                checksum: req.checksum.clone().filter(|_| single),
                headers: req.headers.clone(),
                manual: false,
                auto_category: req.dir.is_none(),
                quality: (kind == Kind::Video)
                    .then(|| req.quality.clone().unwrap_or_else(|| settings.video_quality.clone())),
                url,
            })?;
            ids.push(id);
        }
        drop(db);
        for &id in &ids {
            self.emit_changed(id);
        }
        self.inner.wake.notify_one();
        Ok(ids)
    }

    /// Lifetime and recent download statistics.
    pub fn stats(&self) -> Result<Stats> {
        stats::compute(&self.inner.db.lock().unwrap(), chrono::Local::now().naive_local())
    }

    pub fn list(&self) -> Result<Vec<DownloadView>> {
        let rows = self.inner.db.lock().unwrap().list()?;
        Ok(rows.into_iter().map(|d| self.view(d)).collect())
    }

    pub fn get(&self, id: i64) -> Result<Option<DownloadView>> {
        let row = self.inner.db.lock().unwrap().get(id)?;
        Ok(row.map(|d| self.view(d)))
    }

    /// Missing byte ranges of a running download, for the segment map.
    pub fn segments(&self, id: i64) -> Vec<SegmentView> {
        let active = self.inner.active.lock().unwrap().get(&id).cloned();
        active.and_then(|a| a.progress()).map(|p| p.segments()).unwrap_or_default()
    }

    pub fn pause(&self, id: i64) -> Result<()> {
        if let Some(a) = self.inner.active.lock().unwrap().get(&id) {
            a.stop(StopReason::Pause);
            return Ok(());
        }
        let db = self.inner.db.lock().unwrap();
        if db.get(id)?.is_some_and(|d| d.status == Status::Queued) {
            db.set_status(id, Status::Paused, None)?;
            drop(db);
            self.emit_changed(id);
        }
        Ok(())
    }

    /// Starts (or retries) a download as soon as a slot is free, even
    /// outside the schedule window.
    pub fn resume(&self, id: i64) -> Result<()> {
        let db = self.inner.db.lock().unwrap();
        let Some(d) = db.get(id)? else { return Ok(()) };
        if matches!(d.status, Status::Paused | Status::Failed | Status::Queued) {
            db.set_status(id, Status::Queued, None)?;
            db.set_manual(id, true)?;
            db.clear_retry(id)?;
            drop(db);
            self.emit_changed(id);
            self.inner.wake.notify_one();
        }
        Ok(())
    }

    /// Downloads a finished or failed item again from scratch.
    pub fn redownload(&self, id: i64) -> Result<Vec<i64>> {
        let Some(d) = self.inner.db.lock().unwrap().get(id)? else { return Ok(vec![]) };
        self.add(AddRequest {
            urls: vec![d.url],
            dir: (!d.auto_category).then_some(d.dir),
            connections: Some(d.connections),
            speed_limit: Some(d.speed_limit),
            checksum: d.checksum,
            headers: d.headers,
            kind: Some(d.kind),
            quality: d.quality,
            start: true,
            ..Default::default()
        })
    }

    /// Continues a paused or failed file download from a new link to the same
    /// file, like IDM's "refresh download address": signed links from file
    /// hosts expire, and the bytes already downloaded are kept. The new link
    /// must serve the same size. `headers` replace the old ones unless empty.
    pub async fn relink(&self, id: i64, url: &str, headers: Vec<(String, String)>) -> Result<()> {
        if self.inner.active.lock().unwrap().contains_key(&id) {
            bail!("pause the download before changing its link");
        }
        let d = self.inner.db.lock().unwrap().get(id)?.context("download not found")?;
        if d.kind != Kind::File {
            bail!("only file downloads can switch links; video and stream downloads find theirs again on resume");
        }
        if d.status == Status::Completed {
            bail!("this download has already finished");
        }
        let headers = if headers.is_empty() { d.headers.clone() } else { headers };
        let map = header_map(&headers)?;
        // Keep the queue from starting it while the new link is checked.
        if d.status == Status::Queued {
            self.inner.db.lock().unwrap().set_status(id, Status::Paused, None)?;
        }
        let started = d.path.as_ref().filter(|p| PathBuf::from(format!("{}.rdm.json", p.display())).exists());
        let checked = match started {
            Some(path) => rdm_core::relink(&self.inner.client, path, url, &map).await,
            // Nothing on disk yet, so any link will do.
            _ => url::Url::parse(url).map(drop).with_context(|| format!("invalid URL: {url}")),
        };
        if let Err(e) = checked {
            if d.status == Status::Queued {
                self.inner.db.lock().unwrap().set_status(id, Status::Queued, None)?;
            }
            return Err(e);
        }
        self.inner.db.lock().unwrap().set_url(id, url, &headers)?;
        self.resume(id)
    }

    /// Downloads in the list that `url` (or a file called `file_name` of
    /// `size` bytes) would duplicate. Finished ones only count while their
    /// file still exists.
    pub fn duplicates(&self, url: &str, file_name: Option<&str>, size: Option<u64>) -> Result<Vec<Duplicate>> {
        let by_name = detect_kind(url) == Kind::File;
        let name = file_name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned)
            .unwrap_or_else(|| guess_name(url));
        // Names without an extension ("download", "file") match too much.
        let by_name = by_name && Path::new(&name).extension().is_some();
        let rows = self.inner.db.lock().unwrap().list()?;
        Ok(rows
            .into_iter()
            .filter_map(|d| {
                let d_name = d.file_name.clone().unwrap_or_else(|| guess_name(&d.url));
                let same_url = d.url == url;
                let same_file = by_name
                    && d.kind == Kind::File
                    && d_name.eq_ignore_ascii_case(&name)
                    && matches!((size, d.total), (Some(a), Some(b)) if a == b);
                let gone = d.status == Status::Completed && !d.path.as_ref().is_some_and(|p| p.exists());
                ((same_url || same_file) && !gone).then_some(Duplicate {
                    id: d.id,
                    name: d_name,
                    status: d.status,
                    path: d.path,
                    total: d.total,
                    same_url,
                })
            })
            .take(3)
            .collect())
    }

    /// Whether a copied link looks worth offering to download: a known file
    /// type, an HLS playlist, or a video site (when yt-dlp is installed).
    pub fn is_download_link(&self, url: &str) -> bool {
        let Ok(u) = url::Url::parse(url) else { return false };
        if !matches!(u.scheme(), "http" | "https") {
            return false;
        }
        if rdm_core::hls::looks_like_hls(url) {
            return true;
        }
        if rdm_core::video::is_video_site(url) {
            return rdm_core::video::find_tool("yt-dlp").is_some() && !u.path().trim_matches('/').is_empty();
        }
        let name = guess_name(url);
        self.inner.settings.read().unwrap().category_for(&name).is_some()
    }

    pub fn pause_all(&self) -> Result<()> {
        let ids: Vec<i64> = self.inner.db.lock().unwrap().list()?.iter()
            .filter(|d| matches!(d.status, Status::Queued | Status::Running))
            .map(|d| d.id)
            .collect();
        ids.into_iter().try_for_each(|id| self.pause(id))
    }

    pub fn resume_all(&self) -> Result<()> {
        let ids: Vec<i64> = self.inner.db.lock().unwrap().list()?.iter()
            .filter(|d| matches!(d.status, Status::Paused | Status::Failed))
            .map(|d| d.id)
            .collect();
        ids.into_iter().try_for_each(|id| self.resume(id))
    }

    /// Removes a download from the list. Unfinished data is always deleted;
    /// the finished file only when `delete_file` is set.
    pub fn remove(&self, id: i64, delete_file: bool) -> Result<()> {
        if let Some(a) = self.inner.active.lock().unwrap().get(&id) {
            // The download task cleans up once it has stopped.
            a.stop(StopReason::Remove { delete_file });
            return Ok(());
        }
        let db = self.inner.db.lock().unwrap();
        if let Some(d) = db.get(id)? {
            db.delete(id)?;
            drop(db);
            delete_files(&d, delete_file);
            let _ = self.inner.events.send(Event::Removed { id });
        }
        Ok(())
    }

    /// Removes every completed download from the list (files are kept).
    pub fn clear_completed(&self) -> Result<()> {
        let ids: Vec<i64> = self.inner.db.lock().unwrap().list()?.iter()
            .filter(|d| d.status == Status::Completed)
            .map(|d| d.id)
            .collect();
        ids.into_iter().try_for_each(|id| self.remove(id, false))
    }

    pub fn set_speed_limit(&self, id: i64, bytes_per_sec: u64) -> Result<()> {
        self.inner.db.lock().unwrap().set_speed_limit(id, bytes_per_sec)?;
        if let Some(a) = self.inner.active.lock().unwrap().get(&id) {
            a.limiter.set_rate(bytes_per_sec);
        }
        self.emit_changed(id);
        Ok(())
    }

    fn view(&self, download: Download) -> DownloadView {
        let active = self.inner.active.lock().unwrap().get(&download.id).cloned();
        let (speed, conns, parts, live) = match active.as_ref().and_then(|a| Some((a, a.progress()?))) {
            Some((a, p)) => (a.speed.lock().unwrap().value, p.active_connections(), p.parts(), Some(p.downloaded())),
            None => (0.0, 0, None, None),
        };
        let name = download.file_name.clone().unwrap_or_else(|| guess_name(&download.url));
        let category = self.inner.settings.read().unwrap().category_for(&name).map(|c| c.name.clone());
        let mut download = download;
        if let Some(bytes) = live {
            download.downloaded = bytes;
        }
        DownloadView { download, speed, active_connections: conns, parts, category }
    }

    fn emit_changed(&self, id: i64) {
        if let Ok(Some(view)) = self.get(id) {
            let _ = self.inner.events.send(Event::Changed { download: Box::new(view) });
        }
    }
}

/// Starts queued downloads whenever a slot frees up, and enforces the
/// schedule window.
async fn pump(inner: std::sync::Weak<Inner>) {
    loop {
        let Some(strong) = inner.upgrade() else { return };
        if let Err(e) = pump_once(&strong) {
            eprintln!("rdm: queue error: {e:#}");
        }
        let notified = async { strong.wake.notified().await };
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

fn pump_once(inner: &Arc<Inner>) -> Result<()> {
    let settings = inner.settings.read().unwrap().clone();
    let open = settings.schedule.in_window(chrono::Local::now().time());

    let mut active = inner.active.lock().unwrap();
    if !open {
        for a in active.values().filter(|a| !a.manual) {
            a.stop(StopReason::Requeue);
        }
    }
    let mut running = active.values().filter(|a| !a.cancel.is_cancelled()).count();
    let queued = inner.db.lock().unwrap().queued()?;
    for d in queued {
        if running >= settings.max_active {
            break;
        }
        if active.contains_key(&d.id) || (!open && !d.manual) || d.retry_at.is_some_and(|t| t > db::now_ms()) {
            continue;
        }
        let a = Arc::new(Active {
            cancel: CancellationToken::new(),
            reason: Mutex::new(StopReason::Pause),
            limiter: Arc::new(RateLimiter::new(d.speed_limit)),
            manual: d.manual,
            progress: Mutex::new(None),
            total: Mutex::new(d.total),
            speed: Mutex::new(Speed { bytes: d.downloaded, at: Instant::now(), value: 0.0, peak: 0.0 }),
            started: Instant::now(),
        });
        inner.db.lock().unwrap().set_status(d.id, Status::Running, None)?;
        active.insert(d.id, a.clone());
        running += 1;
        tokio::spawn(run_download(inner.clone(), d, a));
    }
    Ok(())
}

async fn run_download(inner: Arc<Inner>, d: Download, a: Arc<Active>) {
    let manager = Manager { inner: inner.clone() };
    manager.emit_changed(d.id);
    let result = download(&inner, &d, &a).await;
    let reason = *a.reason.lock().unwrap();
    let downloaded = a.progress().map(|p| p.downloaded());
    let cancelled = a.cancel.is_cancelled();
    inner.active.lock().unwrap().remove(&d.id);

    // Bytes since the ticker last looked; the lock keeps them from being counted twice.
    let (tail, peak) = {
        let mut s = a.speed.lock().unwrap();
        let tail = downloaded.map_or(0, |b| b.saturating_sub(s.bytes));
        if let Some(b) = downloaded {
            s.bytes = b;
        }
        (tail, s.peak)
    };
    let _ = inner.db.lock().unwrap().add_traffic(&stats::hour_key(chrono::Local::now().naive_local()), tail);
    let run = Run { active_ms: a.started.elapsed().as_millis() as u64, peak_speed: peak };

    if let Err(e) = finish(&inner, &d, result, reason, cancelled, downloaded, run) {
        eprintln!("rdm: could not record result of download {}: {e:#}", d.id);
    }
    if !matches!(reason, StopReason::Remove { .. }) {
        manager.emit_changed(d.id);
    }
    inner.wake.notify_one();
}

async fn download(inner: &Arc<Inner>, d: &Download, a: &Active) -> Result<Outcome> {
    let headers = header_map(&d.headers)?;
    let limiters = vec![inner.global_limit.clone(), a.limiter.clone()];
    let client = inner.client.clone();
    let cancel = a.cancel.clone();

    match d.kind {
        Kind::File => {
            let job = Job::prepare(client, Request {
                url: d.url.clone(),
                dir: d.dir.clone(),
                file_name: d.file_name.clone(),
                connections: d.connections,
                headers,
                limiters,
                checksum: d.checksum.as_deref().map(str::parse).transpose()?,
            });
            let job = tokio::select! {
                _ = cancel.cancelled() => return Ok(Outcome::Paused { resumable: true }),
                job = job => job?,
            };
            started(inner, d, a, job.info(), job.progress())?;
            job.run(cancel).await
        }
        Kind::Hls => {
            let stem = d.file_name.as_deref().map(|n| {
                Path::new(n).file_stem().map_or(n.to_owned(), |s| s.to_string_lossy().into_owned())
            });
            let job = HlsJob::prepare(client, HlsRequest {
                url: d.url.clone(),
                dir: d.dir.clone(),
                file_name: stem,
                connections: d.connections,
                headers,
                limiters,
            });
            let job = tokio::select! {
                _ = cancel.cancelled() => return Ok(Outcome::Paused { resumable: true }),
                job = job => job?,
            };
            started(inner, d, a, job.info(), job.progress())?;
            job.run(cancel).await
        }
        Kind::Video => {
            let stem = d.file_name.as_deref().map(|n| {
                Path::new(n).file_stem().map_or(n.to_owned(), |s| s.to_string_lossy().into_owned())
            });
            // yt-dlp does its own transfers, so pass the tighter of the limits.
            let rate_limit = [inner.global_limit.rate(), d.speed_limit].into_iter().filter(|&r| r > 0).min().unwrap_or(0);
            let job = VideoJob::prepare(VideoRequest {
                url: d.url.clone(),
                dir: d.dir.clone(),
                file_name: stem,
                quality: d.quality.clone(),
                connections: d.connections,
                headers,
                rate_limit,
                ytdlp: None,
            });
            let job = tokio::select! {
                _ = cancel.cancelled() => return Ok(Outcome::Paused { resumable: true }),
                job = job => job?,
            };
            started(inner, d, a, job.info(), job.progress())?;
            job.run(cancel).await
        }
    }
}

fn detect_kind(url: &str) -> Kind {
    if rdm_core::hls::looks_like_hls(url) {
        Kind::Hls
    } else if rdm_core::video::is_video_site(url) && rdm_core::video::find_tool("yt-dlp").is_some() {
        Kind::Video
    } else {
        Kind::File
    }
}

/// Whether a failure is worth retrying later: the network, not the file.
fn is_transient(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(r) = cause.downcast_ref::<reqwest::Error>() {
            return match r.status() {
                Some(s) => s.is_server_error() || matches!(s.as_u16(), 408 | 429),
                None => r.is_connect() || r.is_timeout() || r.is_request() || r.is_body(),
            };
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            // Disk errors (full, permissions) won't fix themselves.
            return matches!(
                io.kind(),
                ConnectionReset | ConnectionAborted | ConnectionRefused | NotConnected | TimedOut | BrokenPipe | UnexpectedEof
            );
        }
    }
    let msg = format!("{e:#}").to_ascii_lowercase();
    ["connection closed after", "timed out", "temporary failure", "network is unreachable", "connection reset",
     "http error 5", "nodename nor servname", "failed to resolve", "unable to download webpage"]
        .iter()
        .any(|needle| msg.contains(needle))
}

/// Records what the probe decided and starts tracking live progress.
fn started(inner: &Arc<Inner>, d: &Download, a: &Active, info: &JobInfo, progress: Arc<Progress>) -> Result<()> {
    inner.db.lock().unwrap().set_started(d.id, &info.file_name, &info.path, info.total, info.resumable)?;
    *a.speed.lock().unwrap() = Speed { bytes: progress.downloaded(), at: Instant::now(), value: 0.0, peak: 0.0 };
    *a.total.lock().unwrap() = info.total;
    *a.progress.lock().unwrap() = Some(progress);
    Manager { inner: inner.clone() }.emit_changed(d.id);
    Ok(())
}

/// Time spent and top speed during one start-to-stop run of a download.
struct Run {
    active_ms: u64,
    peak_speed: f64,
}

fn finish(
    inner: &Arc<Inner>,
    d: &Download,
    result: Result<Outcome>,
    reason: StopReason,
    cancelled: bool,
    downloaded: Option<u64>,
    run: Run,
) -> Result<()> {
    // Re-read: the probe filled in the name and path.
    let row = inner.db.lock().unwrap().get(d.id)?.unwrap_or_else(|| d.clone());
    let name = row.file_name.clone().unwrap_or_else(|| guess_name(&row.url));
    let downloaded = downloaded.unwrap_or(row.downloaded);
    let send = |e| {
        let _ = inner.events.send(e);
    };

    if let StopReason::Remove { delete_file } = reason {
        inner.db.lock().unwrap().delete(d.id)?;
        delete_files(&row, delete_file);
        send(Event::Removed { id: d.id });
        return Ok(());
    }

    let db = || inner.db.lock().unwrap();
    db().set_downloaded(d.id, downloaded)?;
    let (active_ms, peak_speed) = db().add_run(d.id, run.active_ms, run.peak_speed)?;
    let history = |ok: bool, name: &str| {
        let settings = inner.settings.read().unwrap();
        HistoryEntry {
            download_id: d.id,
            name: name.to_owned(),
            host: url::Url::parse(&row.url).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or_default(),
            category: settings.category_for(name).map_or_else(|| "Other".to_owned(), |c| c.name.clone()),
            bytes: downloaded,
            active_ms,
            peak_speed,
            ok,
        }
    };
    match result {
        Ok(Outcome::Completed(path)) => {
            let settings = inner.settings.read().unwrap().clone();
            let path = move_to_category(&settings, &row, path);
            db().set_completed(d.id, &path, downloaded)?;
            let final_name = path.file_name().map_or(name.clone(), |n| n.to_string_lossy().into_owned());
            db().add_history(&history(true, &final_name))?;
            send(Event::Finished { id: d.id, name, path: Some(path), ok: true, error: None });
        }
        Ok(Outcome::Paused { .. }) | Err(_) if cancelled => {
            let status = if reason == StopReason::Requeue { Status::Queued } else { Status::Paused };
            db().set_status(d.id, status, None)?;
        }
        Ok(Outcome::Paused { .. }) => db().set_status(d.id, Status::Paused, None)?,
        Err(e)
            if inner.settings.read().unwrap().auto_retry
                && row.auto_retries < MAX_AUTO_RETRIES
                && is_transient(&e) =>
        {
            let wait = (FIRST_RETRY_WAIT * 2u32.saturating_pow(row.auto_retries.min(16))).min(MAX_RETRY_WAIT);
            let note = format!("network problem, retrying in {}s · {e:#}", wait.as_secs());
            db().schedule_retry(d.id, db::now_ms() + wait.as_millis() as i64, row.auto_retries + 1, &note)?;
            let wake = Arc::downgrade(inner);
            tokio::spawn(async move {
                tokio::time::sleep(wait + Duration::from_millis(50)).await;
                if let Some(inner) = wake.upgrade() {
                    inner.wake.notify_one();
                }
            });
        }
        Err(e) => {
            let error = format!("{e:#}");
            db().set_status(d.id, Status::Failed, Some(&error))?;
            db().add_history(&history(false, &name))?;
            send(Event::Finished { id: d.id, name, path: row.path.clone(), ok: false, error: Some(error) });
        }
    }
    Ok(())
}

/// Moves a finished file into its category folder, if categories apply.
/// On any problem the file simply stays where it is.
fn move_to_category(settings: &Settings, d: &Download, path: PathBuf) -> PathBuf {
    if !d.auto_category || !settings.use_categories {
        return path;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
        return path;
    };
    let Some(category) = settings.category_for(&name) else { return path };
    let dir = settings.download_dir.join(&category.folder);
    if path.parent() == Some(dir.as_path()) || std::fs::create_dir_all(&dir).is_err() {
        return path;
    }
    let target = unique_path(&dir, &name);
    match std::fs::rename(&path, &target) {
        Ok(()) => target,
        Err(_) => path,
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    let mut path = dir.join(name);
    for n in 1.. {
        if !path.exists() {
            break;
        }
        path = dir.join(format!("{stem} ({n}){ext}"));
    }
    path
}

/// Deletes a download's partial data, and its finished file if asked.
fn delete_files(d: &Download, delete_final: bool) {
    let Some(path) = &d.path else { return };
    let sibling = |suffix: &str| PathBuf::from(format!("{}{suffix}", path.display()));
    let _ = std::fs::remove_file(sibling(".part"));
    let _ = std::fs::remove_file(sibling(".rdm.json"));
    let _ = std::fs::remove_dir_all(sibling(".hls-parts"));
    if d.kind == Kind::Hls {
        let _ = std::fs::remove_file(path.with_extension("ts.part"));
        let _ = std::fs::remove_file(path.with_extension("mp4.part"));
    }
    if d.kind == Kind::Video
        && let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str()))
    {
        // yt-dlp leaves per-stream pieces like `name.f137.mp4.part` and `name.ytdl`.
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(&format!("{stem}.")) && (name.contains(".part") || name.ends_with(".ytdl")) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if delete_final {
        let _ = std::fs::remove_file(path);
    }
}

/// Publishes live speed and progress twice a second.
async fn ticker(inner: std::sync::Weak<Inner>) {
    let mut ticks = 0u32;
    let mut was_busy = false;
    // Bytes transferred since the last write to the traffic table.
    let mut pending = 0u64;
    loop {
        tokio::time::sleep(TICK).await;
        let Some(inner) = inner.upgrade() else { return };
        let active: Vec<(i64, Arc<Active>)> =
            inner.active.lock().unwrap().iter().map(|(id, a)| (*id, a.clone())).collect();
        // One last empty update after the queue goes idle, so speeds reset.
        if active.is_empty() && !was_busy {
            continue;
        }
        was_busy = !active.is_empty();
        ticks = ticks.wrapping_add(1);

        let mut items = Vec::new();
        for (id, a) in active {
            let Some(p) = a.progress() else { continue };
            let bytes = p.downloaded();
            let speed = {
                let mut s = a.speed.lock().unwrap();
                let now = Instant::now();
                let dt = now.duration_since(s.at).as_secs_f64();
                let delta = bytes.saturating_sub(s.bytes);
                if dt > 0.0 {
                    let instant = delta as f64 / dt;
                    s.value = if s.value == 0.0 { instant } else { 0.3 * instant + 0.7 * s.value };
                    s.peak = s.peak.max(s.value);
                }
                pending += delta;
                s.bytes = bytes;
                s.at = now;
                s.value
            };
            if ticks.is_multiple_of(PERSIST_EVERY) {
                let _ = inner.db.lock().unwrap().set_downloaded(id, bytes);
            }
            items.push(LiveStat {
                id,
                downloaded: bytes,
                total: p.total().or(*a.total.lock().unwrap()),
                speed,
                active_connections: p.active_connections(),
                parts: p.parts(),
            });
        }
        let total_speed = items.iter().map(|i| i.speed).sum();
        let _ = inner.events.send(Event::Progress { items, total_speed });
        // Saved every tick so quitting loses at most half a second of traffic.
        if pending > 0 {
            let hour = stats::hour_key(chrono::Local::now().naive_local());
            if inner.db.lock().unwrap().add_traffic(&hour, pending).is_ok() {
                pending = 0;
            }
        }
    }
}

fn header_map(pairs: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.trim().as_bytes()).with_context(|| format!("invalid header name {name:?}"))?,
            HeaderValue::from_str(value.trim()).with_context(|| format!("invalid value for header {name:?}"))?,
        );
    }
    Ok(map)
}

/// A display name before the server has told us the real one.
fn guess_name(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| {
            let last = u.path_segments()?.rev().find(|s| !s.is_empty())?.to_owned();
            Some(percent_encoding::percent_decode_str(&last).decode_utf8_lossy().into_owned())
        })
        .unwrap_or_else(|| url.to_owned())
}
