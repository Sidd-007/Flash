//! End-to-end tests against a tiny local HTTP/1.1 server.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rdm_core::{Job, Outcome, Request};
use reqwest::header::HeaderMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const MIB: usize = 1 << 20;
const SEND_CHUNK: usize = 16 * 1024;

#[derive(Default)]
struct Config {
    data: Vec<u8>,
    ranges: bool,
    /// Omit Content-Length (only meaningful without ranges).
    unknown_length: bool,
    /// Pause after each 16 KiB sent.
    delay: Option<Duration>,
    /// Ranged requests that get cut off halfway before the server behaves.
    failures: AtomicUsize,
}

#[derive(Default)]
struct Stats {
    active: AtomicUsize,
    max_active: AtomicUsize,
    bytes_sent: AtomicUsize,
}

struct Server {
    url: String,
    stats: Arc<Stats>,
}

async fn start(config: Config) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/files/file.bin", listener.local_addr().unwrap());
    let (config, stats) = (Arc::new(config), Arc::new(Stats::default()));
    tokio::spawn({
        let stats = stats.clone();
        async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let (config, stats) = (config.clone(), stats.clone());
                tokio::spawn(async move {
                    let now = stats.active.fetch_add(1, Ordering::SeqCst) + 1;
                    stats.max_active.fetch_max(now, Ordering::SeqCst);
                    let _ = handle(sock, &config, &stats).await;
                    stats.active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        }
    });
    Server { url, stats }
}

async fn handle(mut sock: TcpStream, config: &Config, stats: &Stats) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(&buf[..n]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    let range = head.lines().find_map(|l| {
        let (a, b) = l.strip_prefix("range: bytes=")?.trim().split_once('-')?;
        Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()))
    });

    let len = config.data.len();
    let mut cut_short = false;
    let (status, body, extra) = match range {
        Some((start, _)) if config.ranges && start >= len => {
            ("416 Range Not Satisfiable", &[][..], format!("Content-Range: bytes */{len}\r\n"))
        }
        Some((start, end)) if config.ranges => {
            let end = end.unwrap_or(len - 1).min(len - 1);
            // Never break the 1-byte probe.
            if end > start {
                cut_short = config
                    .failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |f| f.checked_sub(1))
                    .is_ok();
            }
            let body = &config.data[start..=end];
            (
                "206 Partial Content",
                body,
                format!("Content-Range: bytes {start}-{end}/{len}\r\nContent-Length: {}\r\n", body.len()),
            )
        }
        _ if config.unknown_length => ("200 OK", &config.data[..], String::new()),
        _ => ("200 OK", &config.data[..], format!("Content-Length: {len}\r\n")),
    };

    sock.write_all(
        format!("HTTP/1.1 {status}\r\nConnection: close\r\nETag: \"v1\"\r\n{extra}\r\n").as_bytes(),
    )
    .await?;
    let body = if cut_short { &body[..body.len() / 2] } else { body };
    for chunk in body.chunks(SEND_CHUNK) {
        sock.write_all(chunk).await?;
        stats.bytes_sent.fetch_add(chunk.len(), Ordering::SeqCst);
        if let Some(d) = config.delay {
            tokio::time::sleep(d).await;
        }
    }
    sock.shutdown().await
}

/// Deterministic, non-repeating-looking bytes so misplaced chunks are caught.
fn test_data(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x1234_5678;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

fn request(url: &str, dir: &Path, connections: usize) -> Request {
    Request {
        url: url.into(),
        dir: dir.into(),
        file_name: None,
        connections,
        headers: HeaderMap::new(),
        limiters: Vec::new(),
        checksum: None,
    }
}

fn leftovers(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".part") || n.contains(".rdm.json"))
        .collect()
}

#[tokio::test]
async fn parallel_download_is_byte_identical() {
    let data = test_data(5 * MIB + 123);
    let server = start(Config {
        data: data.clone(),
        ranges: true,
        delay: Some(Duration::from_millis(1)),
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();

    let job = Job::prepare(rdm_core::default_client().unwrap(), request(&server.url, dir.path(), 8))
        .await
        .unwrap();
    assert_eq!(job.info().file_name, "file.bin");
    assert_eq!(job.info().total, Some(data.len() as u64));
    assert!(job.info().resumable);

    let Outcome::Completed(path) = job.run(CancellationToken::new()).await.unwrap() else {
        panic!("expected completion");
    };
    assert_eq!(std::fs::read(&path).unwrap(), data);
    assert!(leftovers(dir.path()).is_empty());
    assert!(server.stats.max_active.load(Ordering::SeqCst) > 1, "should use several connections");
}

#[tokio::test]
async fn falls_back_to_one_stream_without_ranges() {
    let data = test_data(2 * MIB);
    let server = start(Config {
        data: data.clone(),
        unknown_length: true,
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();

    let job = Job::prepare(rdm_core::default_client().unwrap(), request(&server.url, dir.path(), 8))
        .await
        .unwrap();
    assert!(!job.info().resumable);
    assert_eq!(job.info().connections, 1);
    assert_eq!(job.info().total, None);

    let Outcome::Completed(path) = job.run(CancellationToken::new()).await.unwrap() else {
        panic!("expected completion");
    };
    assert_eq!(std::fs::read(path).unwrap(), data);
}

#[tokio::test]
async fn pause_then_resume_continues_where_it_stopped() {
    let data = test_data(8 * MIB);
    let server = start(Config {
        data: data.clone(),
        ranges: true,
        delay: Some(Duration::from_millis(10)),
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();
    let client = rdm_core::default_client().unwrap();

    let job = Job::prepare(client.clone(), request(&server.url, dir.path(), 4)).await.unwrap();
    let cancel = CancellationToken::new();
    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            cancel.cancel();
        }
    });
    let outcome = job.run(cancel).await.unwrap();
    assert!(matches!(outcome, Outcome::Paused { resumable: true }), "{outcome:?}");
    assert!(dir.path().join("file.bin.part").exists());
    assert!(dir.path().join("file.bin.rdm.json").exists());

    let job = Job::prepare(client, request(&server.url, dir.path(), 4)).await.unwrap();
    let resumed = job.info().resumed_from;
    assert!(resumed > 0 && resumed < data.len() as u64, "resumed_from = {resumed}");
    assert_eq!(job.info().file_name, "file.bin", "should reuse the unfinished file");

    let Outcome::Completed(path) = job.run(CancellationToken::new()).await.unwrap() else {
        panic!("expected completion");
    };
    assert_eq!(std::fs::read(path).unwrap(), data);
    assert!(leftovers(dir.path()).is_empty());
    // Resuming must not re-download what was already saved.
    let sent = server.stats.bytes_sent.load(Ordering::SeqCst);
    assert!(sent < data.len() + data.len() / 2, "server sent {sent} bytes for {}", data.len());
}

#[tokio::test]
async fn retries_dropped_connections() {
    let data = test_data(4 * MIB);
    let server = start(Config {
        data: data.clone(),
        ranges: true,
        failures: AtomicUsize::new(3),
        ..Default::default()
    })
    .await;
    let dir = tempfile::tempdir().unwrap();

    let job = Job::prepare(rdm_core::default_client().unwrap(), request(&server.url, dir.path(), 4))
        .await
        .unwrap();
    let Outcome::Completed(path) = job.run(CancellationToken::new()).await.unwrap() else {
        panic!("expected completion");
    };
    assert_eq!(std::fs::read(path).unwrap(), data);
}

#[tokio::test]
async fn never_overwrites_an_existing_file() {
    let data = test_data(MIB);
    let server = start(Config { data: data.clone(), ranges: true, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.bin"), b"keep me").unwrap();

    let job = Job::prepare(rdm_core::default_client().unwrap(), request(&server.url, dir.path(), 4))
        .await
        .unwrap();
    assert_eq!(job.info().file_name, "file (1).bin");
    job.run(CancellationToken::new()).await.unwrap();

    assert_eq!(std::fs::read(dir.path().join("file.bin")).unwrap(), b"keep me");
    assert_eq!(std::fs::read(dir.path().join("file (1).bin")).unwrap(), data);
}
