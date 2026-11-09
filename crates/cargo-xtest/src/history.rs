//! What earlier runs measured, kept in `<target>/xtest/history.json`: each
//! test's peak memory and duration, and each fixture's peak memory (under
//! `fixture:<name>`). Only the latest value of each is kept.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Bumped when the file format changes; an older file is ignored.
const VERSION: u32 = 1;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct HistoryFile {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

pub struct History {
    path: PathBuf,
    /// Written by versions before durations were kept; migrated on save.
    old_path: PathBuf,
    entries: BTreeMap<String, Entry>,
}

/// What a run measured, by history key.
#[derive(Debug, Default)]
pub struct Measured {
    pub peaks: BTreeMap<String, u64>,
    pub durations: BTreeMap<String, Duration>,
}

impl History {
    pub fn load(target_dir: &Path) -> History {
        let dir = target_dir.join("xtest");
        let (path, old_path) = (dir.join("history.json"), dir.join("memory.json"));
        // A missing or unreadable file just means nothing was measured yet.
        let entries = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<HistoryFile>(&bytes)
                .ok()
                .filter(|f| f.version == VERSION)
                .map(|f| f.entries)
                .unwrap_or_default(),
            Err(_) => std::fs::read(&old_path)
                .ok()
                .and_then(|b| serde_json::from_slice::<BTreeMap<String, u64>>(&b).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(k, memory)| (k, Entry { memory: Some(memory), duration_ms: None }))
                .collect(),
        };
        History { path, old_path, entries }
    }

    /// The peak memory measured last time, in bytes.
    pub fn peak(&self, key: &str) -> Option<u64> {
        self.entries.get(key)?.memory
    }

    /// How long the test took last time.
    pub fn duration(&self, key: &str) -> Option<Duration> {
        self.entries.get(key)?.duration_ms.map(Duration::from_millis)
    }

    /// Records the latest measurements and writes the file atomically.
    pub fn save(&mut self, measured: Measured) -> Result<()> {
        if measured.peaks.is_empty() && measured.durations.is_empty() {
            return Ok(());
        }
        for (k, peak) in measured.peaks {
            self.entries.entry(k).or_default().memory = Some(peak);
        }
        for (k, d) in measured.durations {
            self.entries.entry(k).or_default().duration_ms = Some(d.as_millis() as u64);
        }
        std::fs::create_dir_all(self.path.parent().unwrap())?;
        let file = HistoryFile { version: VERSION, entries: std::mem::take(&mut self.entries) };
        let tmp = self.path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&file)?)?;
        self.entries = file.entries;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("writing {}", self.path.display()))?;
        let _ = std::fs::remove_file(&self.old_path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_peaks_and_durations_and_migrates_memory_json() {
        let dir = tempfile::tempdir().unwrap();
        let x = dir.path().join("xtest");
        std::fs::create_dir_all(&x).unwrap();
        std::fs::write(x.join("memory.json"), r#"{"a t1": 1000, "fixture:pg": 5000}"#).unwrap();

        let mut h = History::load(dir.path());
        assert_eq!((h.peak("a t1"), h.duration("a t1")), (Some(1000), None));
        let mut m = Measured::default();
        m.durations.insert("a t1".into(), Duration::from_millis(1500));
        m.peaks.insert("a t2".into(), 7);
        h.save(m).unwrap();
        assert!(!x.join("memory.json").exists());

        let h = History::load(dir.path());
        assert_eq!((h.peak("a t1"), h.duration("a t1")), (Some(1000), Some(Duration::from_millis(1500))));
        assert_eq!((h.peak("a t2"), h.peak("fixture:pg")), (Some(7), Some(5000)));
    }
}
