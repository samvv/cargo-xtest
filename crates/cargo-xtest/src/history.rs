//! Measured peak memory per test, kept in `<target>/xtest/memory.json`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct History {
    path: PathBuf,
    pub peaks: BTreeMap<String, u64>,
}

impl History {
    pub fn load(target_dir: &Path) -> History {
        let path = target_dir.join("xtest/memory.json");
        // A missing or unreadable file just means nothing was measured yet.
        let peaks = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        History { path, peaks }
    }

    /// Records the latest peaks and writes the file atomically.
    pub fn save(&mut self, measured: BTreeMap<String, u64>) -> Result<()> {
        if measured.is_empty() {
            return Ok(());
        }
        self.peaks.extend(measured);
        std::fs::create_dir_all(self.path.parent().unwrap())?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&self.peaks)?)?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("writing {}", self.path.display()))
    }
}
