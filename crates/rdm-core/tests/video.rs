//! Video downloads end to end, against a stand-in yt-dlp script.
// The stand-in is a shell script, so these run on macOS and Linux only.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rdm_core::{Outcome, VideoJob, VideoRequest};
use reqwest::header::{HeaderMap, HeaderValue};
use tokio_util::sync::CancellationToken;

/// Behaves like yt-dlp for the flags RDM uses: `-J` prints details; a
/// download prints progress lines, writes the file, prints its path.
const FAKE_YTDLP: &str = r#"#!/bin/sh
log="$(dirname "$0")/calls.log"
echo "ARGS $*" >> "$log"
prev=""
for a in "$@"; do
  if [ "$prev" = "--cookies" ]; then
    echo "COOKIEFILE $a" >> "$log"
    ls -ld "$a" "$(dirname "$a")" >> "$log"
    cat "$a" >> "$log"
  fi
  prev="$a"
done
url=""; out=""; prev=""
for a in "$@"; do
  [ "$prev" = "-o" ] && out="$a"
  prev="$a"; url="$a"
done
case "$url" in *broken*) echo "ERROR: [youtube] abc: Video unavailable" >&2; exit 1;; esac
for a in "$@"; do
  if [ "$a" = "-J" ]; then
    case "$url" in
      *scanned*) echo '{"_type":"playlist","extractor_key":"Generic","title":"Page","entries":[{"id":"p1","title":"Main player","ext":"mp4","filesize":2000},{"id":"p2","title":"Sidebar","ext":"mp4"}]}' ;;
      *list*) echo '{"_type":"playlist","extractor_key":"YoutubeTab","title":"Mix","entries":[{"id":"a","title":"A"}]}' ;;
      *) echo '{"id":"abc","title":"My: Clip","ext":"webm","filesize_approx":3000}' ;;
    esac
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

#[tokio::test]
async fn cookies_reach_ytdlp_only_through_a_private_scoped_file() {
    let tools = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut req = request("https://www.youtube.com/watch?v=abc", out.path(), fake_ytdlp(tools.path()));
    req.headers.insert("cookie", HeaderValue::from_static("SID=fake-secret; PREF=f6=40"));
    req.headers.insert("authorization", HeaderValue::from_static("Bearer fake-token"));
    req.headers.insert("referer", HeaderValue::from_static("https://www.youtube.com/"));
    let job = VideoJob::prepare(req).await.unwrap();
    assert!(matches!(job.run(CancellationToken::new()).await.unwrap(), Outcome::Completed(_)));

    let log = std::fs::read_to_string(tools.path().join("calls.log")).unwrap();
    let args: Vec<&str> = log.lines().filter(|l| l.starts_with("ARGS ")).collect();
    assert_eq!(args.len(), 2, "details, then the download");
    for line in &args {
        assert!(!line.contains("fake-secret") && !line.contains("fake-token"), "credentials on the command line: {line}");
        assert!(line.contains("referer:https://www.youtube.com/"), "other headers still go along: {line}");
    }
    // Scoped to the page's host, HTTPS only.
    assert!(log.contains("www.youtube.com\tFALSE\t/\tTRUE\t0\tSID\tfake-secret\n"), "{log}");
    assert!(log.contains("www.youtube.com\tFALSE\t/\tTRUE\t0\tPREF\tf6=40\n"), "{log}");
    assert!(!log.contains("fake-token"), "other credentials are dropped");
    // Readable by the user only, and gone afterwards.
    assert_eq!(log.lines().filter(|l| l.starts_with("-rw-------")).count(), 2, "{log}");
    assert_eq!(log.lines().filter(|l| l.starts_with("drwx------")).count(), 2, "{log}");
    for path in log.lines().filter_map(|l| l.strip_prefix("COOKIEFILE ")) {
        assert!(!Path::new(path).exists(), "{path} was left behind");
        assert!(!Path::new(path).parent().unwrap().exists());
    }
}

#[tokio::test]
async fn a_scanned_page_downloads_its_first_video_but_real_playlists_are_refused() {
    let tools = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let job = VideoJob::prepare(request("https://news.example/scanned-story", out.path(), fake_ytdlp(tools.path()))).await.unwrap();
    assert!(job.info().file_name.starts_with("Main player."), "{}", job.info().file_name);
    assert_eq!(job.info().total, Some(2000));
    assert!(matches!(job.run(CancellationToken::new()).await.unwrap(), Outcome::Completed(_)));
    let log = std::fs::read_to_string(tools.path().join("calls.log")).unwrap();
    let download = log.lines().filter(|l| l.starts_with("ARGS ")).nth(1).unwrap();
    assert!(download.contains("--playlist-items 1"), "{download}");

    let err = VideoJob::prepare(request("https://www.youtube.com/playlist?list=PL1", out.path(), fake_ytdlp(tools.path())))
        .await
        .err()
        .expect("a real playlist is refused");
    assert!(format!("{err:#}").contains("playlist"), "{err:#}");
}
