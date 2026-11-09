//! The set of files a host needs: workspace sources and fixtures (respecting
//! `.gitignore`), extra `include` globs, and the built executables.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use xtest_proto::{FileEntry, Hash};

use crate::hashcache::{HashCache, Stamp};

pub struct FileSet {
    pub mirrors: Vec<PathBuf>,
    pub files: BTreeMap<PathBuf, FileEntry>,
    pub sizes: BTreeMap<Hash, u64>,
    /// How many files were hashed (the rest came from the hash cache).
    pub hashed: usize,
}

struct Pending {
    path: PathBuf,
    link: bool,
}

pub fn collect(
    workspace_root: &Path,
    target_dir: &Path,
    include: &[String],
    executables: &[PathBuf],
    rehash: bool,
) -> Result<FileSet> {
    let cache = if rehash { HashCache::empty(target_dir) } else { HashCache::load(target_dir) };
    let mut pending: BTreeMap<PathBuf, Pending> = BTreeMap::new();
    let target = target_dir.to_owned();
    let skip = move |p: &Path| p.starts_with(&target) || p.file_name().is_some_and(|n| n == ".git");

    let walk = ignore::WalkBuilder::new(workspace_root)
        .hidden(false)
        .require_git(false)
        .filter_entry(move |e| !skip(e.path()))
        .build();
    for e in walk {
        let e = e?;
        if e.path().is_file() {
            pending.insert(e.path().to_owned(), Pending { path: e.path().to_owned(), link: false });
        }
    }

    if !include.is_empty() {
        let mut b = GlobSetBuilder::new();
        for g in include {
            b.add(Glob::new(g).with_context(|| format!("include glob `{g}`"))?);
        }
        let globs = b.build()?;
        let walk = ignore::WalkBuilder::new(workspace_root)
            .standard_filters(false)
            .filter_entry(|e| e.file_name() != ".git")
            .build();
        for e in walk {
            let e = e?;
            let rel = e.path().strip_prefix(workspace_root).unwrap();
            if e.path().is_file() && globs.is_match(rel) {
                pending.insert(e.path().to_owned(), Pending { path: e.path().to_owned(), link: false });
            }
        }
    }

    for exe in executables {
        pending.insert(exe.clone(), Pending { path: exe.clone(), link: true });
    }

    let mut mirrors = vec![workspace_root.to_owned()];
    if !target_dir.starts_with(workspace_root) {
        mirrors.push(target_dir.to_owned());
    }

    let pending: Vec<Pending> = pending.into_values().collect();
    let hashed = hash_all(&pending, &cache)?;
    let mut files = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    let mut fresh = 0;
    let mut keep = Vec::new();
    for (p, h) in pending.iter().zip(hashed) {
        sizes.insert(h.hash, h.size);
        files.insert(p.path.clone(), FileEntry { path: p.path.clone(), hash: h.hash, mode: h.mode, link: p.link });
        fresh += usize::from(!h.cached);
        if let Some(stamp) = h.cacheable {
            keep.push((p.path.clone(), stamp, h.hash));
        }
    }
    // The cache is only an optimization; a run never fails because of it.
    if let Err(e) = cache.save(keep) {
        eprintln!("warning: could not save the hash cache: {e:#}");
    }
    Ok(FileSet { mirrors, files, sizes, hashed: fresh })
}

struct Hashed {
    hash: Hash,
    mode: u32,
    size: u64,
    /// Taken from the cache instead of reading the file.
    cached: bool,
    /// Stamp to cache the hash under, unless the file may still change unseen.
    cacheable: Option<Stamp>,
}

fn hash_all(files: &[Pending], cache: &HashCache) -> Result<Vec<Hashed>> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| s.spawn(move || part.iter().map(|p| hash_one(&p.path, cache)).collect::<Result<Vec<_>>>()))
            .collect();
        let mut out = Vec::with_capacity(files.len());
        for h in handles {
            out.extend(h.join().expect("hashing thread panicked")?);
        }
        Ok(out)
    })
}

fn hash_one(path: &Path, cache: &HashCache) -> Result<Hashed> {
    let read_at = SystemTime::now();
    let md = std::fs::metadata(path)?;
    let stamp = Stamp::of(&md);
    let (mode, size) = (md.permissions().mode(), md.len());
    if let Some(hash) = cache.get(path, &stamp) {
        return Ok(Hashed { hash, mode, size, cached: true, cacheable: Some(stamp) });
    }
    let hash = xtest_agent::store::hash_file(path).with_context(|| format!("hashing {}", path.display()))?;
    // Not cached if it changed while we read it, or may change unseen.
    let unchanged = std::fs::metadata(path).is_ok_and(|after| Stamp::of(&after) == stamp);
    let cacheable = (unchanged && !stamp.is_racy(read_at)).then_some(stamp);
    Ok(Hashed { hash, mode, size, cached: false, cacheable })
}
