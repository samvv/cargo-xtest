//! Removes what earlier agents left behind: run directories whose lock is no
//! longer held, and containers labeled with a run that no longer exists.

use std::fs::{self, File};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Takes an exclusive, non-blocking lock. Returns `None` if someone holds it.
pub fn try_lock(path: &Path) -> Option<File> {
    let f = File::options().create(true).truncate(false).write(true).open(path).ok()?;
    let ok = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    ok.then_some(f)
}

pub fn collect(root: &Path, docker: bool) {
    let runs = root.join("runs");
    let Ok(entries) = fs::read_dir(&runs) else { return };
    for e in entries.filter_map(|e| e.ok()) {
        let dir = e.path();
        let id = e.file_name().to_string_lossy().into_owned();
        if dir.join("keep").exists() {
            continue;
        }
        let lock = dir.join("lock");
        let stale = if lock.exists() {
            try_lock(&lock).is_some()
        } else {
            // A brand-new run creates its lock right after its directory.
            e.metadata()
                .and_then(|m| m.modified())
                .map(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(60))
                .unwrap_or(false)
        };
        if stale {
            if docker {
                remove_containers(&format!("label=xtest.run={id}"));
            }
            if let Err(err) = remove_tree(&dir) {
                eprintln!("xtest-agent: could not remove stale run {}: {err}", dir.display());
            }
        }
    }
    if docker {
        // Containers whose run directory is gone entirely.
        let out = Command::new("docker")
            .args(["ps", "-a", "--filter"])
            .arg(format!("label=xtest.root={}", root.display()))
            .args(["--format", "{{.Names}} {{.Label \"xtest.run\"}}"])
            .output();
        if let Ok(out) = out {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                if let Some((name, run)) = line.split_once(' ') {
                    if !runs.join(run).exists() {
                        let _ = Command::new("docker").args(["rm", "-f", name]).output();
                    }
                }
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
