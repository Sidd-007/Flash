//! Queue behaviour end to end against a local range-capable server.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rdm_manager::{AddRequest, Event, Manager, Settings, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serves `data` at any path, honouring `Range`, `delay` per 16 KiB sent.
async fn serve(data: Arc<Vec<u8>>, delay: Duration) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let data = data.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = sock.read(&mut buf).await else { return };
                    if n == 0 {
                        return;
                    }
                    head.extend_from_slice(&buf[..n]);
                }
                let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
                let len = data.len();
                let range = head.lines().find_map(|l| {
                    let (a, b) = l.strip_prefix("range: bytes=")?.trim().split_once('-')?;
                    Some((a.parse::<usize>().ok()?, b.parse::<usize>().unwrap_or(len - 1).min(len - 1)))
                });
                let (status, start, end) = match range {
                    Some((s, e)) => ("206 Partial Content", s, e),
                    None => ("200 OK", 0, len - 1),
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Range: bytes {start}-{end}/{len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    end - start + 1
                );
                if sock.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
                for chunk in data[start..=end].chunks(16 * 1024) {
                    if sock.write_all(chunk).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(delay).await;
                }
            });
        }
    });
    base
}

fn data(len: usize) -> Arc<Vec<u8>> {
    Arc::new((0..len).map(|i| (i * 31 % 251) as u8).collect())
}

fn manager(dir: &Path, f: impl FnOnce(&mut Settings)) -> Manager {
    let mut s = Settings { download_dir: dir.into(), ..Default::default() };
    f(&mut s);
    Manager::in_memory(s).unwrap()
}

async fn wait_for(m: &Manager, id: i64, status: Status) {
    for _ in 0..200 {
        if m.get(id).unwrap().is_some_and(|d| d.download.status == status) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("download {id} never became {status:?}: {:?}", m.get(id).unwrap().map(|d| d.download));
}

fn add(m: &Manager, url: String) -> i64 {
    m.add(AddRequest { urls: vec![url], start: true, ..Default::default() }).unwrap()[0]
}

#[tokio::test]
async fn downloads_and_sorts_into_category_folder() {
    let body = data(3 << 20);
    let base = serve(body.clone(), Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |s| s.use_categories = true);
    let mut events = m.subscribe();

    let id = add(&m, format!("{base}/stuff/backup.zip"));
    wait_for(&m, id, Status::Completed).await;

    let d = m.get(id).unwrap().unwrap().download;
    let path = d.path.unwrap();
    assert_eq!(path, dir.path().join("Archives/backup.zip"));
    assert_eq!(std::fs::read(&path).unwrap(), *body);
    assert_eq!(d.downloaded, body.len() as u64);

    let mut finished = false;
    while let Ok(e) = events.try_recv() {
        finished |= matches!(e, Event::Finished { ok: true, .. });
    }
    assert!(finished, "a Finished event is sent");

    // Stats survive removing the download from the list.
    tokio::time::sleep(Duration::from_millis(700)).await;
    m.remove(id, false).unwrap();
    let stats = m.stats().unwrap();
    assert_eq!(stats.lifetime_bytes, body.len() as u64, "every byte counted exactly once");
    assert_eq!(stats.today_bytes, body.len() as u64);
    assert_eq!((stats.completed, stats.failed), (1, 0));
    assert_eq!(stats.categories[0].name, "Archives");
    assert_eq!(stats.hosts[0].name, "127.0.0.1");
    assert_eq!(stats.largest.unwrap().name, "backup.zip");
    assert!(stats.avg_speed > 0.0);
}

#[tokio::test]
async fn queue_runs_max_active_at_a_time() {
    let base = serve(data(2 << 20), Duration::from_millis(2)).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |s| s.max_active = 1);

    let ids = m
        .add(AddRequest { urls: vec![format!("{base}/f[1-3].bin")], start: true, ..Default::default() })
        .unwrap();
    assert_eq!(ids.len(), 3, "batch pattern expands");

    tokio::time::sleep(Duration::from_millis(300)).await;
    let statuses: Vec<Status> = ids.iter().map(|&id| m.get(id).unwrap().unwrap().download.status).collect();
    assert_eq!(statuses.iter().filter(|s| **s == Status::Running).count(), 1, "{statuses:?}");

    for id in ids {
        wait_for(&m, id, Status::Completed).await;
    }
    for n in 1..=3 {
        assert!(dir.path().join(format!("f{n}.bin")).exists());
    }
}

#[tokio::test]
async fn pause_resume_and_remove() {
    // 16 MiB over 8 connections at ~1.6 MB/s each takes over a second.
    let body = data(16 << 20);
    let base = serve(body.clone(), Duration::from_millis(10)).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});

    let id = add(&m, format!("{base}/big.iso"));
    wait_for(&m, id, Status::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    m.pause(id).unwrap();
    wait_for(&m, id, Status::Paused).await;
    assert!(dir.path().join("big.iso.part").exists());
    assert!(dir.path().join("big.iso.rdm.json").exists());
    let paused_at = m.get(id).unwrap().unwrap().download.downloaded;
    assert!(paused_at > 0);

    m.resume(id).unwrap();
    wait_for(&m, id, Status::Completed).await;
    assert_eq!(std::fs::read(dir.path().join("big.iso")).unwrap(), *body);
    // Pausing and resuming must not count any byte twice (or skip any).
    // Traffic is saved on the manager's half-second tick.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(m.stats().unwrap().lifetime_bytes, body.len() as u64);

    // Removing a running download deletes its partial data.
    let id2 = add(&m, format!("{base}/other.iso"));
    wait_for(&m, id2, Status::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    m.remove(id2, false).unwrap();
    for _ in 0..100 {
        if m.get(id2).unwrap().is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(m.get(id2).unwrap().is_none());
    assert!(!dir.path().join("other.iso.part").exists());
    assert!(!dir.path().join("other.iso.rdm.json").exists());

    // Removing a finished one keeps the file unless asked.
    m.remove(id, false).unwrap();
    assert!(dir.path().join("big.iso").exists());
}

#[tokio::test]
async fn rejects_bad_input() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});
    assert!(m.add(AddRequest { urls: vec!["ftp://x.com/a".into()], ..Default::default() }).is_err());
    assert!(m.add(AddRequest { urls: vec![], ..Default::default() }).is_err());
    let bad_sum = AddRequest { urls: vec!["https://x.com/a".into()], checksum: Some("nope".into()), ..Default::default() };
    assert!(m.add(bad_sum).is_err());
}

#[tokio::test]
async fn network_failures_retry_but_missing_files_fail() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});

    // Nothing listens on this port yet: connection refused is a network problem.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let id = add(&m, format!("http://127.0.0.1:{port}/later.bin"));
    let mut d = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let v = m.get(id).unwrap().unwrap().download;
        if v.auto_retries > 0 {
            d = Some(v);
            break;
        }
    }
    let d = d.expect("a retry is scheduled");
    assert_eq!(d.status, Status::Queued);
    assert!(d.retry_at.unwrap() > chrono::Utc::now().timestamp_millis(), "waits before retrying");
    assert!(d.error.as_deref().unwrap().starts_with("network problem"), "{:?}", d.error);

    // The server comes back; resuming by hand skips the wait.
    let body = data(1 << 20);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let base = serve_on(listener, body.clone()).await;
    assert!(base.ends_with(&port.to_string()));
    m.resume(id).unwrap();
    wait_for(&m, id, Status::Completed).await;
    let d = m.get(id).unwrap().unwrap().download;
    assert_eq!((d.auto_retries, d.retry_at), (0, None), "retries reset after success");

    // A 404 won't fix itself: it fails straight away.
    let missing = add(&m, format!("{base}/missing"));
    wait_for(&m, missing, Status::Failed).await;
    assert_eq!(m.get(missing).unwrap().unwrap().download.auto_retries, 0);
}

/// Like `serve`, on a given listener; `/missing` answers 404.
async fn serve_on(listener: TcpListener, data: Arc<Vec<u8>>) -> String {
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let data = data.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if head.starts_with("get /missing") {
                    let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                }
                let len = data.len();
                let (status, start, end) = match head.lines().find_map(|l| l.strip_prefix("range: bytes=")) {
                    Some(r) => {
                        let (a, b) = r.trim().split_once('-').unwrap();
                        ("206 Partial Content", a.parse().unwrap(), b.parse().unwrap_or(len - 1).min(len - 1))
                    }
                    None => ("200 OK", 0usize, len - 1),
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Range: bytes {start}-{end}/{len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    end - start + 1
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.write_all(&data[start..=end]).await;
            });
        }
    });
    base
}

#[tokio::test]
async fn relink_continues_from_a_fresh_link() {
    let body = data(16 << 20);
    let old = serve(body.clone(), Duration::from_millis(10)).await;
    let fresh = serve(body.clone(), Duration::ZERO).await;
    let other = serve(data(5 << 20), Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});

    let id = add(&m, format!("{old}/files/big.iso?sig=expired"));
    wait_for(&m, id, Status::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(m.relink(id, &format!("{fresh}/x"), vec![]).await.is_err(), "not while running");
    m.pause(id).unwrap();
    wait_for(&m, id, Status::Paused).await;

    // A link to a different file is refused and changes nothing.
    let err = m.relink(id, &format!("{other}/big.iso"), vec![]).await.unwrap_err();
    assert!(format!("{err:#}").contains("different file"), "{err:#}");
    assert_eq!(m.get(id).unwrap().unwrap().download.status, Status::Paused);

    // The fresh link continues into the same file, under the same name.
    let url = format!("{fresh}/elsewhere/download?sig=new");
    m.relink(id, &url, vec![]).await.unwrap();
    wait_for(&m, id, Status::Completed).await;
    let d = m.get(id).unwrap().unwrap().download;
    assert_eq!(d.url, url);
    assert_eq!(std::fs::read(dir.path().join("big.iso")).unwrap(), *body);
    assert!(!dir.path().join("big (1).iso").exists());
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(m.stats().unwrap().lifetime_bytes, body.len() as u64, "kept bytes aren't downloaded again");
}

#[tokio::test]
async fn finds_duplicates_by_url_or_name_and_size() {
    let body = data(1 << 20);
    let base = serve(body.clone(), Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});
    let url = format!("{base}/pkg/tool.dmg");
    let id = add(&m, url.clone());
    wait_for(&m, id, Status::Completed).await;
    let size = Some(body.len() as u64);

    let same = m.duplicates(&url, None, None).unwrap();
    assert_eq!((same.len(), same[0].id, same[0].same_url), (1, id, true));
    // Another mirror of the same file: same name and size.
    let mirror = m.duplicates("https://mirror.example/tool.dmg", None, size).unwrap();
    assert_eq!((mirror.len(), mirror[0].same_url), (1, false));
    // Same name, different size: a different file.
    assert!(m.duplicates("https://mirror.example/tool.dmg", None, Some(5)).unwrap().is_empty());
    assert!(m.duplicates("https://mirror.example/other.dmg", Some("TOOL.dmg"), size).unwrap().len() == 1);

    // A finished download whose file is gone doesn't count.
    std::fs::remove_file(dir.path().join("tool.dmg")).unwrap();
    assert!(m.duplicates(&url, None, None).unwrap().is_empty());
}

#[tokio::test]
async fn forgets_finished_files_deleted_outside_flash() {
    let base = serve(data(64 * 1024), Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});
    let kept = add(&m, format!("{base}/kept.bin"));
    let deleted = add(&m, format!("{base}/deleted.bin"));
    wait_for(&m, kept, Status::Completed).await;
    wait_for(&m, deleted, Status::Completed).await;

    std::fs::remove_file(dir.path().join("deleted.bin")).unwrap();
    assert_eq!(m.forget_missing().unwrap(), vec![deleted]);
    assert!(m.get(deleted).unwrap().is_none());
    assert!(m.get(kept).unwrap().is_some());

    // A whole folder gone (an unplugged drive) isn't treated as deleted.
    let other = tempfile::tempdir().unwrap();
    let id = m.add(AddRequest { urls: vec![format!("{base}/far.bin")], dir: Some(other.path().join("drive")), start: true, ..Default::default() }).unwrap()[0];
    wait_for(&m, id, Status::Completed).await;
    std::fs::remove_dir_all(other.path().join("drive")).unwrap();
    assert!(m.forget_missing().unwrap().is_empty());
    assert!(m.get(id).unwrap().is_some());
}

#[tokio::test]
async fn clear_all_keeps_finished_files_and_drops_partial_data() {
    let body = data(16 << 20);
    let base = serve(body.clone(), Duration::from_millis(10)).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |s| s.max_active = 2);
    let small = serve(data(64 * 1024), Duration::ZERO).await;
    let done = add(&m, format!("{small}/done.bin"));
    wait_for(&m, done, Status::Completed).await;
    let paused = add(&m, format!("{base}/paused.iso"));
    wait_for(&m, paused, Status::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    m.pause(paused).unwrap();
    wait_for(&m, paused, Status::Paused).await;
    let running = add(&m, format!("{base}/running.iso"));
    wait_for(&m, running, Status::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    m.clear_all().unwrap();
    for _ in 0..100 {
        if m.list().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(m.list().unwrap().is_empty());
    assert!(dir.path().join("done.bin").exists(), "finished files stay on disk");
    for name in ["paused.iso", "running.iso"] {
        assert!(!dir.path().join(format!("{name}.part")).exists(), "{name}.part is deleted");
        assert!(!dir.path().join(format!("{name}.rdm.json")).exists());
    }
}

/// Answers everything with this status line and extra headers.
async fn serve_status(status: &'static str, headers: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let reply = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: 0\r\n{headers}\r\n");
                let _ = sock.write_all(reply.as_bytes()).await;
            });
        }
    });
    base
}

async fn first_retry(m: &Manager, id: i64) -> rdm_manager::Download {
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let d = m.get(id).unwrap().unwrap().download;
        if d.auto_retries > 0 {
            return d;
        }
    }
    panic!("no retry scheduled: {:?}", m.get(id).unwrap().unwrap().download);
}

#[tokio::test]
async fn a_busy_host_is_retried_soon_or_when_it_asks() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});
    let now = || chrono::Utc::now().timestamp_millis();

    let base = serve_status("503 Service Unavailable", "").await;
    let d = first_retry(&m, add(&m, format!("{base}/busy.iso"))).await;
    assert_eq!(d.status, Status::Queued);
    assert!(d.retry_at.unwrap() - now() <= 3_000, "the first wait is short");

    let base = serve_status("429 Too Many Requests", "Retry-After: 45\r\n").await;
    let d = first_retry(&m, add(&m, format!("{base}/slow-down.iso"))).await;
    let wait = d.retry_at.unwrap() - now();
    assert!(wait > 40_000 && wait <= 45_000, "waits as asked: {wait} ms");
    assert!(d.error.as_deref().unwrap().contains("retrying in 45s"), "{:?}", d.error);
}

/// A stand-in for Real-Debrid: its API, plus its download servers at
/// `/d/<anything>`, which refuse with 410 while `expired` is set for that link.
struct FakeDebrid {
    base: String,
    /// What `/unrestrict/link` answers: status line and JSON. `{base}` is filled in.
    answer: Mutex<(String, String)>,
    /// Every request's head, lowercased, with the body of POSTs.
    log: Mutex<Vec<String>>,
    data: Arc<Vec<u8>>,
    delay: Mutex<Duration>,
    expired: Mutex<Vec<String>>,
}

impl FakeDebrid {
    async fn start(data: Arc<Vec<u8>>) -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake = Arc::new(Self {
            base: format!("http://{}", listener.local_addr().unwrap()),
            answer: Mutex::new(("200 OK".into(), String::new())),
            log: Mutex::default(),
            data,
            delay: Mutex::new(Duration::ZERO),
            expired: Mutex::default(),
        });
        fake.link_is("first");
        let server = fake.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                let server = server.clone();
                tokio::spawn(async move { server.handle(sock).await });
            }
        });
        fake
    }

    /// Unlocking answers with a direct link at `/d/<name>`.
    fn link_is(&self, name: &str) {
        let json = format!(
            r#"{{"id":"1","filename":"Real Name.mp4","filesize":{},"link":"https://rapidgator.net/x","host":"rapidgator.net","chunks":3,"download":"{}/d/{name}"}}"#,
            self.data.len(),
            self.base,
        );
        *self.answer.lock().unwrap() = ("200 OK".into(), json);
    }

    fn refuse(&self, status: &str, code: i64, message: &str) {
        *self.answer.lock().unwrap() = (status.into(), format!(r#"{{"error":"{message}","error_code":{code}}}"#));
    }

    fn requests_to(&self, path: &str) -> Vec<String> {
        let path = path.to_ascii_lowercase();
        self.log.lock().unwrap().iter().filter(|r| r.split_whitespace().nth(1).is_some_and(|p| p.starts_with(&path))).cloned().collect()
    }

    async fn handle(&self, mut sock: tokio::net::TcpStream) {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let Ok(n) = sock.read(&mut buf).await else { return };
            if n == 0 {
                return;
            }
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
            let Some(end) = text.find("\r\n\r\n") else { continue };
            let want = text.lines().find_map(|l| l.strip_prefix("content-length: ")?.trim().parse::<usize>().ok()).unwrap_or(0);
            if raw.len() >= end + 4 + want {
                break;
            }
        }
        let text = String::from_utf8_lossy(&raw).to_ascii_lowercase();
        self.log.lock().unwrap().push(text.clone());
        let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
        let json = |status: &str, body: &str| format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let reply = if path.ends_with("/unrestrict/link") {
            let (status, body) = self.answer.lock().unwrap().clone();
            json(&status, &body)
        } else if path.ends_with("/hosts/domains") {
            json("200 OK", r#"["rapidgator.net","127.0.0.1"]"#)
        } else if path.ends_with("/user") {
            json("200 OK", r#"{"id":1,"username":"someone","type":"premium","premium":100,"expiration":"2027-01-18T04:42:42.000Z"}"#)
        } else if self.expired.lock().unwrap().iter().any(|l| path.ends_with(l.as_str())) {
            json("410 Gone", "")
        } else {
            return self.send_file(sock, &text).await;
        };
        let _ = sock.write_all(reply.as_bytes()).await;
    }

    async fn send_file(&self, mut sock: tokio::net::TcpStream, head: &str) {
        let len = self.data.len();
        let (start, end) = head
            .lines()
            .find_map(|l| {
                let (a, b) = l.strip_prefix("range: bytes=")?.trim().split_once('-')?;
                Some((a.parse().ok()?, b.parse::<usize>().unwrap_or(len - 1).min(len - 1)))
            })
            .unwrap_or((0, len - 1));
        let reply = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{len}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            end - start + 1
        );
        if sock.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        let delay = *self.delay.lock().unwrap();
        for chunk in self.data[start..=end].chunks(16 * 1024) {
            if sock.write_all(chunk).await.is_err() {
                return;
            }
            tokio::time::sleep(delay).await;
        }
    }
}

fn debrid_manager(dir: &Path, fake: &FakeDebrid) -> Manager {
    let m = manager(dir, |s| s.realdebrid_token = "secret-token".into());
    m.set_debrid_api(&format!("{}/rest/1.0", fake.base));
    m
}

/// A browser hand-off: the host's own page, with its cookies and address.
fn add_host_link(m: &Manager) -> i64 {
    m.add(AddRequest {
        urls: vec!["https://rapidgator.net/file/abc/Real%20Name.mp4.html".into()],
        file_name: Some("Real Name.mp4.html".into()),
        headers: vec![("Cookie".into(), "host-session=abc".into()), ("Referer".into(), "https://rapidgator.net/page".into())],
        start: true,
        ..Default::default()
    })
    .unwrap()[0]
}

#[tokio::test]
async fn host_links_download_through_real_debrid() {
    let body = data(2 << 20);
    let fake = FakeDebrid::start(body.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let m = debrid_manager(dir.path(), &fake);

    let id = add_host_link(&m);
    wait_for(&m, id, Status::Completed).await;
    let d = m.get(id).unwrap().unwrap().download;
    assert_eq!(std::fs::read(dir.path().join("Real Name.mp4")).unwrap(), *body, "named as Real-Debrid says, not the page");
    assert_eq!(d.url, "https://rapidgator.net/file/abc/Real%20Name.mp4.html", "the list keeps the original link");

    let api = fake.requests_to("/rest/1.0/unrestrict/link");
    assert_eq!(api.len(), 1);
    assert!(api[0].contains("authorization: bearer secret-token"));
    assert!(api[0].contains("link=https%3a%2f%2frapidgator.net%2ffile%2fabc"), "{}", api[0]);
    let files = fake.requests_to("/d/first");
    assert!(!files.is_empty());
    for request in files {
        assert!(!request.contains("secret-token"), "the token only goes to the API");
        assert!(!request.contains("host-session") && !request.contains("referer"), "the host's cookies stay with the host:\n{request}");
    }
}

#[tokio::test]
async fn a_name_the_user_chose_is_kept() {
    let fake = FakeDebrid::start(data(1 << 20)).await;
    let dir = tempfile::tempdir().unwrap();
    let m = debrid_manager(dir.path(), &fake);
    let id = m.add(AddRequest { urls: vec!["https://rg.to/file/abc".into()], file_name: Some("Mine.mp4".into()), start: true, ..Default::default() }).unwrap()[0];
    wait_for(&m, id, Status::Completed).await;
    assert!(dir.path().join("Mine.mp4").exists());
}

#[tokio::test]
async fn an_expired_link_is_replaced_and_the_download_carries_on() {
    let body = data(16 << 20);
    let fake = FakeDebrid::start(body.clone()).await;
    *fake.delay.lock().unwrap() = Duration::from_millis(10);
    let dir = tempfile::tempdir().unwrap();
    let m = debrid_manager(dir.path(), &fake);
    let id = add_host_link(&m);
    wait_for(&m, id, Status::Running).await;
    for _ in 0..200 {
        if m.get(id).unwrap().unwrap().download.downloaded > 1 << 20 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Pausing drops the connections; the link has expired by the time it resumes.
    m.pause(id).unwrap();
    wait_for(&m, id, Status::Paused).await;
    let kept = m.get(id).unwrap().unwrap().download.downloaded;
    assert!(kept > 0);
    fake.expired.lock().unwrap().push("/d/first".into());
    fake.link_is("second");
    *fake.delay.lock().unwrap() = Duration::ZERO;
    m.resume(id).unwrap();
    wait_for(&m, id, Status::Completed).await;

    assert_eq!(std::fs::read(dir.path().join("Real Name.mp4")).unwrap(), *body);
    assert!(!dir.path().join("Real Name (1).mp4").exists(), "continued the same file");
    assert!(!fake.requests_to("/d/second").is_empty());
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(m.stats().unwrap().lifetime_bytes, body.len() as u64, "what it had wasn't downloaded again");
}

#[tokio::test]
async fn a_link_expiring_mid_download_gets_a_new_one_by_itself() {
    let body = data(16 << 20);
    let fake = FakeDebrid::start(body.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let m = debrid_manager(dir.path(), &fake);
    fake.expired.lock().unwrap().push("/d/first".into());
    // The check of the file also goes to the expired link, as if it had just lapsed.
    let id = add_host_link(&m);
    let d = first_retry(&m, id).await;
    assert_eq!(d.status, Status::Queued, "a refused Real-Debrid link is retried, not failed: {:?}", d.error);
    fake.link_is("second");
    wait_for_long(&m, id, Status::Completed).await;
    assert_eq!(std::fs::read(dir.path().join("Real Name.mp4")).unwrap(), *body);
}

async fn wait_for_long(m: &Manager, id: i64, status: Status) {
    for _ in 0..30 {
        if m.get(id).unwrap().is_some_and(|d| d.download.status == status) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("download {id} never became {status:?}: {:?}", m.get(id).unwrap().map(|d| d.download));
}

#[tokio::test]
async fn what_real_debrid_says_decides_what_happens_next() {
    let dir = tempfile::tempdir().unwrap();

    // A dead file or a bad token won't fix themselves, and say why.
    let fake = FakeDebrid::start(data(1 << 20)).await;
    let m = debrid_manager(dir.path(), &fake);
    fake.refuse("503 Service Unavailable", 24, "unavailable_file");
    let id = add_host_link(&m);
    wait_for(&m, id, Status::Failed).await;
    let d = m.get(id).unwrap().unwrap().download;
    assert!(d.error.as_deref().unwrap().contains("no longer available"), "{:?}", d.error);
    assert_eq!(d.auto_retries, 0);

    fake.refuse("401 Unauthorized", 8, "bad_token");
    let id = add_host_link(&m);
    wait_for(&m, id, Status::Failed).await;
    assert!(m.get(id).unwrap().unwrap().download.error.unwrap().contains("Settings"));

    // A busy host is tried again later.
    fake.refuse("503 Service Unavailable", 19, "hoster_unavailable");
    let id = add_host_link(&m);
    assert_eq!(first_retry(&m, id).await.status, Status::Queued);
}

#[tokio::test]
async fn links_real_debrid_cannot_handle_download_directly() {
    let body = data(1 << 20);
    let fake = FakeDebrid::start(body.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let m = debrid_manager(dir.path(), &fake);
    m.refresh_debrid_hosts().await.unwrap();
    fake.refuse("403 Forbidden", 16, "hoster_unsupported");

    let id = add(&m, format!("{}/direct/plain.bin", fake.base));
    wait_for(&m, id, Status::Completed).await;
    assert_eq!(std::fs::read(dir.path().join("plain.bin")).unwrap(), *body);
    assert_eq!(fake.requests_to("/rest/1.0/unrestrict/link").len(), 1, "it was asked first");
}

#[tokio::test]
async fn without_a_token_nothing_goes_to_real_debrid() {
    let body = data(1 << 20);
    let fake = FakeDebrid::start(body.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});
    m.set_debrid_api(&format!("{}/rest/1.0", fake.base));
    m.refresh_debrid_hosts().await.unwrap();

    let id = add(&m, format!("{}/direct/plain.bin", fake.base));
    wait_for(&m, id, Status::Completed).await;
    assert!(fake.requests_to("/rest/1.0/unrestrict").is_empty());
    assert!(!m.is_download_link("https://rapidgator.net/file/abc/movie.zip.html"));

    // With one, a copied host link is worth offering, but a general site isn't.
    let dir = tempfile::tempdir().unwrap();
    let m = debrid_manager(dir.path(), &fake);
    assert!(m.is_download_link("https://rapidgator.net/file/abc/movie.mp4.html"));
    assert!(!m.is_download_link("https://rapidgator.net/"));
    assert!(!m.is_download_link("https://drive.google.com/file/d/1/view"));

    let account = m.debrid_account("secret-token").await.unwrap();
    assert_eq!((account.username.as_str(), account.kind.as_str()), ("someone", "premium"));
    assert_eq!(account.expiration.as_deref(), Some("2027-01-18T04:42:42.000Z"));
}

/// The app's Save button runs on the UI thread, outside any async runtime.
#[tokio::test]
async fn settings_can_be_saved_from_outside_the_runtime() {
    let fake = FakeDebrid::start(data(1 << 20)).await;
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), |_| {});
    m.set_debrid_api(&format!("{}/rest/1.0", fake.base));

    let saver = m.clone();
    let saved = std::thread::spawn(move || {
        let mut settings = saver.settings();
        settings.realdebrid_token = "new-token".into();
        saver.update_settings(settings).map(|s| s.realdebrid_token)
    })
    .join()
    .expect("saving must not panic: a panic on the UI thread aborts the app");
    assert_eq!(saved.unwrap(), "new-token");
    assert_eq!(m.settings().realdebrid_token, "new-token");

    // The new token's arrival still loads the live list of hosts.
    for _ in 0..100 {
        if !fake.requests_to("/rest/1.0/hosts/domains").is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the host list was never loaded");
}
