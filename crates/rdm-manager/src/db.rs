use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row as SqlRow, params};
use serde::{Deserialize, Serialize};

use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Hls,
    /// A page on a video site, downloaded with yt-dlp.
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Waiting for a free slot (or the schedule window).
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => Self::Paused,
        }
    }
}

/// One download as stored.
#[derive(Debug, Clone, Serialize)]
pub struct Download {
    pub id: i64,
    pub url: String,
    pub kind: Kind,
    /// Folder it downloads into.
    pub dir: PathBuf,
    /// Chosen once the server has been probed (or given by the user).
    pub file_name: Option<String>,
    /// Final location once known; moves if a category folder applies.
    pub path: Option<PathBuf>,
    pub status: Status,
    pub total: Option<u64>,
    pub downloaded: u64,
    pub connections: usize,
    /// Per-download limit in bytes/s. 0 = unlimited.
    pub speed_limit: u64,
    pub checksum: Option<String>,
    /// Sent with every request; cleared once the download completes.
    #[serde(skip)]
    pub headers: Vec<(String, String)>,
    pub error: Option<String>,
    /// Started by hand, so it ignores the schedule window.
    pub manual: bool,
    /// No folder was chosen, so the category folder may apply.
    pub auto_category: bool,
    pub resumable: bool,
    /// Unix milliseconds.
    pub added_at: i64,
    pub finished_at: Option<i64>,
    /// Queued again after a network failure; don't start before this (Unix ms).
    pub retry_at: Option<i64>,
    /// Automatic retries since the last success or manual resume.
    pub auto_retries: u32,
    /// Video downloads: "best", "1080", "720", "audio", ...
    pub quality: Option<String>,
}

/// Fields for a new row.
pub struct NewDownload {
    pub url: String,
    pub kind: Kind,
    pub dir: PathBuf,
    pub file_name: Option<String>,
    pub status: Status,
    pub connections: usize,
    pub speed_limit: u64,
    pub checksum: Option<String>,
    pub headers: Vec<(String, String)>,
    pub manual: bool,
    pub auto_category: bool,
    pub quality: Option<String>,
}

pub struct Db(Connection);

pub struct HistoryEntry {
    pub download_id: i64,
    pub name: String,
    pub host: String,
    pub category: String,
    pub bytes: u64,
    pub active_ms: u64,
    pub peak_speed: f64,
    pub ok: bool,
}

pub struct HistoryTotals {
    pub completed: u64,
    pub failed: u64,
    /// Bytes and time of completed downloads whose run time is known.
    pub timed_bytes: u64,
    pub timed_ms: u64,
    pub peak_speed: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub name: String,
    pub bytes: u64,
    pub count: u64,
}

#[derive(Clone, Copy)]
pub enum GroupBy {
    Category,
    Host,
}

/// Makes the database's folder (0700) and the database with SQLite's
/// companion files (0600) private to the user before SQLite opens them, and
/// tightens installs from before this. SQLite creates new WAL and
/// shared-memory files with the database's own permissions.
#[cfg(unix)]
fn lock_down(path: &Path) -> std::io::Result<()> {
    use std::fs::{DirBuilder, OpenOptions, Permissions, set_permissions};
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        set_permissions(dir, Permissions::from_mode(0o700))?;
    }
    // Creates the database file, private, if it doesn't exist yet.
    OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        match set_permissions(&file, Permissions::from_mode(0o600)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Windows keeps each user's app data folder private already.
#[cfg(not(unix))]
fn lock_down(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(dir) => std::fs::create_dir_all(dir),
        None => Ok(()),
    }
}

const COLUMNS: &str = "id, url, kind, dir, file_name, path, status, total, downloaded, connections, \
     speed_limit, checksum, headers, error, manual, auto_category, resumable, added_at, finished_at, \
     retry_at, auto_retries, quality";

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        // The pairing token and pending downloads' cookies live in here.
        lock_down(path).with_context(|| format!("could not secure {}", path.display()))?;
        let conn = Connection::open(path).with_context(|| format!("could not open {}", path.display()))?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS downloads (
                id INTEGER PRIMARY KEY,
                url TEXT NOT NULL,
                kind TEXT NOT NULL,
                dir TEXT NOT NULL,
                file_name TEXT,
                path TEXT,
                status TEXT NOT NULL,
                total INTEGER,
                downloaded INTEGER NOT NULL DEFAULT 0,
                connections INTEGER NOT NULL,
                speed_limit INTEGER NOT NULL DEFAULT 0,
                checksum TEXT,
                headers TEXT NOT NULL DEFAULT '[]',
                error TEXT,
                manual INTEGER NOT NULL DEFAULT 0,
                auto_category INTEGER NOT NULL DEFAULT 0,
                resumable INTEGER NOT NULL DEFAULT 1,
                added_at INTEGER NOT NULL,
                finished_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             -- Bytes actually transferred, per local hour ('2026-10-27T17').
             -- Kept when downloads are removed, so lifetime stats survive.
             CREATE TABLE IF NOT EXISTS traffic (hour TEXT PRIMARY KEY, bytes INTEGER NOT NULL);
             -- One row per finished (or failed) download, also kept on removal.
             CREATE TABLE IF NOT EXISTS history (
                id INTEGER PRIMARY KEY,
                download_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                host TEXT NOT NULL,
                category TEXT NOT NULL,
                bytes INTEGER NOT NULL,
                active_ms INTEGER NOT NULL,
                peak_speed REAL NOT NULL,
                ok INTEGER NOT NULL,
                finished_at INTEGER NOT NULL
             );",
        )?;
        // Columns added after the first release; ignore "duplicate column".
        for column in [
            "active_ms INTEGER NOT NULL DEFAULT 0",
            "peak_speed REAL NOT NULL DEFAULT 0",
            "retry_at INTEGER",
            "auto_retries INTEGER NOT NULL DEFAULT 0",
            "quality TEXT",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE downloads ADD COLUMN {column}"), []);
        }
        Ok(Self(conn))
    }

    pub fn add_traffic(&self, hour: &str, bytes: u64) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        self.0.execute(
            "INSERT INTO traffic (hour, bytes) VALUES (?1, ?2)
             ON CONFLICT(hour) DO UPDATE SET bytes = bytes + excluded.bytes",
            params![hour, bytes as i64],
        )?;
        Ok(())
    }

    /// Adds one run's active time and peak speed to a download, and returns
    /// the totals across all its runs.
    pub fn add_run(&self, id: i64, active_ms: u64, peak_speed: f64) -> Result<(u64, f64)> {
        self.0.execute(
            "UPDATE downloads SET active_ms = active_ms + ?2, peak_speed = MAX(peak_speed, ?3) WHERE id = ?1",
            params![id, active_ms as i64, peak_speed],
        )?;
        Ok(self
            .0
            .query_row("SELECT active_ms, peak_speed FROM downloads WHERE id = ?1", [id], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get(1)?))
            })
            .optional()?
            .unwrap_or((active_ms, peak_speed)))
    }

    pub fn add_history(&self, h: &HistoryEntry) -> Result<()> {
        self.0.execute(
            "INSERT INTO history (download_id, name, host, category, bytes, active_ms, peak_speed, ok, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                h.download_id,
                h.name,
                h.host,
                h.category,
                h.bytes as i64,
                h.active_ms as i64,
                h.peak_speed,
                h.ok,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// All hourly traffic, oldest first.
    pub fn traffic(&self) -> Result<Vec<(String, u64)>> {
        let mut stmt = self.0.prepare("SELECT hour, bytes FROM traffic ORDER BY hour")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn history_totals(&self) -> Result<HistoryTotals> {
        Ok(self.0.query_row(
            "SELECT COALESCE(SUM(ok), 0), COALESCE(SUM(1 - ok), 0),
                    COALESCE(SUM(CASE WHEN ok AND active_ms > 0 THEN bytes END), 0),
                    COALESCE(SUM(CASE WHEN ok AND active_ms > 0 THEN active_ms END), 0),
                    COALESCE(MAX(peak_speed), 0)
             FROM history",
            [],
            |r| {
                Ok(HistoryTotals {
                    completed: r.get::<_, i64>(0)? as u64,
                    failed: r.get::<_, i64>(1)? as u64,
                    timed_bytes: r.get::<_, i64>(2)? as u64,
                    timed_ms: r.get::<_, i64>(3)? as u64,
                    peak_speed: r.get(4)?,
                })
            },
        )?)
    }

    pub fn largest(&self) -> Result<Option<Group>> {
        Ok(self
            .0
            .query_row("SELECT name, bytes FROM history WHERE ok ORDER BY bytes DESC LIMIT 1", [], |r| {
                Ok(Group { name: r.get(0)?, bytes: r.get::<_, i64>(1)? as u64, count: 1 })
            })
            .optional()?)
    }

    /// Finished downloads grouped by `category` or `host`, biggest first.
    pub fn history_groups(&self, column: GroupBy, limit: usize) -> Result<Vec<Group>> {
        let col = match column {
            GroupBy::Category => "category",
            GroupBy::Host => "host",
        };
        let mut stmt = self.0.prepare(&format!(
            "SELECT {col}, SUM(bytes), COUNT(*) FROM history WHERE ok GROUP BY {col} ORDER BY 2 DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(Group { name: r.get(0)?, bytes: r.get::<_, i64>(1)? as u64, count: r.get::<_, i64>(2)? as u64 })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn insert(&self, d: &NewDownload) -> Result<i64> {
        self.0.execute(
            "INSERT INTO downloads (url, kind, dir, file_name, status, connections, speed_limit,
                checksum, headers, manual, auto_category, quality, added_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                d.url,
                kind_str(d.kind),
                d.dir.to_string_lossy(),
                d.file_name,
                d.status.as_str(),
                d.connections as i64,
                d.speed_limit as i64,
                d.checksum,
                serde_json::to_string(&d.headers)?,
                d.manual,
                d.auto_category,
                d.quality,
                now_ms(),
            ],
        )?;
        Ok(self.0.last_insert_rowid())
    }

    pub fn get(&self, id: i64) -> Result<Option<Download>> {
        Ok(self
            .0
            .query_row(&format!("SELECT {COLUMNS} FROM downloads WHERE id = ?1"), [id], from_row)
            .optional()?)
    }

    /// Newest first.
    pub fn list(&self) -> Result<Vec<Download>> {
        let mut stmt = self.0.prepare(&format!("SELECT {COLUMNS} FROM downloads ORDER BY id DESC"))?;
        let rows = stmt.query_map([], from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Queue order: oldest first.
    pub fn queued(&self) -> Result<Vec<Download>> {
        let mut stmt =
            self.0.prepare(&format!("SELECT {COLUMNS} FROM downloads WHERE status = 'queued' ORDER BY id"))?;
        let rows = stmt.query_map([], from_row)?.collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    pub fn set_status(&self, id: i64, status: Status, error: Option<&str>) -> Result<()> {
        self.0.execute(
            "UPDATE downloads SET status = ?2, error = ?3 WHERE id = ?1",
            params![id, status.as_str(), error],
        )?;
        Ok(())
    }

    /// Puts a download back in the queue to retry after `retry_at` (Unix ms).
    pub fn schedule_retry(&self, id: i64, retry_at: i64, auto_retries: u32, error: &str) -> Result<()> {
        self.0.execute(
            "UPDATE downloads SET status = 'queued', retry_at = ?2, auto_retries = ?3, error = ?4 WHERE id = ?1",
            params![id, retry_at, auto_retries as i64, error],
        )?;
        Ok(())
    }

    /// Forgets automatic retries, e.g. when the user resumes by hand.
    pub fn clear_retry(&self, id: i64) -> Result<()> {
        self.0.execute("UPDATE downloads SET retry_at = NULL, auto_retries = 0 WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn set_manual(&self, id: i64, manual: bool) -> Result<()> {
        self.0.execute("UPDATE downloads SET manual = ?2 WHERE id = ?1", params![id, manual])?;
        Ok(())
    }

    /// Records what the probe decided.
    pub fn set_started(&self, id: i64, file_name: &str, path: &Path, total: Option<u64>, resumable: bool) -> Result<()> {
        self.0.execute(
            "UPDATE downloads SET file_name = ?2, path = ?3, total = ?4, resumable = ?5 WHERE id = ?1",
            params![id, file_name, path.to_string_lossy(), total.map(|t| t as i64), resumable],
        )?;
        Ok(())
    }

    pub fn set_downloaded(&self, id: i64, downloaded: u64) -> Result<()> {
        self.0.execute("UPDATE downloads SET downloaded = ?2 WHERE id = ?1", params![id, downloaded as i64])?;
        Ok(())
    }

    pub fn set_completed(&self, id: i64, path: &Path, downloaded: u64) -> Result<()> {
        self.0.execute(
            "UPDATE downloads SET status = 'completed', error = NULL, path = ?2, downloaded = ?3,
                total = COALESCE(total, ?3), finished_at = ?4, headers = '[]',
                retry_at = NULL, auto_retries = 0 WHERE id = ?1",
            params![id, path.to_string_lossy(), downloaded as i64, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_speed_limit(&self, id: i64, limit: u64) -> Result<()> {
        self.0.execute("UPDATE downloads SET speed_limit = ?2 WHERE id = ?1", params![id, limit as i64])?;
        Ok(())
    }

    /// Switches to a new link for the same file; errors and retries reset.
    pub fn set_url(&self, id: i64, url: &str, headers: &[(String, String)]) -> Result<()> {
        self.0.execute(
            "UPDATE downloads SET url = ?2, headers = ?3, error = NULL, retry_at = NULL, auto_retries = 0 WHERE id = ?1",
            params![id, url, serde_json::to_string(headers)?],
        )?;
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.0.execute("DELETE FROM downloads WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Downloads interrupted by a crash or quit go back in the queue.
    pub fn requeue_running(&self) -> Result<()> {
        self.0.execute("UPDATE downloads SET status = 'queued' WHERE status = 'running'", [])?;
        Ok(())
    }

    pub fn load_settings(&self) -> Result<Option<Settings>> {
        let json: Option<String> = self
            .0
            .query_row("SELECT value FROM settings WHERE key = 'settings'", [], |r| r.get(0))
            .optional()?;
        // Unknown or missing fields fall back to defaults.
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        self.0.execute(
            "INSERT INTO settings (key, value) VALUES ('settings', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [serde_json::to_string(settings)?],
        )?;
        Ok(())
    }
}

fn kind_str(kind: Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Hls => "hls",
        Kind::Video => "video",
    }
}

fn from_row(r: &SqlRow) -> rusqlite::Result<Download> {
    let headers: String = r.get("headers")?;
    Ok(Download {
        id: r.get("id")?,
        url: r.get("url")?,
        kind: match r.get::<_, String>("kind")?.as_str() {
            "hls" => Kind::Hls,
            "video" => Kind::Video,
            _ => Kind::File,
        },
        dir: PathBuf::from(r.get::<_, String>("dir")?),
        file_name: r.get("file_name")?,
        path: r.get::<_, Option<String>>("path")?.map(PathBuf::from),
        status: Status::parse(&r.get::<_, String>("status")?),
        total: r.get::<_, Option<i64>>("total")?.map(|t| t as u64),
        downloaded: r.get::<_, i64>("downloaded")? as u64,
        connections: r.get::<_, i64>("connections")? as usize,
        speed_limit: r.get::<_, i64>("speed_limit")? as u64,
        checksum: r.get("checksum")?,
        headers: serde_json::from_str(&headers).unwrap_or_default(),
        error: r.get("error")?,
        manual: r.get("manual")?,
        auto_category: r.get("auto_category")?,
        resumable: r.get("resumable")?,
        added_at: r.get("added_at")?,
        finished_at: r.get("finished_at")?,
        retry_at: r.get("retry_at")?,
        auto_retries: r.get::<_, i64>("auto_retries")? as u32,
        quality: r.get("quality")?,
    })
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn database_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("app/flash.sqlite3");
        let db = Db::open(&path).unwrap();
        db.save_settings(&Settings::default()).unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        // SQLite's own files, while it has them open, follow the database.
        let wal = PathBuf::from(format!("{}-wal", path.display()));
        assert_eq!(mode(&wal), 0o600);
        drop(db);

        // Installs from before this are tightened on the next start.
        for p in [path.parent().unwrap(), &path] {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _db = Db::open(&path).unwrap();
        assert_eq!((mode(path.parent().unwrap()), mode(&path)), (0o700, 0o600));
    }

    fn new(url: &str) -> NewDownload {
        NewDownload {
            url: url.into(),
            kind: Kind::File,
            dir: "/tmp".into(),
            file_name: None,
            status: Status::Queued,
            connections: 8,
            speed_limit: 0,
            checksum: None,
            headers: vec![("Cookie".into(), "a=b".into())],
            manual: false,
            auto_category: true,
            quality: None,
        }
    }

    #[test]
    fn round_trips_rows() {
        let db = Db::in_memory().unwrap();
        let a = db.insert(&new("https://a.com/1")).unwrap();
        let b = db.insert(&new("https://a.com/2")).unwrap();
        db.set_started(a, "1.zip", Path::new("/tmp/1.zip"), Some(100), true).unwrap();
        db.set_status(b, Status::Running, None).unwrap();

        let row = db.get(a).unwrap().unwrap();
        assert_eq!(row.file_name.as_deref(), Some("1.zip"));
        assert_eq!(row.total, Some(100));
        assert_eq!(row.headers, vec![("Cookie".to_string(), "a=b".to_string())]);
        assert_eq!(db.queued().unwrap().len(), 1);

        db.requeue_running().unwrap();
        assert_eq!(db.queued().unwrap().iter().map(|d| d.id).collect::<Vec<_>>(), vec![a, b]);

        db.set_completed(a, Path::new("/tmp/Archives/1.zip"), 100).unwrap();
        let row = db.get(a).unwrap().unwrap();
        assert_eq!(row.status, Status::Completed);
        assert!(row.headers.is_empty(), "cookies are dropped once done");
        assert!(row.finished_at.is_some());

        db.delete(b).unwrap();
        assert_eq!(db.list().unwrap().len(), 1);
    }

    #[test]
    fn settings_persist() {
        let db = Db::in_memory().unwrap();
        assert!(db.load_settings().unwrap().is_none());
        db.save_settings(&Settings { max_active: 5, ..Default::default() }).unwrap();
        assert_eq!(db.load_settings().unwrap().unwrap().max_active, 5);
    }
}
