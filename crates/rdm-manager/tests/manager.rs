//! Queue behaviour end to end against a local range-capable server.

use std::path::Path;
use std::sync::Arc;
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
