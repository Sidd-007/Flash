//! Video downloads end to end, against a stand-in yt-dlp script.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rdm_core::{Outcome, VideoJob, VideoRequest};
use reqwest::header::HeaderMap;
use tokio_util::sync::CancellationToken;

/// Behaves like yt-dlp for the flags RDM uses: `-J` prints details; a
/// download prints progress lines, writes the file, prints its path.
const FAKE_YTDLP: &str = r#"#!/bin/sh
url=""; out=""; prev=""
for a in "$@"; do
  [ "$prev" = "-o" ] && out="$a"
  prev="$a"; url="$a"
done
case "$url" in *broken*) echo "ERROR: [youtube] abc: Video unavailable" >&2; exit 1;; esac
for a in "$@"; do
  if [ "$a" = "-J" ]; then
    echo '{"id":"abc","title":"My: Clip","ext":"webm","filesize_approx":3000}'
    exit 0
  fi
done
file=$(echo "$out" | sed 's/%(ext)s/mp4/')
echo "[youtube] Extracting URL: $url"
echo "RDM|1000|2000|NA|137"
echo "RDM|2000|2000|NA|137"
echo "RDM|500|NA|1000|140"
echo "RDM|1000|1000|NA|140"
printf 'video-bytes' > "$file"
echo "RDMFILE|$file"
"#;

fn fake_ytdlp(dir: &Path) -> PathBuf {
    let path = dir.join("yt-dlp");
    std::fs::write(&path, FAKE_YTDLP).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn request(url: &str, dir: &Path, ytdlp: PathBuf) -> VideoRequest {
    VideoRequest {
        url: url.into(),
        dir: dir.into(),
        file_name: None,
        quality: Some("720".into()),
        connections: 4,
        headers: HeaderMap::new(),
        rate_limit: 0,
        ytdlp: Some(ytdlp),
    }
}

#[tokio::test]
async fn downloads_with_ytdlp_and_tracks_progress() {
    let tools = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let job = VideoJob::prepare(request("https://www.youtube.com/watch?v=abc", out.path(), fake_ytdlp(tools.path())))
        .await
        .unwrap();
    // The title becomes a safe file name.
    assert!(job.info().file_name.starts_with("My_ Clip."), "{}", job.info().file_name);
    assert_eq!(job.info().total, Some(3000));

    let progress = job.progress();
    let Outcome::Completed(path) = job.run(CancellationToken::new()).await.unwrap() else {
        panic!("expected completion");
    };
    assert_eq!(path, out.path().join("My_ Clip.mp4"));
    assert_eq!(std::fs::read(&path).unwrap(), b"video-bytes");
    // Video stream (2000) then audio stream (1000).
    assert_eq!(progress.downloaded(), 3000);
    assert_eq!(progress.total(), Some(3000));

    // A second download of the same video doesn't overwrite the first.
    let again = VideoJob::prepare(request("https://www.youtube.com/watch?v=abc", out.path(), fake_ytdlp(tools.path())))
        .await
        .unwrap();
    assert!(again.info().file_name.starts_with("My_ Clip (1)."), "{}", again.info().file_name);
}

#[tokio::test]
async fn reports_ytdlp_errors() {
    let tools = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let err = VideoJob::prepare(request("https://youtu.be/broken", out.path(), fake_ytdlp(tools.path())))
        .await
        .err()
        .expect("should fail");
    assert!(format!("{err:#}").contains("Video unavailable"), "{err:#}");
}
