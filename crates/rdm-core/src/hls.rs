//! HLS (`.m3u8`) video downloads: fetch every segment in parallel, decrypt
//! AES-128 if needed, and join them into one file. Finished segments are kept
//! in a `<name>.hls-parts` folder until the end, so an interrupted download
//! resumes by skipping them.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use aes::Aes128;
use anyhow::{Context, Result, anyhow, bail};
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use futures_util::StreamExt;
use reqwest::Client;
use reqwest::header::HeaderMap;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::filename;
use crate::job::{JobInfo, MAX_CONNECTIONS, MAX_RETRIES, Outcome, Progress, retry_delay};
use crate::limit::RateLimiter;

/// What to download and where.
#[derive(Debug, Clone)]
pub struct HlsRequest {
    /// A master or media playlist URL.
    pub url: String,
    pub dir: PathBuf,
    /// Name without extension; the extension comes from the stream format.
    pub file_name: Option<String>,
    pub connections: usize,
    pub headers: HeaderMap,
    pub limiters: Vec<Arc<RateLimiter>>,
}

/// Quick check for URLs that look like HLS playlists.
pub fn looks_like_hls(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
    path.ends_with(".m3u8") || path.ends_with(".m3u")
}

#[derive(Debug, Clone, PartialEq)]
struct Key {
    uri: Url,
    iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq)]
struct Segment {
    url: Url,
    key: Option<Key>,
    sequence: u64,
}

#[derive(Debug, PartialEq)]
enum Playlist {
    /// Variant streams as (bandwidth, url).
    Master(Vec<(u64, Url)>),
    Media { segments: Vec<Segment>, init: Option<Url> },
}

/// A probed HLS download, ready to run.
pub struct HlsJob {
    client: Client,
    /// The URL as requested, recorded so a rerun can find its parts folder.
    source: String,
    headers: HeaderMap,
    limiters: Vec<Arc<RateLimiter>>,
    segments: Vec<Segment>,
    init: Option<Url>,
    parts_dir: PathBuf,
    /// Joined output before any remux: `.ts`, or `.mp4` for fMP4 streams.
    joined: PathBuf,
    info: JobInfo,
    progress: Arc<Progress>,
}

impl HlsJob {
    pub async fn prepare(client: Client, req: HlsRequest) -> Result<Self> {
        let mut url = Url::parse(&req.url).with_context(|| format!("invalid URL: {}", req.url))?;
        let mut playlist = fetch_playlist(&client, &url, &req.headers).await?;
        if let Playlist::Master(variants) = &playlist {
            // Best quality = highest bandwidth.
            let (_, best) = variants.iter().max_by_key(|(bw, _)| *bw).context("empty master playlist")?;
            url = best.clone();
            playlist = fetch_playlist(&client, &url, &req.headers).await?;
        }
        let Playlist::Media { segments, init } = playlist else {
            bail!("master playlist points to another master playlist");
        };
        if segments.is_empty() {
            bail!("playlist has no segments");
        }

        let stem = filename::sanitize(&req.file_name.clone().unwrap_or_else(|| {
            filename::from_url(&url)
                .map(|n| n.trim_end_matches(".m3u8").trim_end_matches(".m3u").to_owned())
                .unwrap_or_else(|| "video".into())
        }));
        let fmp4 = init.is_some();
        let remux = !fmp4 && ffmpeg_available();
        let final_ext = if fmp4 || remux { "mp4" } else { "ts" };
        fs::create_dir_all(&req.dir)?;

        // Reuse our own unfinished parts folder; never clobber a finished file.
        let mut name = format!("{stem}.{final_ext}");
        for n in 1.. {
            let parts = req.dir.join(format!("{name}.hls-parts"));
            if fs::read_to_string(parts.join("source.txt")).is_ok_and(|s| s == req.url) {
                break;
            }
            if !parts.exists() && !req.dir.join(&name).exists() {
                break;
            }
            name = filename::numbered(&format!("{stem}.{final_ext}"), n);
        }
        let path = req.dir.join(&name);
        let parts_dir = req.dir.join(format!("{name}.hls-parts"));
        let joined = if remux { path.with_extension("ts.part") } else { path.with_extension(format!("{final_ext}.part")) };

        let progress = Arc::new(Progress::default());
        let (mut done, mut bytes) = (0, 0);
        for i in 0..segments.len() {
            if let Ok(meta) = fs::metadata(part_path(&parts_dir, i)) {
                done += 1;
                bytes += meta.len();
            }
        }
        progress.set_parts(done, segments.len() as u64);
        progress.add_downloaded(bytes);

        Ok(Self {
            info: JobInfo {
                file_name: name,
                path,
                total: None,
                resumable: true,
                resumed_from: bytes,
                connections: req.connections.clamp(1, MAX_CONNECTIONS),
            },
            client,
            source: req.url,
            headers: req.headers,
            limiters: req.limiters,
            segments,
            init,
            parts_dir,
            joined,
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
        fs::create_dir_all(&self.parts_dir)?;
        fs::write(self.parts_dir.join("source.txt"), &self.source)?;
        let job = Arc::new(self);
        let stop = cancel.child_token();

        if let Some(init) = &job.init {
            let path = job.parts_dir.join("init");
            if !path.exists() {
                let data = job.fetch(init, &stop).await?;
                write_atomic(&path, &data)?;
            }
        }

        let next = Arc::new(AtomicUsize::new(0));
        let keys = Arc::new(Mutex::new(HashMap::new()));
        let mut workers = JoinSet::new();
        for _ in 0..job.info.connections {
            let (job, next, keys, stop) = (job.clone(), next.clone(), keys.clone(), stop.clone());
            workers.spawn(async move { job.worker(&next, &keys, &stop).await });
        }
        let mut error = None;
        while let Some(joined) = workers.join_next().await {
            if let Err(e) = joined.map_err(anyhow::Error::from).and_then(|r| r) {
                error.get_or_insert(e);
                stop.cancel();
            }
        }
        if let Some(e) = error {
            return Err(e);
        }
        if cancel.is_cancelled() {
            return Ok(Outcome::Paused { resumable: true });
        }

        let job = Arc::try_unwrap(job).map_err(|_| anyhow!("workers still running"))?;
        tokio::task::spawn_blocking(move || job.finish()).await?
    }

    async fn worker(
        &self,
        next: &AtomicUsize,
        keys: &Mutex<HashMap<Url, [u8; 16]>>,
        stop: &CancellationToken,
    ) -> Result<()> {
        loop {
            let i = next.fetch_add(1, Ordering::Relaxed);
            let Some(seg) = self.segments.get(i) else { return Ok(()) };
            let path = part_path(&self.parts_dir, i);
            if path.exists() {
                continue;
            }
            self.progress.active.fetch_add(1, Ordering::Relaxed);
            let result = self.fetch_segment(seg, keys, stop).await;
            self.progress.active.fetch_sub(1, Ordering::Relaxed);
            let Some(data) = result? else { return Ok(()) };
            write_atomic(&path, &data)?;
            self.progress.parts_done.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// `None` when stopped.
    async fn fetch_segment(
        &self,
        seg: &Segment,
        keys: &Mutex<HashMap<Url, [u8; 16]>>,
        stop: &CancellationToken,
    ) -> Result<Option<Vec<u8>>> {
        let mut data = self.fetch(&seg.url, stop).await?;
        if stop.is_cancelled() {
            return Ok(None);
        }
        if let Some(key) = &seg.key {
            let cached = keys.lock().unwrap().get(&key.uri).copied();
            let k = match cached {
                Some(k) => k,
                None => {
                    let raw = self.fetch(&key.uri, stop).await?;
                    let k: [u8; 16] = raw.as_slice().try_into().map_err(|_| anyhow!("AES key must be 16 bytes"))?;
                    keys.lock().unwrap().insert(key.uri.clone(), k);
                    k
                }
            };
            let iv = key.iv.unwrap_or_else(|| (seg.sequence as u128).to_be_bytes());
            data = cbc::Decryptor::<Aes128>::new(&k.into(), &iv.into())
                .decrypt_padded_vec_mut::<Pkcs7>(&data)
                .map_err(|_| anyhow!("could not decrypt segment {}", seg.sequence))?;
        }
        Ok(Some(data))
    }

    /// GETs a whole (small) resource with retries and rate limiting.
    async fn fetch(&self, url: &Url, stop: &CancellationToken) -> Result<Vec<u8>> {
        let mut failures = 0;
        loop {
            let mut counted = 0;
            let attempt = async {
                let resp = self.client.get(url.clone()).headers(self.headers.clone()).send().await?;
                let mut body = resp.error_for_status()?.bytes_stream();
                let mut data = Vec::new();
                while let Some(chunk) = body.next().await {
                    let chunk = chunk?;
                    for l in &self.limiters {
                        l.acquire(chunk.len() as u64).await;
                    }
                    self.progress.add_downloaded(chunk.len() as u64);
                    counted += chunk.len() as u64;
                    data.extend_from_slice(&chunk);
                }
                anyhow::Ok(data)
            };
            let result = tokio::select! {
                _ = stop.cancelled() => Ok(Vec::new()),
                r = attempt => r,
            };
            match result {
                Ok(data) => return Ok(data),
                Err(e) => {
                    // Don't count bytes we'll fetch again.
                    self.progress.sub_downloaded(counted);
                    failures += 1;
                    if failures > MAX_RETRIES {
                        return Err(e.context(format!("failed to download {url}")));
                    }
                    tokio::select! {
                        _ = stop.cancelled() => return Ok(Vec::new()),
                        _ = tokio::time::sleep(retry_delay(failures)) => {}
                    }
                }
            }
        }
    }

    /// Joins the parts (blocking), remuxes `.ts` to `.mp4` when ffmpeg is
    /// available, and removes the parts folder.
    fn finish(self) -> Result<Outcome> {
        let mut out = File::create(&self.joined)?;
        let init = self.parts_dir.join("init");
        if init.exists() {
            io::copy(&mut File::open(&init)?, &mut out)?;
        }
        for i in 0..self.segments.len() {
            io::copy(&mut File::open(part_path(&self.parts_dir, i))?, &mut out)?;
        }
        out.sync_all()?;
        drop(out);

        let path = self.info.path;
        let ts_joined = self.joined.to_string_lossy().ends_with(".ts.part");
        let result = if ts_joined && path.extension().is_some_and(|e| e == "mp4") {
            match remux(&self.joined, &path) {
                Ok(()) => {
                    fs::remove_file(&self.joined)?;
                    path
                }
                // Keep the playable .ts rather than failing the whole download.
                Err(_) => {
                    let ts = path.with_extension("ts");
                    fs::rename(&self.joined, &ts)?;
                    ts
                }
            }
        } else {
            fs::rename(&self.joined, &path)?;
            path
        };
        fs::remove_dir_all(&self.parts_dir)?;
        Ok(Outcome::Completed(result))
    }
}

fn part_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("{i:06}.seg"))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    File::create(&tmp)?.write_all(data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn ffmpeg_available() -> bool {
    crate::video::find_tool("ffmpeg").is_some()
}

fn remux(input: &Path, output: &Path) -> Result<()> {
    let ffmpeg = crate::video::find_tool("ffmpeg").context("ffmpeg not found")?;
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, crate::video::CREATE_NO_WINDOW);
    let status = cmd
        .args(["-y", "-loglevel", "error", "-i"])
        .arg(input)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(output)
        .status()?;
    if !status.success() {
        bail!("ffmpeg exited with {status}");
    }
    Ok(())
}

async fn fetch_playlist(client: &Client, url: &Url, headers: &HeaderMap) -> Result<Playlist> {
    let text = client
        .get(url.clone())
        .headers(headers.clone())
        .send()
        .await
        .with_context(|| format!("could not reach {url}"))?
        .error_for_status()?
        .text()
        .await?;
    parse_playlist(&text, url)
}

fn parse_playlist(text: &str, base: &Url) -> Result<Playlist> {
    if !text.trim_start().starts_with("#EXTM3U") {
        bail!("not an HLS playlist");
    }
    let mut variants = Vec::new();
    let mut pending_bandwidth = None;
    let mut segments = Vec::new();
    let mut init = None;
    let mut key = None;
    let mut sequence = 0;
    let mut ended = false;

    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_bandwidth = Some(attr(attrs, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0));
        } else if let Some(n) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = n.trim().parse().context("bad media sequence")?;
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            key = match attr(attrs, "METHOD").as_deref() {
                Some("NONE") => None,
                Some("AES-128") => Some(Key {
                    uri: base.join(&attr(attrs, "URI").context("key has no URI")?)?,
                    iv: attr(attrs, "IV").map(|iv| parse_iv(&iv)).transpose()?,
                }),
                Some(m) => bail!("encryption method {m} is not supported (likely DRM)"),
                None => bail!("key has no METHOD"),
            };
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            if attr(attrs, "BYTERANGE").is_some() {
                bail!("byte-range init segments are not supported");
            }
            init = Some(base.join(&attr(attrs, "URI").context("map has no URI")?)?);
        } else if line.starts_with("#EXT-X-BYTERANGE") {
            bail!("byte-range playlists are not supported");
        } else if line.starts_with("#EXT-X-ENDLIST") {
            ended = true;
        } else if !line.starts_with('#') {
            let url = base.join(line)?;
            match pending_bandwidth.take() {
                Some(bw) => variants.push((bw, url)),
                None => {
                    segments.push(Segment { url, key: key.clone(), sequence });
                    sequence += 1;
                }
            }
        }
    }

    if !variants.is_empty() {
        return Ok(Playlist::Master(variants));
    }
    if !ended {
        bail!("live streams are not supported (playlist has no #EXT-X-ENDLIST)");
    }
    Ok(Playlist::Media { segments, init })
}

/// Reads `NAME=value` or `NAME="quoted, value"` from an attribute list.
fn attr(list: &str, name: &str) -> Option<String> {
    let mut rest = list;
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let (value, next) = match after.strip_prefix('"') {
            Some(q) => {
                let end = q.find('"')?;
                (&q[..end], q[end + 1..].trim_start_matches(','))
            }
            None => after.split_once(',').unwrap_or((after, "")),
        };
        if key.trim().eq_ignore_ascii_case(name) {
            return Some(value.to_owned());
        }
        rest = next;
    }
    None
}

fn parse_iv(s: &str) -> Result<[u8; 16]> {
    let hex = s.trim_start_matches("0x").trim_start_matches("0X");
    let n = u128::from_str_radix(hex, 16).context("bad IV")?;
    Ok(n.to_be_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.x.com/v/index.m3u8").unwrap()
    }

    #[test]
    fn detects_hls_urls() {
        assert!(looks_like_hls("https://a.com/x/master.m3u8?token=1"));
        assert!(!looks_like_hls("https://a.com/x.mp4"));
    }

    #[test]
    fn parses_master() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,CODECS=\"avc1,mp4a\"\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=5000000\nhigh/index.m3u8\n";
        let Playlist::Master(v) = parse_playlist(text, &base()).unwrap() else { panic!() };
        assert_eq!(v[0], (800_000, Url::parse("https://cdn.x.com/v/low.m3u8").unwrap()));
        assert_eq!(v[1].1.as_str(), "https://cdn.x.com/v/high/index.m3u8");
    }

    #[test]
    fn parses_media_with_keys() {
        let text = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0x01\n#EXTINF:4,\na.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\nhttps://other.com/b.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media { segments, init } = parse_playlist(text, &base()).unwrap() else { panic!() };
        assert!(init.is_none());
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].sequence, 7);
        let key = segments[0].key.as_ref().unwrap();
        assert_eq!(key.uri.as_str(), "https://cdn.x.com/v/k.bin");
        assert_eq!(key.iv.unwrap()[15], 1);
        assert!(segments[1].key.is_none());
        assert_eq!(segments[1].url.as_str(), "https://other.com/b.ts");
    }

    #[test]
    fn rejects_live_and_drm() {
        assert!(parse_playlist("#EXTM3U\n#EXTINF:4,\na.ts\n", &base()).is_err());
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:4,\na.ts\n#EXT-X-ENDLIST\n";
        assert!(parse_playlist(drm, &base()).is_err());
    }

    #[test]
    fn reads_attributes() {
        let list = "METHOD=AES-128,URI=\"https://k.com/a,b\",IV=0xff";
        assert_eq!(attr(list, "URI").as_deref(), Some("https://k.com/a,b"));
        assert_eq!(attr(list, "iv").as_deref(), Some("0xff"));
        assert_eq!(attr(list, "KEYFORMAT"), None);
    }
}
