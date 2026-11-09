//! Cache of file hashes, kept in `<target>/xtest/hashes.json`, so a run only
//! hashes files that changed since the last one.
//!
//! An entry is reused only while the file's size, mtime, ctime, inode and
//! device are all unchanged. ctime can't be set by users, so it also catches
//! a file whose mtime was put back.
//!
//! Timestamps are coarse on some filesystems: a file changed again within the
//! same tick after we read its stat would keep its old stat but have new
//! content. Like git's "racy" index entries, we never store an entry whose
//! mtime or ctime is within [`RACY`] of the moment we read it; such files are
//! hashed again next time. Entries are also skipped if the file changed while
//! it was being hashed.

use std::collections::BTreeMap;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use xtest_proto::Hash;

/// Bumped when the file format or the meaning of an entry changes.
const VERSION: u32 = 1;

/// Timestamps closer than this to when the stat was read aren't trusted.
const RACY: Duration = Duration::from_secs(2);

/// What identifies a file's contents without reading them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stamp {
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    ino: u64,
    dev: u64,
}

impl Stamp {
    pub fn of(md: &Metadata) -> Stamp {
        Stamp {
            size: md.len(),
            mtime_ns: md.mtime().saturating_mul(1_000_000_000).saturating_add(md.mtime_nsec()),
            ctime_ns: md.ctime().saturating_mul(1_000_000_000).saturating_add(md.ctime_nsec()),
            ino: md.ino(),
            dev: md.dev(),
        }
    }

    /// Whether the file may still change without its stamp changing,
    /// given the stamp was read at `read_at`.
    pub fn is_racy(&self, read_at: SystemTime) -> bool {
        let read_ns = read_at.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as i64;
        let limit = read_ns - RACY.as_nanos() as i64;
        self.mtime_ns >= limit || self.ctime_ns >= limit
    }
}

#[derive(Serialize, Deserialize)]
struct Entry {
    stamp: Stamp,
    /// SHA-256, hex.
    hash: String,
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

pub struct HashCache {
    path: PathBuf,
    entries: BTreeMap<PathBuf, (Stamp, Hash)>,
}

impl HashCache {
    /// Loads the cache. A missing, unreadable or outdated cache is empty.
    pub fn load(target_dir: &Path) -> HashCache {
        let path = target_dir.join("xtest/hashes.json");
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<CacheFile>(&b).ok())
            .filter(|c| c.version == VERSION)
            .map(|c| {
                c.entries
                    .into_iter()
                    .filter_map(|(p, e)| {
                        let bytes: [u8; 32] = hex::decode(&e.hash).ok()?.try_into().ok()?;
                        Some((PathBuf::from(p), (e.stamp, Hash(bytes))))
                    })
                    .collect()
            })
            .unwrap_or_default();
        HashCache { path, entries }
    }

    /// An empty cache that saves to the usual place (for `--rehash`).
    pub fn empty(target_dir: &Path) -> HashCache {
        HashCache { path: target_dir.join("xtest/hashes.json"), entries: BTreeMap::new() }
    }

    /// The cached hash of `path`, if its stamp is unchanged.
    pub fn get(&self, path: &Path, stamp: &Stamp) -> Option<Hash> {
        self.entries.get(path).filter(|(s, _)| s == stamp).map(|(_, h)| *h)
    }

    /// Replaces the cache with `entries` (files that no longer exist drop
    /// out) and writes it atomically.
    pub fn save(mut self, entries: impl IntoIterator<Item = (PathBuf, Stamp, Hash)>) -> Result<()> {
        self.entries = entries.into_iter().map(|(p, s, h)| (p, (s, h))).collect();
        let file = CacheFile {
            version: VERSION,
            entries: self
                .entries
                .iter()
                .filter_map(|(p, (stamp, hash))| Some((p.to_str()?.to_owned(), Entry { stamp: *stamp, hash: hash.to_hex() })))
                .collect(),
        };
        std::fs::create_dir_all(self.path.parent().unwrap())?;
        let tmp = self.path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec(&file)?)?;
        std::fs::rename(&tmp, &self.path).with_context(|| format!("writing {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_timestamps_are_racy() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, "x").unwrap();
        let stamp = Stamp::of(&std::fs::metadata(&f).unwrap());
        assert!(stamp.is_racy(SystemTime::now()));
        assert!(!stamp.is_racy(SystemTime::now() + Duration::from_secs(10)));
    }

    #[test]
    fn roundtrips_and_checks_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, "x").unwrap();
        let stamp = Stamp::of(&std::fs::metadata(&f).unwrap());
        let hash = Hash([7; 32]);
        HashCache::empty(dir.path()).save([(f.clone(), stamp, hash)]).unwrap();

        let cache = HashCache::load(dir.path());
        assert_eq!(cache.get(&f, &stamp), Some(hash));
        let changed = Stamp { size: 2, ..stamp };
        assert_eq!(cache.get(&f, &changed), None);

        // Corrupt or outdated caches are ignored.
        std::fs::write(dir.path().join("xtest/hashes.json"), "{not json").unwrap();
        assert_eq!(HashCache::load(dir.path()).get(&f, &stamp), None);
        std::fs::write(dir.path().join("xtest/hashes.json"), r#"{"version": 999, "entries": {}}"#).unwrap();
        assert_eq!(HashCache::load(dir.path()).get(&f, &stamp), None);
    }
}
