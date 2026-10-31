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
        let differs = |a: &Option<String>, b: &Option<String>| a.is_some() && b.is_some() && a != b;
        let matches = |a: &Option<String>, b: &Option<String>| a.is_some() && a == b;
        let (etag, modified) = ((&self.etag, &probe.etag), (&self.last_modified, &probe.last_modified));
        // The servers behind one address can disagree on one of these for the
        // same file, so the other one matching settles it.
        let changed = (differs(etag.0, etag.1) && !matches(modified.0, modified.1))
            || (differs(modified.0, modified.1) && !matches(etag.0, etag.1));
        probe.ranges
            && probe.total == Some(self.total)
            && !changed
            && self.remaining.iter().all(|&(s, e)| s <= e && e <= self.total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(etag: Option<&str>, modified: Option<&str>) -> Probe {
        Probe {
            total: Some(100),
            ranges: true,
            file_name: None,
            etag: etag.map(str::to_owned),
            last_modified: modified.map(str::to_owned),
        }
    }

    #[test]
    fn a_file_counts_as_changed_only_when_nothing_vouches_for_it() {
        let saved = SavedState::new("https://x/f", 100, &probe(Some("a"), Some("mon")));
        assert!(saved.still_valid(&probe(Some("a"), Some("mon"))));
        assert!(saved.still_valid(&probe(None, None)), "no validators to compare");
        assert!(saved.still_valid(&probe(Some("b"), Some("mon"))), "another server's etag, same date");
        assert!(saved.still_valid(&probe(Some("a"), Some("tue"))), "same etag, another server's date");
        assert!(!saved.still_valid(&probe(Some("b"), Some("tue"))), "both differ");
        assert!(!saved.still_valid(&probe(Some("b"), None)), "the only validator differs");
        assert!(!saved.still_valid(&Probe { total: Some(99), ..probe(Some("a"), Some("mon")) }), "another size");
        assert!(!saved.still_valid(&Probe { ranges: false, ..probe(Some("a"), Some("mon")) }));
    }
}
