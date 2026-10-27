//! Video-site downloads (YouTube, Vimeo, X, ...) by driving `yt-dlp`.
//!
//! Flash asks yt-dlp for the video's details first, then runs it with a
//! machine-readable progress template and relays the numbers into
//! [`Progress`]. Stopping kills yt-dlp; its `.part` files let the next run
//! continue where it left off.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::header::{AUTHORIZATION, COOKIE, HeaderMap, HeaderName, PROXY_AUTHORIZATION};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::filename;
use crate::job::{JobInfo, MAX_CONNECTIONS, Outcome, Progress};

/// Hosts yt-dlp handles that a plain HTTP download can't.
const VIDEO_SITES: &[&str] = &[
    "youtube.com", "youtu.be", "vimeo.com", "x.com", "twitter.com", "tiktok.com", "instagram.com",
    "facebook.com", "fb.watch", "reddit.com", "twitch.tv", "dailymotion.com", "soundcloud.com",
    "bilibili.com", "streamable.com", "loom.com", "bandcamp.com", "ted.com", "nicovideo.jp",
];

/// Whether `url` is on a site that needs yt-dlp.
pub fn is_video_site(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else { return false };
    let host = u.host_str().unwrap_or("").trim_start_matches("www.").trim_start_matches("m.");
    VIDEO_SITES.iter().any(|s| host == *s || host.ends_with(&format!(".{s}")))
}

/// How to install yt-dlp, for messages.
#[cfg(windows)]
pub const INSTALL_YTDLP: &str = "winget install yt-dlp.yt-dlp";
#[cfg(not(windows))]
pub const INSTALL_YTDLP: &str = "brew install yt-dlp";

/// The user's home folder.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// Finds a command-line tool. Apps opened from Finder or the Start menu may
/// get a bare `PATH`, so the usual package manager locations are searched too.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_owned() };
    search_path().into_iter().map(|dir| dir.join(&file)).find(|p| is_executable(p))
}

fn search_path() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if cfg!(windows) {
        // winget, Scoop and Chocolatey.
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(Path::new(&local).join(r"Microsoft\WinGet\Links"));
        }
        if let Some(home) = home_dir() {
            dirs.push(home.join(r"scoop\shims"));
        }
        if let Some(data) = std::env::var_os("ProgramData") {
            dirs.push(Path::new(&data).join(r"chocolatey\bin"));
        }
    } else {
        for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            dirs.push(extra.into());
        }
        if let Some(home) = home_dir() {
            dirs.push(home.join(".local/bin"));
        }
    }
    dirs
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Windows would otherwise open a console window for every helper we run.
#[cfg(windows)]
pub(crate) const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `PATH` for child processes, so yt-dlp can find ffmpeg and friends too.
fn child_path() -> std::ffi::OsString {
    std::env::join_paths(search_path()).unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct VideoRequest {
    /// The page URL, e.g. a YouTube watch link.
    pub url: String,
    pub dir: PathBuf,
    /// Name without extension; defaults to the video's title.
    pub file_name: Option<String>,
    /// "best", a height like "1080", or "audio".
    pub quality: Option<String>,
    pub connections: usize,
    pub headers: HeaderMap,
    /// Bytes/s, 0 = unlimited.
    pub rate_limit: u64,
    /// yt-dlp to use instead of searching for it (tests).
    pub ytdlp: Option<PathBuf>,
}

pub struct VideoJob {
    req: VideoRequest,
    ytdlp: PathBuf,
    ffmpeg: Option<PathBuf>,
    stem: String,
    info: JobInfo,
    progress: Arc<Progress>,
}

impl VideoJob {
    pub async fn prepare(req: VideoRequest) -> Result<Self> {
        let ytdlp = req
            .ytdlp
            .clone()
            .or_else(|| find_tool("yt-dlp"))
            .with_context(|| format!("yt-dlp is not installed. Install it with: {INSTALL_YTDLP}"))?;
        let ffmpeg = find_tool("ffmpeg");

        // Lives until yt-dlp is done with it, then deletes itself.
        let cookies = CookieFile::create(&req.url, &req.headers)?;
        let mut cmd = Command::new(&ytdlp);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.args(["-J", "--no-playlist", "--no-warnings"]).args(header_args(&req.headers));
        if let Some(c) = &cookies {
            cmd.arg("--cookies").arg(&c.path);
        }
        cmd.arg(&req.url);
        cmd.env("PATH", child_path()).stdin(Stdio::null()).kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_secs(90), cmd.output())
            .await
            .context("yt-dlp took too long to read the video's details")??;
        if !out.status.success() {
            bail!("yt-dlp couldn't read this page: {}", last_lines(&out.stderr));
        }
        let meta: serde_json::Value =
            serde_json::from_slice(&out.stdout).context("yt-dlp returned details Flash couldn't read")?;
        if meta.get("_type").and_then(|t| t.as_str()) == Some("playlist") {
            bail!("this is a playlist; add the individual videos instead");
        }

        let title = meta["title"].as_str().or(meta["id"].as_str()).unwrap_or("video");
        let stem = filename::sanitize(req.file_name.as_deref().unwrap_or(title));
        let stem = unique_stem(&req.dir, &stem);
        let audio = req.quality.as_deref() == Some("audio");
        let ext = if audio { "m4a" } else if ffmpeg.is_some() { "mp4" } else { meta["ext"].as_str().unwrap_or("mp4") };
        let total = meta["filesize"].as_u64().or(meta["filesize_approx"].as_u64());

        std::fs::create_dir_all(&req.dir)?;
        let progress = Arc::new(Progress::default());
        Ok(Self {
            info: JobInfo {
                file_name: format!("{stem}.{ext}"),
                path: req.dir.join(format!("{stem}.{ext}")),
                total,
                resumable: true,
                resumed_from: 0,
                connections: req.connections.clamp(1, MAX_CONNECTIONS),
            },
            req,
            ytdlp,
            ffmpeg,
            stem,
            progress,
        })
    }

    pub fn info(&self) -> &JobInfo {
        &self.info
    }

    pub fn progress(&self) -> Arc<Progress> {
        self.progress.clone()
    }

    pub async fn run(self, cancel: CancellationToken) -> Result<Outcome> {
        let template = self.req.dir.join(format!("{}.%(ext)s", self.stem));
        let mut cmd = Command::new(&self.ytdlp);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.args(["--newline", "--progress", "--no-warnings", "--no-playlist", "--no-mtime", "--continue"])
            .args(["-f", &format_for(self.req.quality.as_deref(), self.ffmpeg.is_some())])
            .arg("-o")
            .arg(&template)
            .args(["--concurrent-fragments", &self.info.connections.to_string()])
            .args([
                "--progress-template",
                "download:RDM|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(info.format_id)s",
            ])
            .args(["--print", "after_move:RDMFILE|%(filepath)s"]);
        if let Some(ff) = &self.ffmpeg {
            cmd.arg("--ffmpeg-location").arg(ff);
            if self.req.quality.as_deref() != Some("audio") {
                cmd.args(["--merge-output-format", "mp4"]);
            }
        }
        if self.req.rate_limit > 0 {
            cmd.args(["--limit-rate", &self.req.rate_limit.to_string()]);
        }
        let cookies = CookieFile::create(&self.req.url, &self.req.headers)?;
        cmd.args(header_args(&self.req.headers));
        if let Some(c) = &cookies {
            cmd.arg("--cookies").arg(&c.path);
        }
        cmd.arg(&self.req.url);
        cmd.env("PATH", child_path()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn().with_context(|| format!("could not start {}", self.ytdlp.display()))?;
        let mut stdout = BufReader::new(child.stdout.take().context("no stdout")?).lines();
        let stderr = child.stderr.take().context("no stderr")?;
        let errors = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            let mut tail = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                tail.push(line);
                if tail.len() > 8 {
                    tail.remove(0);
                }
            }
            tail
        });

        self.progress.active.store(1, Ordering::Relaxed);
        let mut tracker = Tracker::default();
        let mut final_path = None;
        loop {
            let line = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    self.progress.active.store(0, Ordering::Relaxed);
                    return Ok(Outcome::Paused { resumable: true });
                }
                line = stdout.next_line() => line?,
            };
            let Some(line) = line else { break };
            if let Some(path) = line.strip_prefix("RDMFILE|") {
                final_path = Some(PathBuf::from(path.trim()));
            } else if let Some(fields) = line.strip_prefix("RDM|")
                && let Some((done, total)) = tracker.update(fields)
            {
                self.progress.set_downloaded(done);
                if let Some(t) = total {
                    self.progress.set_total(t);
                }
            }
        }
        let status = child.wait().await?;
        self.progress.active.store(0, Ordering::Relaxed);
        let tail = errors.await.unwrap_or_default();
        if !status.success() {
            let msg = tail.iter().rev().find(|l| l.contains("ERROR")).cloned().unwrap_or_else(|| tail.join(" "));
            bail!("yt-dlp failed: {}", msg.trim_start_matches("ERROR: "));
        }
        Ok(Outcome::Completed(final_path.unwrap_or(self.info.path)))
    }
}

/// yt-dlp format selectors. Without ffmpeg only single-file formats work,
/// which on YouTube usually tops out at 720p.
pub fn format_for(quality: Option<&str>, has_ffmpeg: bool) -> String {
    match quality.unwrap_or("best") {
        "audio" => "ba[ext=m4a]/ba/b".into(),
        "best" | "" => if has_ffmpeg { "bv*+ba/b".into() } else { "b".into() },
        h if h.chars().all(|c| c.is_ascii_digit()) => {
            if has_ffmpeg {
                format!("bv*[height<={h}]+ba/b[height<={h}]/b")
            } else {
                format!("b[height<={h}]/b")
            }
        }
        other => other.to_owned(),
    }
}

/// Headers for yt-dlp's command line, where anything is visible to other
/// programs. Credentials never go there: cookies travel in a [`CookieFile`],
/// and other credentials are dropped, since yt-dlp would send them to every
/// host it contacts.
fn header_args(headers: &HeaderMap) -> Vec<String> {
    headers
        .iter()
        .filter(|(k, _)| !is_credential(k))
        .filter_map(|(k, v)| Some(vec!["--add-header".into(), format!("{}:{}", k.as_str(), v.to_str().ok()?)]))
        .flatten()
        .collect()
}

fn is_credential(name: &HeaderName) -> bool {
    name == COOKIE || name == AUTHORIZATION || name == PROXY_AUTHORIZATION
}

/// The browser's cookies for a video page, as a cookies.txt that only the
/// user can read. Each cookie is scoped to the page's host (and to HTTPS when
/// the page is), so yt-dlp can't send it anywhere else. The file and its
/// folder are deleted on drop: after success, failure and cancellation alike.
struct CookieFile {
    dir: PathBuf,
    path: PathBuf,
}

impl CookieFile {
    fn create(url: &str, headers: &HeaderMap) -> Result<Option<Self>> {
        let pairs: Vec<(&str, &str)> = headers
            .get_all(COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .filter_map(|c| c.trim().split_once('='))
            .filter(|(name, value)| !name.is_empty() && !(name.contains('\t') || value.contains('\t')))
            .collect();
        if pairs.is_empty() {
            return Ok(None);
        }
        let url = url::Url::parse(url).with_context(|| format!("invalid URL: {url}"))?;
        let host = url.host_str().context("the video URL has no host")?;
        let secure = if url.scheme() == "https" { "TRUE" } else { "FALSE" };
        let mut text = String::from("# Netscape HTTP Cookie File\n");
        for (name, value) in pairs {
            // domain, include subdomains, path, secure, expires (0 = session), name, value
            text.push_str(&format!("{host}\tFALSE\t/\t{secure}\t0\t{name}\t{value}\n"));
        }
        let dir = private_temp_dir().context("could not create a private folder for cookies")?;
        let file = Self { path: dir.join("cookies.txt"), dir };
        write_private(&file.path, text.as_bytes()).context("could not write the cookie file")?;
        Ok(Some(file))
    }
}

impl Drop for CookieFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A new folder in the temp directory that only the user can open.
fn private_temp_dir() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    for n in 0..100u32 {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let dir = base.join(format!("flash-{}-{nanos:x}-{n}", std::process::id()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

/// Writes a new file only the user can read (Windows keeps the temp folder per user).
fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(data)
}

/// Picks a name no finished file is using. Unfinished `.part` files with the
/// same name are ours from an earlier run, so they don't count.
fn unique_stem(dir: &Path, stem: &str) -> String {
    let taken = |s: &str| {
        std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with(&format!("{s}.")) && !name.contains(".part") && !name.ends_with(".ytdl")
        })
    };
    let mut candidate = stem.to_owned();
    for n in 1.. {
        if !taken(&candidate) {
            break;
        }
        candidate = format!("{stem} ({n})");
    }
    candidate
}

fn last_lines(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().rev().find(|l| l.contains("ERROR")).or(text.lines().last()).unwrap_or("unknown error");
    line.trim_start_matches("ERROR: ").to_owned()
}

/// Turns yt-dlp's per-stream progress (video, then audio) into one running total.
#[derive(Default)]
struct Tracker {
    format: String,
    /// Bytes of streams already finished.
    base: u64,
    current_total: u64,
    current_done: u64,
}

impl Tracker {
    /// `fields` = `downloaded|total|estimate|format_id`. Returns (done, total).
    fn update(&mut self, fields: &str) -> Option<(u64, Option<u64>)> {
        let mut parts = fields.split('|');
        let num = |s: Option<&str>| s.and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v as u64);
        let done = num(parts.next())?;
        let total = num(parts.next());
        let estimate = num(parts.next());
        let format = parts.next().unwrap_or("").trim().to_owned();
        if !self.format.is_empty() && format != self.format {
            // Next stream started; bank the previous one.
            self.base += self.current_total.max(self.current_done);
            self.current_total = 0;
        }
        self.format = format;
        self.current_done = done;
        if let Some(t) = total.or(estimate) {
            self.current_total = t;
        }
        let total = (self.current_total > 0).then_some(self.base + self.current_total);
        Some((self.base + done, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_video_sites() {
        assert!(is_video_site("https://www.youtube.com/watch?v=abc"));
        assert!(is_video_site("https://youtu.be/abc"));
        assert!(is_video_site("https://m.youtube.com/watch?v=abc"));
        assert!(is_video_site("https://player.vimeo.com/video/1"));
        assert!(!is_video_site("https://notyoutube.com/x"));
        assert!(!is_video_site("https://example.com/a.mp4"));
    }

    #[test]
    fn picks_formats() {
        assert_eq!(format_for(None, true), "bv*+ba/b");
        assert_eq!(format_for(Some("best"), false), "b");
        assert_eq!(format_for(Some("1080"), true), "bv*[height<=1080]+ba/b[height<=1080]/b");
        assert_eq!(format_for(Some("720"), false), "b[height<=720]/b");
        assert_eq!(format_for(Some("audio"), false), "ba[ext=m4a]/ba/b");
    }

    #[test]
    fn tracks_progress_across_streams() {
        let mut t = Tracker::default();
        assert_eq!(t.update("100|1000|NA|137"), Some((100, Some(1000))));
        assert_eq!(t.update("1000|1000|NA|137"), Some((1000, Some(1000))));
        // Audio stream starts: video's 1000 bytes are banked.
        assert_eq!(t.update("50|NA|200|140"), Some((1050, Some(1200))));
        assert_eq!(t.update("NA|NA|NA|140"), None);
    }
}
