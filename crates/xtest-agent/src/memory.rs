//! Memory accounting: free memory on the host, current usage of running
//! tests, and peak usage once a test is done.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// `(MemAvailable, MemTotal)` in bytes.
pub fn meminfo() -> io::Result<(u64, u64)> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let field = |name: &str| -> io::Result<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
            .ok_or_else(|| io::Error::other(format!("no {name} in /proc/meminfo")))
    };
    Ok((field("MemAvailable:")?, field("MemTotal:")?))
}

/// Resident memory of every process in each of the given process groups.
pub fn pgroup_rss(pgids: &HashSet<i32>) -> HashMap<i32, u64> {
    let mut out = HashMap::new();
    if pgids.is_empty() {
        return out;
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let Ok(dir) = fs::read_dir("/proc") else { return out };
    for e in dir.filter_map(|e| e.ok()) {
        let name = e.file_name();
        let Some(pid) = name.to_str().filter(|n| n.bytes().all(|b| b.is_ascii_digit())) else { continue };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // Fields after the command name, which is in parentheses and may contain spaces:
        // state ppid pgrp ...
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { continue };
        let Some(pgrp) = rest.split_whitespace().nth(2).and_then(|f| f.parse::<i32>().ok()) else { continue };
        if !pgids.contains(&pgrp) {
            continue;
        }
        let Ok(statm) = fs::read_to_string(format!("/proc/{pid}/statm")) else { continue };
        if let Some(pages) = statm.split_whitespace().nth(1).and_then(|f| f.parse::<u64>().ok()) {
            *out.entry(pgrp).or_default() += pages * page;
        }
    }
    out
}

/// Waits for `pid` and returns its wait status and peak RSS in bytes
/// (the largest of the process and the descendants it waited for).
pub fn wait4(pid: i32) -> io::Result<(i32, u64)> {
    loop {
        let mut status = 0;
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::wait4(pid, &mut status, 0, &mut ru) };
        if r == pid {
            return Ok((status, ru.ru_maxrss as u64 * 1024));
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

pub fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// The cgroup v2 directory of the process `pid`, if it has readable memory accounting.
pub fn cgroup_of(pid: u32) -> Option<PathBuf> {
    let text = fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    let dir = Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    read_u64(&dir.join("memory.current")).map(|_| dir)
}

/// Resident memory of the processes in `cgroup` whose environment contains
/// `XTEST_REQ=<req>`. Separates tests that share one container.
pub fn tagged_rss(cgroup: &Path, req: u64) -> u64 {
    let Ok(procs) = fs::read_to_string(cgroup.join("cgroup.procs")) else { return 0 };
    let tag = format!("XTEST_REQ={req}");
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut total = 0;
    for pid in procs.split_whitespace() {
        let Ok(env) = fs::read(format!("/proc/{pid}/environ")) else { continue };
        if !env.split(|b| *b == 0).any(|var| var == tag.as_bytes()) {
            continue;
        }
        let Ok(statm) = fs::read_to_string(format!("/proc/{pid}/statm")) else { continue };
        total += statm.split_whitespace().nth(1).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0) * page;
    }
    total
}

/// Polls the memory of one test's processes inside a shared container and
/// keeps the highest value.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<Option<u64>>,
}

impl Sampler {
    pub fn start(cgroup: PathBuf, req: u64) -> Sampler {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let task = tokio::spawn(async move {
            let mut max = 0;
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(50)).await;
                max = max.max(tagged_rss(&cgroup, req));
            }
            (max > 0).then_some(max)
        });
        Sampler { stop, task }
    }

    pub async fn finish(self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        self.task.await.ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_meminfo() {
        let (avail, total) = meminfo().unwrap();
        assert!(avail > 0 && avail <= total);
    }

    #[test]
    #[allow(clippy::zombie_processes)] // reaped by wait4
    fn measures_peak_rss() {
        // dd holds one 64 MiB block in memory.
        let child = std::process::Command::new("sh")
            .args(["-c", "dd if=/dev/zero bs=64M count=1 of=/dev/null 2>/dev/null"])
            .spawn()
            .unwrap();
        let (status, peak) = wait4(child.id() as i32).unwrap();
        assert_eq!(status, 0);
        assert!(peak >= 64 << 20, "peak {peak}");
    }

    #[test]
    fn sums_process_group() {
        let me = unsafe { libc::getpgrp() };
        let rss = pgroup_rss(&[me].into());
        assert!(rss[&me] > 0);
    }
}
