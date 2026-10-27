use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::probe::Probe;

const VERSION: u32 = 1;

/// Contents of the `<name>.rdm.json` sidecar that lets a download resume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct SavedState {
    pub version: u32,
    pub url: String,
    pub total: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Byte ranges `[start, end)` that are not on disk yet.
    pub remaining: Vec<(u64, u64)>,
}

impl SavedState {
    pub fn new(url: &str, total: u64, probe: &Probe) -> Self {
        Self {
            version: VERSION,
            url: url.to_owned(),
            total,
            etag: probe.etag.clone(),
            last_modified: probe.last_modified.clone(),
            remaining: Vec::new(),
        }
    }

    /// Returns `None` if the file is missing, unreadable or from another version.
    pub fn load(path: &Path) -> Option<Self> {
        let state: Self = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        (state.version == VERSION).then_some(state)
    }

    /// Writes atomically: a crash leaves either the old or the new state.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        let mut file = File::create(&tmp)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Whether the file on the server is still the one we started downloading.
    pub fn still_valid(&self, probe: &Probe) -> bool {
        fn same(a: &Option<String>, b: &Option<String>) -> bool {
            a.is_none() || b.is_none() || a == b
        }
        probe.ranges
            && probe.total == Some(self.total)
            && same(&self.etag, &probe.etag)
            && same(&self.last_modified, &probe.last_modified)
            && self.remaining.iter().all(|&(s, e)| s <= e && e <= self.total)
    }
}
