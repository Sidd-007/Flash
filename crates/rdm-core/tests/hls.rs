//! HLS end to end: master playlist -> variant -> AES-128 segments -> one file.

use std::collections::HashMap;
use std::sync::Arc;

use aes::Aes128;
use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
use rdm_core::{HlsJob, HlsRequest, Outcome};
use reqwest::header::HeaderMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

async fn serve(files: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let files = Arc::new(files);
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let files = files.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let (status, body) = match files.get(&path) {
                    Some(b) => ("200 OK", b.clone()),
                    None => ("404 Not Found", Vec::new()),
                };
                let reply = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = sock.write_all(reply.as_bytes()).await;
                let _ = sock.write_all(&body).await;
            });
        }
    });
    base
}

#[tokio::test]
async fn downloads_master_playlist_with_encrypted_segments() {
    let key = [7u8; 16];
    let plain: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 50_000 + i as usize]).collect();

    let mut files = HashMap::new();
    files.insert(
        "/master.m3u8".into(),
        b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100000\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900000\nhigh/index.m3u8\n".to_vec(),
    );
    files.insert("/low/index.m3u8".into(), b"#EXTM3U\n#EXTINF:1,\nnope.ts\n#EXT-X-ENDLIST\n".to_vec());
    let mut media = String::from("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"/key.bin\"\n");
    for (i, data) in plain.iter().enumerate() {
        // Default IV = media sequence number.
        let iv = (i as u128).to_be_bytes();
        let enc = cbc::Encryptor::<Aes128>::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(data);
        files.insert(format!("/high/seg{i}.ts"), enc);
        media.push_str(&format!("#EXTINF:2,\nseg{i}.ts\n"));
    }
    media.push_str("#EXT-X-ENDLIST\n");
    files.insert("/high/index.m3u8".into(), media.into_bytes());
    files.insert("/key.bin".into(), key.to_vec());

    let base = serve(files).await;
    let dir = tempfile::tempdir().unwrap();
    let job = HlsJob::prepare(rdm_core::default_client().unwrap(), HlsRequest {
        url: format!("{base}/master.m3u8"),
        dir: dir.path().into(),
        file_name: Some("movie".into()),
        connections: 3,
        headers: HeaderMap::new(),
        limiters: Vec::new(),
    })
    .await
    .unwrap();
    assert!(job.info().file_name.starts_with("movie."));
    assert_eq!(job.progress().parts(), Some((0, 5)));

    let Outcome::Completed(path) = job.run(CancellationToken::new()).await.unwrap() else {
        panic!("expected completion");
    };
    // Without ffmpeg the joined transport stream is kept as-is.
    if path.extension().is_some_and(|e| e == "ts") {
        assert_eq!(std::fs::read(&path).unwrap(), plain.concat());
    }
    assert!(!dir.path().join(format!("{}.hls-parts", job_name(&path))).exists());
}

fn job_name(path: &std::path::Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}
