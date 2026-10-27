use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    /// Downloads running at once; the rest wait in the queue.
    pub max_active: usize,
    /// Connections per download.
    pub connections: usize,
    /// Global limit in bytes/s across all downloads. 0 = unlimited.
    pub speed_limit: u64,
    /// Move finished files into a category subfolder of `download_dir`.
    pub use_categories: bool,
    pub categories: Vec<Category>,
    pub schedule: Schedule,
    pub notify: bool,
    /// Show the add dialog for downloads sent by the browser extension
    /// instead of starting them straight away.
    pub confirm_browser_downloads: bool,
    pub bridge_port: u16,
    /// Shared secret the browser extension must send.
    pub bridge_token: String,
    /// Retry downloads that failed because of the network, with backoff.
    pub auto_retry: bool,
    /// Keep the Mac from sleeping while anything downloads.
    pub keep_awake: bool,
    /// Offer to download links you copy.
    pub clipboard_watch: bool,
    /// Default quality for video-site downloads.
    pub video_quality: String,
    /// Global limit of the "Background" speed profile, bytes/s.
    pub background_limit: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: dirs_downloads(),
            max_active: 3,
            connections: 8,
            speed_limit: 0,
            use_categories: false,
            categories: default_categories(),
            schedule: Schedule::default(),
            notify: true,
            confirm_browser_downloads: true,
            bridge_port: 47321,
            bridge_token: String::new(),
            auto_retry: true,
            keep_awake: true,
            clipboard_watch: true,
            video_quality: "best".into(),
            background_limit: 1024 * 1024,
        }
    }
}

impl Settings {
    /// The category whose extensions match `file_name`, if any.
    pub fn category_for(&self, file_name: &str) -> Option<&Category> {
        let ext = Path::new(file_name).extension()?.to_str()?.to_ascii_lowercase();
        self.categories.iter().find(|c| c.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext)))
    }

    pub(crate) fn sanitized(mut self) -> Self {
        self.max_active = self.max_active.clamp(1, 20);
        self.connections = self.connections.clamp(1, 32);
        if self.background_limit == 0 {
            self.background_limit = Self::default().background_limit;
        }
        if self.bridge_port < 1024 {
            self.bridge_port = Self::default().bridge_port;
        }
        if self.bridge_token.len() < 16 {
            self.bridge_token = random_token();
        }
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Category {
    pub name: String,
    /// Subfolder of the download folder.
    pub folder: String,
    pub extensions: Vec<String>,
}

/// A daily window in which queued downloads may start automatically.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Schedule {
    pub enabled: bool,
    /// Local time, "HH:MM".
    pub start: String,
    pub stop: String,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { enabled: false, start: "02:00".into(), stop: "08:00".into() }
    }
}

impl Schedule {
    /// Whether automatic downloads may run at `now`. Windows may cross
    /// midnight (23:00-07:00). Always true when disabled or misconfigured.
    pub fn in_window(&self, now: NaiveTime) -> bool {
        let parse = |s: &str| NaiveTime::parse_from_str(s.trim(), "%H:%M").ok();
        let (Some(start), Some(stop)) = (parse(&self.start), parse(&self.stop)) else {
            return true;
        };
        if !self.enabled || start == stop {
            true
        } else if start < stop {
            now >= start && now < stop
        } else {
            now >= start || now < stop
        }
    }
}

fn default_categories() -> Vec<Category> {
    let cat = |name: &str, exts: &str| Category {
        name: name.into(),
        folder: name.into(),
        extensions: exts.split_whitespace().map(String::from).collect(),
    };
    vec![
        cat("Video", "mp4 mkv avi mov wmv flv webm m4v ts mpg mpeg 3gp"),
        cat("Music", "mp3 flac wav aac m4a ogg opus wma aiff"),
        cat("Archives", "zip rar 7z tar gz bz2 xz tgz zst"),
        cat("Programs", "dmg pkg app exe msi deb rpm apk iso img appimage"),
        cat("Documents", "pdf doc docx xls xlsx ppt pptx txt rtf epub csv odt pages key numbers"),
        cat("Images", "jpg jpeg png gif webp heic svg tiff bmp raw"),
    ]
}

fn dirs_downloads() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join("Downloads")
}

pub(crate) fn random_token() -> String {
    let mut bytes = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .expect("read /dev/urandom");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> NaiveTime {
        NaiveTime::parse_from_str(s, "%H:%M").unwrap()
    }

    #[test]
    fn schedule_windows() {
        let mut s = Schedule { enabled: true, start: "02:00".into(), stop: "08:00".into() };
        assert!(s.in_window(t("03:00")));
        assert!(!s.in_window(t("08:00")));
        assert!(!s.in_window(t("23:00")));
        s.start = "23:00".into();
        s.stop = "07:00".into();
        assert!(s.in_window(t("23:30")));
        assert!(s.in_window(t("01:00")));
        assert!(!s.in_window(t("12:00")));
        s.enabled = false;
        assert!(s.in_window(t("12:00")));
    }

    #[test]
    fn categories_match_extensions() {
        let s = Settings::default();
        assert_eq!(s.category_for("movie.MKV").map(|c| c.name.as_str()), Some("Video"));
        assert_eq!(s.category_for("setup.dmg").map(|c| c.name.as_str()), Some("Programs"));
        assert!(s.category_for("README").is_none());
    }

    #[test]
    fn sanitizing_fills_token() {
        let s = Settings::default().sanitized();
        assert_eq!(s.bridge_token.len(), 48);
        assert_ne!(s.bridge_token, Settings::default().sanitized().bridge_token);
    }
}
