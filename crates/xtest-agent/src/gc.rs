//! Removes what earlier agents left behind, and keeps the caches in check:
//!
//! - run directories whose lock is no longer held, and the containers, k3d
//!   clusters and networks labeled with a run that no longer exists;
//! - least recently used store blobs, while the store is over its size cap;
//! - agent builds, kubectl (k3s) binaries and built images not used for a while.
//!
//! "Used" is tracked with mtimes: the agent touches a blob when a run asks
//! for it, its own binary when it starts, and a cache entry when it uses it.

use std::collections::HashSet;
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

/// Default cap on the blob store's size.
pub const DEFAULT_STORE_SIZE: u64 = 10 << 30;

/// What the collector may remove.
#[derive(Debug, Clone)]
pub struct Policy {
    /// Evict blobs while the store is bigger than this.
    pub store_size: u64,
    /// Never evict blobs used more recently than this.
    pub blob_age: Duration,
    /// Remove agent builds, tools and images unused for this long.
    pub cache_age: Duration,
    /// Also remove runs kept with `--keep-fixtures`.
    pub remove_kept: bool,
}

impl Policy {
    /// What every agent does when it starts.
    pub fn startup(store_size: u64) -> Policy {
        Policy {
            store_size,
            blob_age: Duration::from_secs(24 * 3600),
            cache_age: Duration::from_secs(7 * 24 * 3600),
            remove_kept: false,
        }
    }

    /// `cargo xtest clean`: everything that no live run uses.
    pub fn everything() -> Policy {
        Policy { store_size: 0, blob_age: Duration::ZERO, cache_age: Duration::ZERO, remove_kept: true }
    }
}

/// What a collection removed.
#[derive(Debug, Default)]
pub struct Report {
    pub runs: usize,
    pub blobs: usize,
    pub blob_bytes: u64,
    /// Store size after collection.
    pub store_bytes: u64,
    pub agents: usize,
    pub tools: usize,
    pub images: usize,
    /// The store was in use by a run that was syncing, so no blobs were evicted.
    pub store_busy: bool,
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "removed {} run(s), {} blob(s) ({}), {} agent build(s), {} tool(s), {} image(s); store is now {}",
            self.runs,
            self.blobs,
            human(self.blob_bytes),
            self.agents,
            self.tools,
            self.images,
            human(self.store_bytes),
        )?;
        if self.store_busy {
            write!(f, " (store busy: another run was syncing, blobs were left alone)")?;
        }
        Ok(())
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// Takes an exclusive, non-blocking lock. Returns `None` if someone holds it.
pub fn try_lock(path: &Path) -> Option<File> {
    lock(path, libc::LOCK_EX | libc::LOCK_NB)
}

/// Whether nobody holds the lock at `path`. The probe lock is released
/// explicitly: a process forked meanwhile shares the open file, so merely
/// closing ours would leave the lock held until that child calls `exec`.
fn is_unlocked(path: &Path) -> bool {
    match try_lock(path) {
        Some(f) => {
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_UN) };
            true
        }
        None => false,
    }
}

/// Takes a `flock` of the given kind on `path` (created if needed).
pub fn lock(path: &Path, op: libc::c_int) -> Option<File> {
    let f = File::options().create(true).truncate(false).write(true).open(path).ok()?;
    let ok = unsafe { libc::flock(f.as_raw_fd(), op) } == 0;
    ok.then_some(f)
}

/// Marks a file as used now.
pub fn touch(path: &Path) {
    if let Ok(f) = File::open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

/// Lock that runs hold (shared) while they sync and materialize, and the
/// collector takes (exclusive) to evict blobs. Without it, a blob reported
/// as present could be evicted before the run links it.
pub fn store_lock_path(root: &Path) -> PathBuf {
    root.join("store").join("lock")
}

fn age(md: &fs::Metadata) -> Duration {
    md.modified().ok().and_then(|t| t.elapsed().ok()).unwrap_or_default()
}

pub fn collect(root: &Path, docker: bool, policy: &Policy) -> Report {
    let mut report = Report::default();
    let runs = root.join("runs");
    let mut stale = Vec::new();
    if let Ok(entries) = fs::read_dir(&runs) {
        for e in entries.filter_map(|e| e.ok()) {
            let dir = e.path();
            let lock = dir.join("lock");
            let unlocked = if lock.exists() {
                is_unlocked(&lock)
            } else {
                // A brand-new run creates its lock right after its directory.
                e.metadata().map(|m| age(&m) > Duration::from_secs(60)).unwrap_or(false)
            };
            if !unlocked {
                continue;
            }
            if dir.join("keep").exists() && !policy.remove_kept {
                continue;
            }
            stale.push((e.file_name().to_string_lossy().into_owned(), dir));
        }
    }
    // A run is dead if it is stale or its directory is gone altogether.
    let dead = |run: &str| stale.iter().any(|(id, _)| id == run) || !runs.join(run).exists();

    if docker {
        // k3d first: deleting a cluster properly also removes its network
        // and volumes, which force-removing its containers would leave behind.
        collect_k3d(root, &dead);
        collect_networks(root, &dead);
        let out = Command::new("docker")
            .args(["ps", "-a", "--filter"])
            .arg(format!("label=xtest.root={}", root.display()))
            .args(["--format", "{{.Names}} {{.Label \"xtest.run\"}}"])
            .output();
        if let Ok(out) = out {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((name, run)) = line.split_once(' ') {
                    if dead(run) {
                        let _ = Command::new("docker").args(["rm", "-f", name]).output();
                    }
                }
            }
        }
        for (id, _) in &stale {
            remove_containers(&format!("label=xtest.run={id}"));
        }
    }
    for (_, dir) in stale {
        match remove_tree(&dir) {
            Ok(()) => report.runs += 1,
            Err(err) => eprintln!("xtest-agent: could not remove stale run {}: {err}", dir.display()),
        }
    }

    // Caches. Run directories are gone by now, so hardlink counts only
    // include runs that are still alive.
    collect_store(root, policy, &mut report);
    collect_agents(root, policy, &mut report);
    collect_tools(root, policy, &mut report);
    if docker {
        collect_images(root, policy, &mut report);
    }
    report
}

/// Evicts least recently used blobs until the store fits `store_size`.
/// A blob with more than one link is still materialized in a live run.
fn collect_store(root: &Path, policy: &Policy, report: &mut Report) {
    let store = root.join("store");
    // Leftovers of interrupted uploads (chunks keep updating the mtime).
    if let Ok(rd) = fs::read_dir(store.join("incoming")) {
        for e in rd.filter_map(|e| e.ok()) {
            if e.metadata().map(|m| age(&m) > Duration::from_secs(3600)).unwrap_or(false) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    let mut blobs = Vec::new();
    let mut total = 0u64;
    for shard in fs::read_dir(store.join("sha256")).into_iter().flatten().filter_map(|e| e.ok()) {
        for e in fs::read_dir(shard.path()).into_iter().flatten().filter_map(|e| e.ok()) {
            let Ok(md) = e.metadata() else { continue };
            total += md.len();
            blobs.push((md.modified().unwrap_or(SystemTime::UNIX_EPOCH), md.len(), md.nlink(), e.path()));
        }
    }
    report.store_bytes = total;
    if total <= policy.store_size {
        return;
    }
    let Some(_lock) = lock(&store_lock_path(root), libc::LOCK_EX | libc::LOCK_NB) else {
        report.store_busy = true;
        return;
    };
    blobs.sort();
    let now = SystemTime::now();
    for (used, size, links, path) in blobs {
        if total <= policy.store_size {
            break;
        }
        let recent = now.duration_since(used).unwrap_or_default() < policy.blob_age;
        if recent || links > 1 {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            total -= size;
            report.blobs += 1;
            report.blob_bytes += size;
            if let Some(shard) = path.parent() {
                let _ = fs::remove_dir(shard); // only succeeds once empty
            }
        }
    }
    report.store_bytes = total;
}

/// Executables of running processes (that we can see), to never remove an
/// agent build that is in use: it re-executes itself for the sandbox.
fn running_executables() -> HashSet<PathBuf> {
    let mut exes = HashSet::new();
    for e in fs::read_dir("/proc").into_iter().flatten().filter_map(|e| e.ok()) {
        if let Ok(exe) = fs::read_link(e.path().join("exe")) {
            exes.insert(exe);
        }
    }
    exes
}

/// Removes agent builds (`<root>/agent/<id>/`) unused for `cache_age`.
fn collect_agents(root: &Path, policy: &Policy, report: &mut Report) {
    let dir = root.join("agent");
    let Ok(rd) = fs::read_dir(&dir) else { return };
    let me = std::env::current_exe().ok();
    let running = running_executables();
    for e in rd.filter_map(|e| e.ok()) {
        let path = e.path();
        let exe = path.join("xtest-agent");
        if me.as_deref().is_some_and(|m| m.starts_with(&path)) || running.contains(&exe) {
            continue;
        }
        let md = fs::metadata(&exe).or_else(|_| e.metadata());
        if md.map(|m| age(&m) >= policy.cache_age).unwrap_or(false) && remove_tree(&path).is_ok() {
            report.agents += 1;
        }
    }
}

/// Removes cached tools (`<root>/tools/<tool>/<id>/`) unused for `cache_age`.
/// A binary hardlinked into a live run has more than one link.
fn collect_tools(root: &Path, policy: &Policy, report: &mut Report) {
    for tool in fs::read_dir(root.join("tools")).into_iter().flatten().filter_map(|e| e.ok()) {
        for e in fs::read_dir(tool.path()).into_iter().flatten().filter_map(|e| e.ok()) {
            let mut files = fs::read_dir(e.path()).into_iter().flatten().filter_map(|f| f.ok());
            let in_use = files.any(|f| {
                let Ok(md) = f.metadata() else { return false };
                let tmp = f.file_name().to_string_lossy().contains(".tmp.");
                // A download in progress, or a binary used recently or by a live run.
                md.nlink() > 1 || age(&md) < if tmp { Duration::from_secs(3600) } else { policy.cache_age }
            });
            if !in_use && remove_tree(&e.path()).is_ok() {
                report.tools += 1;
            }
        }
    }
}

/// Where the agent records that it used a built image.
pub fn image_marker(root: &Path, tag: &str) -> PathBuf {
    root.join("images").join(tag.replace(['/', ':'], "_"))
}

/// Removes images built by agents of this root and unused for `cache_age`.
/// Images still used by a container can't be removed and are kept.
fn collect_images(root: &Path, policy: &Policy, report: &mut Report) {
    let out = Command::new("docker")
        .args(["image", "ls", "--filter"])
        .arg(format!("label=xtest.root={}", root.display()))
        .args(["--format", "{{.Repository}}:{{.Tag}}"])
        .output();
    let Ok(out) = out else { return };
    let mut known = HashSet::new();
    for tag in String::from_utf8_lossy(&out.stdout).lines().filter(|t| !t.contains("<none>")) {
        let marker = image_marker(root, tag);
        known.insert(marker.clone());
        let unused = match fs::metadata(&marker) {
            Ok(md) => age(&md) >= policy.cache_age,
            Err(_) => {
                // Built before markers existed: start counting now.
                let _ = fs::create_dir_all(marker.parent().unwrap());
                let _ = File::create(&marker);
                policy.cache_age.is_zero()
            }
        };
        if unused {
            let removed = Command::new("docker").args(["image", "rm", tag]).output();
            if removed.is_ok_and(|o| o.status.success()) {
                let _ = fs::remove_file(&marker);
                report.images += 1;
            }
        }
    }
    // Markers of images that were removed some other way.
    for e in fs::read_dir(root.join("images")).into_iter().flatten().filter_map(|e| e.ok()) {
        if !known.contains(&e.path()) {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn collect_k3d(root: &Path, dead: &dyn Fn(&str) -> bool) {
    let out = Command::new("docker")
        .args(["ps", "-a", "--filter", "label=app=k3d", "--filter"])
        .arg(format!("label=xtest.root={}", root.display()))
        .args(["--format", "{{.Label \"k3d.cluster\"}} {{.Label \"xtest.run\"}}"])
        .output();
    let Ok(out) = out else { return };
    let mut clusters: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, run)| dead(run))
        .map(|(cluster, _)| cluster.to_string())
        .collect();
    clusters.sort();
    clusters.dedup();
    for c in clusters {
        if Command::new("k3d").args(["cluster", "delete", &c]).output().is_err() {
            eprintln!("xtest-agent: k3d cluster {c} is left over, but k3d is not installed");
        }
    }
}

/// Shared networks of dead runs (after their clusters are deleted).
fn collect_networks(root: &Path, dead: &dyn Fn(&str) -> bool) {
    let out = Command::new("docker")
        .args(["network", "ls", "--filter"])
        .arg(format!("label=xtest.root={}", root.display()))
        .args(["--format", "{{.Name}} {{.Label \"xtest.run\"}}"])
        .output();
    let Ok(out) = out else { return };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((name, run)) = line.split_once(' ') {
            if dead(run) {
                let _ = Command::new("docker").args(["network", "rm", name]).output();
            }
        }
    }
}

fn remove_containers(filter: &str) {
    let Ok(out) = Command::new("docker").args(["ps", "-aq", "--filter", filter]).output() else {
        return;
    };
    let ids: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(String::from)
        .collect();
    if !ids.is_empty() {
        let _ = Command::new("docker").args(["rm", "-f"]).args(&ids).output();
    }
}

/// `remove_dir_all` that also copes with read-only directories made by tests.
/// Only directories are chmod'ed: files may be hardlinks into the store.
pub fn remove_tree(dir: &Path) -> std::io::Result<()> {
    if fs::remove_dir_all(dir).is_ok() || !dir.exists() {
        return Ok(());
    }
    fn make_writable(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    make_writable(&e.path());
                }
            }
        }
    }
    make_writable(dir);
    fs::remove_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use sha2::{Digest, Sha256};
    use xtest_proto::Hash;

    fn put(store: &Store, data: &[u8], days_ago: u64) -> Hash {
        let hash = Hash(Sha256::digest(data).into());
        store.write_chunk(&hash, 0, data, true).unwrap();
        let t = SystemTime::now() - Duration::from_secs(days_ago * 24 * 3600);
        File::open(store.blob_path(&hash)).unwrap().set_modified(t).unwrap();
        hash
    }

    #[test]
    fn evicts_least_recently_used_blobs_down_to_the_cap() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let oldest = put(&store, &[1; 100], 9);
        let linked = put(&store, &[2; 100], 8);
        let older = put(&store, &[3; 100], 7);
        let recent = put(&store, &[4; 100], 0);
        // A live run has this one materialized.
        fs::hard_link(store.blob_path(&linked), root.path().join("link")).unwrap();

        let policy = Policy { store_size: 250, ..Policy::startup(0) };
        let report = collect(root.path(), false, &policy);
        assert_eq!((report.blobs, report.blob_bytes, report.store_bytes), (2, 200, 200));
        assert!(!store.has(&oldest) && !store.has(&older));
        assert!(store.has(&linked) && store.has(&recent));

        // Recently used blobs stay even when the store is over its cap.
        let report = collect(root.path(), false, &Policy { store_size: 0, ..Policy::startup(0) });
        assert_eq!(report.blobs, 0);
        assert!(store.has(&recent));
    }

    #[test]
    fn leaves_blobs_alone_while_a_run_is_syncing() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::open(root.path()).unwrap();
        let hash = put(&store, b"x", 30);
        let _shared = lock(&store_lock_path(root.path()), libc::LOCK_SH).unwrap();
        let report = collect(root.path(), false, &Policy::everything());
        assert!(report.store_busy && store.has(&hash));
    }

    #[test]
    fn kept_runs_are_removed_only_when_asked() {
        let root = tempfile::tempdir().unwrap();
        Store::open(root.path()).unwrap();
        let run = root.path().join("runs/1-2");
        fs::create_dir_all(&run).unwrap();
        File::create(run.join("lock")).unwrap();
        File::create(run.join("keep")).unwrap();
        assert_eq!(collect(root.path(), false, &Policy::startup(DEFAULT_STORE_SIZE)).runs, 0);
        assert_eq!(collect(root.path(), false, &Policy::everything()).runs, 1);
        assert!(!run.exists());
    }

    #[test]
    fn removes_unused_agent_builds_and_tools() {
        let root = tempfile::tempdir().unwrap();
        let old = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        for (dir, file) in [("agent/aaaa", "xtest-agent"), ("agent/bbbb", "xtest-agent"), ("tools/k3s/cccc", "k3s"), ("tools/k3s/dddd", "k3s")] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
            File::create(root.path().join(dir).join(file)).unwrap();
        }
        File::open(root.path().join("agent/aaaa/xtest-agent")).unwrap().set_modified(old).unwrap();
        File::open(root.path().join("tools/k3s/cccc/k3s")).unwrap().set_modified(old).unwrap();
        let report = collect(root.path(), false, &Policy::startup(DEFAULT_STORE_SIZE));
        assert_eq!((report.agents, report.tools), (1, 1));
        assert!(!root.path().join("agent/aaaa").exists() && root.path().join("agent/bbbb").exists());
        assert!(!root.path().join("tools/k3s/cccc").exists() && root.path().join("tools/k3s/dddd").exists());
    }
}
