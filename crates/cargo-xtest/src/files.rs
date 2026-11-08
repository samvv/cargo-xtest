//! The set of files a host needs: workspace sources and fixtures (respecting
//! `.gitignore`), extra `include` globs, and the built executables.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSetBuilder};
use xtest_proto::{FileEntry, Hash};

pub struct FileSet {
    pub mirrors: Vec<PathBuf>,
    pub files: BTreeMap<PathBuf, FileEntry>,
    pub sizes: BTreeMap<Hash, u64>,
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
) -> Result<FileSet> {
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
    let hashed = hash_all(&pending)?;
    let mut files = BTreeMap::new();
    let mut sizes = BTreeMap::new();
    for (p, (hash, mode, size)) in pending.iter().zip(hashed) {
        sizes.insert(hash, size);
        files.insert(p.path.clone(), FileEntry { path: p.path.clone(), hash, mode, link: p.link });
    }
    Ok(FileSet { mirrors, files, sizes })
}

fn hash_all(files: &[Pending]) -> Result<Vec<(Hash, u32, u64)>> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let chunk = files.len().div_ceil(threads).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .map(|p| {
                            let md = std::fs::metadata(&p.path)?;
                            let hash = xtest_agent::store::hash_file(&p.path)
                                .with_context(|| format!("hashing {}", p.path.display()))?;
                            Ok((hash, md.permissions().mode(), md.len()))
                        })
                        .collect::<Result<Vec<_>>>()
                })
            })
            .collect();
        let mut out = Vec::with_capacity(files.len());
        for h in handles {
            out.extend(h.join().expect("hashing thread panicked")?);
        }
        Ok(out)
    })
}
