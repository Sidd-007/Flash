use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, RANGE};
use reqwest::{Client, StatusCode};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::checksum::Checksum;
use crate::filename;
use crate::limit::RateLimiter;
use crate::plan::Plan;
use crate::probe::{self, Probe};
use crate::state::SavedState;

/// Segments smaller than this aren't worth another connection.
const MIN_SPLIT: u64 = 1 << 20;
pub(crate) const MAX_CONNECTIONS: usize = 32;
/// Consecutive failures without progress before a segment gives up.
pub(crate) const MAX_RETRIES: u32 = 5;
const SAVE_EVERY: Duration = Duration::from_secs(2);
/// Chunks buffered between the network and the disk writer.
const WRITE_QUEUE: usize = 256;

/// What to download and where.
#[derive(Debug, Clone)]
pub struct Request {
    pub url: String,
    pub dir: PathBuf,
    /// Overrides the name taken from the server or URL.
    pub file_name: Option<String>,
    pub connections: usize,
    /// Extra headers sent with every request (cookies, referer, ...).
    pub headers: HeaderMap,
    /// Every chunk passes through all of these (e.g. a global and a
    /// per-download limit).
    pub limiters: Vec<Arc<RateLimiter>>,
    /// Verified after the download completes.
    pub checksum: Option<Checksum>,
}

/// Facts about a prepared job, for display.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub file_name: String,
    /// Where the finished file will be.
    pub path: PathBuf,
    pub total: Option<u64>,
    /// Whether the server supports ranges, so the job can be paused and resumed.
    pub resumable: bool,
    /// Bytes already on disk from an earlier run.
    pub resumed_from: u64,
    pub connections: usize,
}

/// Live counters, safe to read from another task while the job runs.
#[derive(Debug, Default)]
pub struct Progress {
    downloaded: AtomicU64,
    pub(crate) active: AtomicUsize,
    /// HLS only: segments finished / in the playlist.
    pub(crate) parts_done: AtomicU64,
    parts_total: AtomicU64,
    /// Size learned while running (0 = unknown), for sources like yt-dlp.
    total: AtomicU64,
    plan: OnceLock<Arc<Mutex<Plan>>>,
}

/// A byte range `[start, end)` that is not on disk yet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SegmentView {
    pub start: u64,
    pub end: u64,
    /// A connection is downloading it right now.
    pub active: bool,
}

impl Progress {
    pub(crate) fn add_downloaded(&self, n: u64) {
        self.downloaded.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn sub_downloaded(&self, n: u64) {
        self.downloaded.fetch_sub(n, Ordering::Relaxed);
    }

    pub(crate) fn set_downloaded(&self, n: u64) {
        self.downloaded.store(n, Ordering::Relaxed);
    }

    pub(crate) fn set_total(&self, n: u64) {
        self.total.store(n, Ordering::Relaxed);
    }

    /// The size, if it only became known while downloading.
    pub fn total(&self) -> Option<u64> {
        let t = self.total.load(Ordering::Relaxed);
        (t > 0).then_some(t)
    }

    pub(crate) fn set_parts(&self, done: u64, total: u64) {
        self.parts_done.store(done, Ordering::Relaxed);
        self.parts_total.store(total, Ordering::Relaxed);
    }

    /// For HLS downloads: (segments done, segments total).
    pub fn parts(&self) -> Option<(u64, u64)> {
        let total = self.parts_total.load(Ordering::Relaxed);
        (total > 0).then(|| (self.parts_done.load(Ordering::Relaxed), total))
    }

    /// The ranges still missing, for drawing a segment map. Empty before
    /// the job starts and for non-segmented downloads.
    pub fn segments(&self) -> Vec<SegmentView> {
        self.plan.get().map(|p| p.lock().unwrap().views()).unwrap_or_default()
    }

    /// Bytes on disk, including any from an earlier run.
    pub fn downloaded(&self) -> u64 {
        self.downloaded.load(Ordering::Relaxed)
    }

    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub enum Outcome {
    Completed(PathBuf),
    /// Stopped by cancellation. If `resumable`, preparing the same request
    /// again continues where this left off.
    Paused { resumable: bool },
}

struct Paths {
    done: PathBuf,
    part: PathBuf,
    state: PathBuf,
}

impl Paths {
    fn new(dir: &Path, name: &str) -> Self {
        Self {
            done: dir.join(name),
            part: dir.join(format!("{name}.part")),
            state: dir.join(format!("{name}.rdm.json")),
        }
    }
}

/// A download that has been probed and is ready to run.
pub struct Job {
    client: Client,
    url: Url,
    headers: HeaderMap,
    limiters: Vec<Arc<RateLimiter>>,
    checksum: Option<Checksum>,
    info: JobInfo,
    paths: Paths,
    probe: Probe,
    plan: Plan,
    fresh: bool,
    progress: Arc<Progress>,
}

/// Points the unfinished download at `path` to a new link for the same file,
/// e.g. a fresh signed URL after the old one expired, keeping the bytes
/// already on disk. The new link must serve the same size with ranges.
pub async fn relink(client: &Client, path: &Path, new_url: &str, headers: &HeaderMap) -> Result<()> {
    let url = Url::parse(new_url).with_context(|| format!("invalid URL: {new_url}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("only http and https URLs are supported");
    }
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        bail!("not a file path: {}", path.display());
    };
    let paths = Paths::new(dir, name);
    let saved = SavedState::load(&paths.state).filter(|_| paths.part.exists());
    let Some(mut saved) = saved else {
        bail!("there's no unfinished download of {name} to continue");
    };
    let probe = probe::probe(client, &url, headers).await?;
    if probe.total != Some(saved.total) {
        let size = probe.total.map_or("unknown size".into(), size_text);
        bail!("the new link is a different file ({size}, expected {})", size_text(saved.total));
    }
    if !probe.ranges {
        bail!("the new link's server can't continue partial downloads");
    }
    saved.url = url.as_str().to_owned();
    saved.etag = probe.etag;
    saved.last_modified = probe.last_modified;
    saved.save(&paths.state)
}

/// `9800000000` -> `9.1 GB`, for messages.
fn size_text(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let (mut n, mut i) = (n as f64, 0);
    while n >= 1024.0 && i < UNITS.len() - 1 {
        n /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n:.0} {}", UNITS[i]) } else { format!("{n:.1} {}", UNITS[i]) }
}

impl Job {
    /// Probes the server, chooses the file name and loads any saved progress.
    pub async fn prepare(client: Client, req: Request) -> Result<Self> {
        let url = Url::parse(&req.url).with_context(|| format!("invalid URL: {}", req.url))?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("only http and https URLs are supported");
        }
        let probe = probe::probe(&client, &url, &req.headers).await?;
        fs::create_dir_all(&req.dir)
            .with_context(|| format!("could not create {}", req.dir.display()))?;

        let base = filename::sanitize(
            &req.file_name
                .clone()
                .or_else(|| probe.file_name.clone())
                .or_else(|| filename::from_url(&url))
                .unwrap_or_default(),
        );

        // Pick the first name that is either our own unfinished download of
        // this URL, or free. Never overwrite a finished file or someone
        // else's unfinished one.
        let mut file_name = base.clone();
        let mut resume = None;
        for n in 1.. {
            let paths = Paths::new(&req.dir, &file_name);
            let saved = SavedState::load(&paths.state);
            if saved.as_ref().is_some_and(|s| s.url == url.as_str()) {
                resume = saved.filter(|s| paths.part.exists() && s.still_valid(&probe));
                break;
            }
            if saved.is_none() && !paths.done.exists() {
                break;
            }
            file_name = filename::numbered(&base, n);
        }
        let paths = Paths::new(&req.dir, &file_name);

        let connections = if probe.ranges { req.connections.clamp(1, MAX_CONNECTIONS) } else { 1 };
        let (plan, fresh) = match (&resume, probe.total) {
            (Some(saved), _) => (Plan::new(saved.remaining.iter().copied()), false),
            (None, Some(total)) if probe.ranges => (Plan::even(total, connections, MIN_SPLIT), true),
            (None, total) => (Plan::new([(0, total.unwrap_or(u64::MAX))]), true),
        };
        let resumed_from = match (&resume, probe.total) {
            (Some(_), Some(total)) => total - plan.remaining_bytes(),
            _ => 0,
        };

        let progress = Arc::new(Progress::default());
        progress.downloaded.store(resumed_from, Ordering::Relaxed);

        Ok(Self {
            info: JobInfo {
                file_name,
                path: paths.done.clone(),
                total: probe.total,
                resumable: probe.ranges,
                resumed_from,
                connections,
            },
            client,
            url,
            headers: req.headers,
            limiters: req.limiters,
            checksum: req.checksum,
            paths,
            probe,
            plan,
            fresh,
            progress,
        })
    }

    pub fn info(&self) -> &JobInfo {
        &self.info
    }

    pub fn progress(&self) -> Arc<Progress> {
        self.progress.clone()
    }

    /// Downloads until complete, failed, or `cancel` fires (which pauses).
    pub async fn run(self, cancel: CancellationToken) -> Result<Outcome> {
        let Job { client, url, headers, limiters, checksum, info, paths, probe, plan, fresh, progress } =
            self;

        let file = Arc::new(open_part(&paths, fresh, probe.ranges.then_some(info.total).flatten())?);
        let plan = Arc::new(Mutex::new(plan));
        if probe.ranges {
            let _ = progress.plan.set(plan.clone());
        }
        let stop = cancel.child_token();
        let (tx, rx) = mpsc::channel(WRITE_QUEUE);

        let writer = tokio::task::spawn_blocking({
            let (file, plan, progress) = (file.clone(), plan.clone(), progress.clone());
            move || write_loop(&file, rx, &plan, &progress)
        });

        let saver = probe.ranges.then(|| Saver {
            file: file.clone(),
            plan: plan.clone(),
            path: paths.state.clone(),
            template: SavedState::new(url.as_str(), info.total.unwrap_or(0), &probe),
        });
        let saving_done = CancellationToken::new();
        let save_task = saver.clone().map(|s| tokio::spawn(save_loop(s, saving_done.clone())));

        let ctx = Arc::new(Ctx {
            client,
            url,
            headers,
            limiters,
            ranged: probe.ranges,
            plan: plan.clone(),
            progress,
            tx,
            stop: stop.clone(),
        });
        let mut workers = JoinSet::new();
        for _ in 0..info.connections {
            workers.spawn(worker(ctx.clone()));
        }
        // Workers hold the only senders now; the writer ends when they do.
        drop(ctx);

        let mut error = None;
        while let Some(joined) = workers.join_next().await {
            if let Err(e) = joined.map_err(anyhow::Error::from).and_then(|r| r) {
                error.get_or_insert(e);
                stop.cancel();
            }
        }
        // A disk error is the root cause of any "writer stopped" worker errors.
        if let Err(e) = writer.await? {
            error = Some(e);
        }
        saving_done.cancel();
        if let Some(task) = save_task {
            task.await?;
        }

        let complete = {
            let plan = plan.lock().unwrap();
            plan.is_complete()
                || (info.total.is_none() && error.is_none() && !cancel.is_cancelled())
        };
        if complete {
            file.sync_all()?;
            drop(file);
            fs::rename(&paths.part, &paths.done)
                .with_context(|| format!("could not move file to {}", paths.done.display()))?;
            remove_if_exists(&paths.state)?;
            if let Some(sum) = checksum {
                let path = paths.done.clone();
                tokio::task::spawn_blocking(move || sum.verify(&path))
                    .await?
                    .with_context(|| format!("{} was downloaded but failed verification", info.file_name))?;
            }
            return Ok(Outcome::Completed(paths.done));
        }

        if let Some(saver) = saver {
            tokio::task::spawn_blocking(move || saver.save()).await??;
        }
        match error {
            Some(e) => Err(e),
            None => Ok(Outcome::Paused { resumable: probe.ranges }),
        }
    }
}

fn open_part(paths: &Paths, fresh: bool, preallocate: Option<u64>) -> Result<File> {
    if fresh {
        remove_if_exists(&paths.state)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(fresh)
        .open(&paths.part)
        .with_context(|| format!("could not open {}", paths.part.display()))?;
    if let (true, Some(total)) = (fresh, preallocate) {
        // Sparse on APFS: reserves the size without writing zeros.
        file.set_len(total)?;
    }
    Ok(file)
}

fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Shared by every worker of one job.
struct Ctx {
    client: Client,
    url: Url,
    headers: HeaderMap,
    limiters: Vec<Arc<RateLimiter>>,
    ranged: bool,
    plan: Arc<Mutex<Plan>>,
    progress: Arc<Progress>,
    tx: mpsc::Sender<Chunk>,
    stop: CancellationToken,
}

struct Chunk {
    segment: usize,
    offset: u64,
    data: Bytes,
}

/// One connection: keeps taking segments until there is nothing left to take.
async fn worker(ctx: Arc<Ctx>) -> Result<()> {
    let min_split = if ctx.ranged { MIN_SPLIT } else { u64::MAX };
    while !ctx.stop.is_cancelled() {
        let Some(id) = ctx.plan.lock().unwrap().assign(min_split) else {
            break;
        };
        ctx.progress.active.fetch_add(1, Ordering::Relaxed);
        let result = download_segment(&ctx, id).await;
        ctx.progress.active.fetch_sub(1, Ordering::Relaxed);
        result?;
    }
    Ok(())
}

/// Streams one segment, retrying with backoff. Retries continue from the
/// last received byte, and the counter resets whenever a try made progress.
async fn download_segment(ctx: &Ctx, id: usize) -> Result<()> {
    let mut failures = 0;
    loop {
        let before = ctx.plan.lock().unwrap().segment(id).received;
        let Err(e) = stream_segment(ctx, id).await else {
            return Ok(());
        };
        if ctx.stop.is_cancelled() {
            return Ok(());
        }
        let progressed = ctx.plan.lock().unwrap().segment(id).received > before;
        failures = if progressed { 1 } else { failures + 1 };
        // Without ranges a retry would have to start from zero.
        if !ctx.ranged || failures > MAX_RETRIES {
            return Err(e);
        }
        tokio::select! {
            _ = ctx.stop.cancelled() => return Ok(()),
            _ = tokio::time::sleep(retry_delay(failures)) => {}
        }
    }
}

/// One HTTP request for whatever is left of segment `id`. Returns `Ok` when
/// the segment is finished or the job is stopping.
async fn stream_segment(ctx: &Ctx, id: usize) -> Result<()> {
    let (from, end) = {
        let plan = ctx.plan.lock().unwrap();
        let s = plan.segment(id);
        (s.received, s.end)
    };
    if from >= end {
        return Ok(());
    }

    let mut req = ctx.client.get(ctx.url.clone()).headers(ctx.headers.clone());
    if ctx.ranged {
        req = req.header(RANGE, format!("bytes={from}-{}", end - 1));
    }
    let resp = tokio::select! {
        _ = ctx.stop.cancelled() => return Ok(()),
        resp = req.send() => resp?.error_for_status()?,
    };
    if ctx.ranged && resp.status() != StatusCode::PARTIAL_CONTENT {
        bail!("server ignored the range request (status {})", resp.status());
    }

    let mut body = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = ctx.stop.cancelled() => return Ok(()),
            next = body.next() => next,
        };
        let Some(chunk) = next else { break };
        let mut data = chunk?;
        for limiter in &ctx.limiters {
            tokio::select! {
                _ = ctx.stop.cancelled() => return Ok(()),
                _ = limiter.acquire(data.len() as u64) => {}
            }
        }
        // The end may have moved since the request was sent (another worker
        // split this segment), so ask the plan how much of this chunk is ours.
        let (offset, n, done) = ctx.plan.lock().unwrap().claim(id, data.len());
        if n > 0 {
            data.truncate(n);
            ctx.tx
                .send(Chunk { segment: id, offset, data })
                .await
                .map_err(|_| anyhow!("disk writer stopped"))?;
        }
        if done {
            return Ok(());
        }
    }

    let plan = ctx.plan.lock().unwrap();
    let s = plan.segment(id);
    // u64::MAX marks a stream of unknown length, which ends when the body does.
    if s.received < s.end && s.end != u64::MAX {
        bail!("connection closed after {} of {} bytes", s.received - from, s.end - from);
    }
    Ok(())
}

/// Runs on a blocking thread: writes each chunk at its offset, then records it.
fn write_loop(
    file: &File,
    mut rx: mpsc::Receiver<Chunk>,
    plan: &Mutex<Plan>,
    progress: &Progress,
) -> Result<()> {
    while let Some(chunk) = rx.blocking_recv() {
        file.write_all_at(&chunk.data, chunk.offset).context("could not write to disk")?;
        let n = chunk.data.len() as u64;
        plan.lock().unwrap().mark_written(chunk.segment, n);
        progress.add_downloaded(n);
    }
    Ok(())
}

/// 0.5 s, 1 s, 2 s, ... after the 1st, 2nd, 3rd consecutive failure.
pub(crate) fn retry_delay(failures: u32) -> Duration {
    Duration::from_millis(500 << failures.clamp(1, 8).saturating_sub(1))
}

#[derive(Clone)]
struct Saver {
    file: Arc<File>,
    plan: Arc<Mutex<Plan>>,
    path: PathBuf,
    template: SavedState,
}

impl Saver {
    fn save(&self) -> Result<()> {
        // Snapshot first, then fsync: everything the snapshot calls written
        // is on disk before the state file says so.
        let remaining = self.plan.lock().unwrap().remaining();
        self.file.sync_data()?;
        SavedState { remaining, ..self.template.clone() }.save(&self.path)
    }
}

async fn save_loop(saver: Saver, done: CancellationToken) {
    loop {
        tokio::select! {
            _ = done.cancelled() => return,
            _ = tokio::time::sleep(SAVE_EVERY) => {}
        }
        let saver = saver.clone();
        // A failed periodic save only loses resume progress; the final save
        // in `run` reports errors.
        let _ = tokio::task::spawn_blocking(move || saver.save()).await;
    }
}
