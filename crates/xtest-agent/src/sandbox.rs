//! Makes the run's sandbox visible at the original absolute paths.
//!
//! The agent re-executes itself as `xtest-agent sandbox-exec <plan> -- <cmd>`.
//! That fresh, single-threaded process enters a new user + mount namespace,
//! bind-mounts each mirror over its original path and then execs the test.
//! No root is needed. Capabilities gained in the namespace are dropped by the
//! final exec, because the test runs as a non-root uid.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub mirrors: Vec<Mirror>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mirror {
    /// Real directory in the run's sandbox.
    pub src: PathBuf,
    /// Path the test expects.
    pub dst: PathBuf,
}

/// Entry point of `xtest-agent sandbox-exec`.
pub fn main(args: &[String]) -> ! {
    let result = (|| -> Result<()> {
        if args.first().map(String::as_str) == Some("--probe") {
            return enter(&Plan { mirrors: vec![], cwd: "/".into() });
        }
        let (plan, cmd) = match args {
            [plan, sep, cmd @ ..] if sep == "--" && !cmd.is_empty() => (plan, cmd),
            _ => bail!("usage: xtest-agent sandbox-exec <plan-json> -- <cmd>..."),
        };
        let plan: Plan = serde_json::from_str(plan).context("bad sandbox plan")?;
        enter(&plan)?;
        let err = Command::new(&cmd[0]).args(&cmd[1..]).exec();
        bail!("exec {}: {err}", cmd[0])
    })();
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("xtest-agent sandbox: {e:#}");
            std::process::exit(127)
        }
    }
}

/// Whether this host lets us create user + mount namespaces.
pub fn probe(agent_exe: &Path) -> bool {
    Command::new(agent_exe)
        .args(["sandbox-exec", "--probe"])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn enter(plan: &Plan) -> Result<()> {
    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    check(unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) }).context("unshare")?;
    fs::write("/proc/self/setgroups", "deny").context("setgroups")?;
    fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n")).context("uid_map")?;
    fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n")).context("gid_map")?;
    mount(None, Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE, None)
        .context("making mounts private")?;
    for m in &plan.mirrors {
        ensure_dir(&m.dst)?;
        mount(Some(&m.src), &m.dst, None, libc::MS_BIND | libc::MS_REC, None)
            .with_context(|| format!("bind {} -> {}", m.src.display(), m.dst.display()))?;
    }
    std::env::set_current_dir(&plan.cwd)
        .with_context(|| format!("chdir {}", plan.cwd.display()))?;
    Ok(())
}

/// Creates `dir`. If that isn't allowed (e.g. `/home/alice` on a host where
/// we're `ci`), the deepest existing ancestor is covered with a tmpfs that
/// holds bind mounts of its original entries, which makes it writable.
fn ensure_dir(dir: &Path) -> Result<()> {
    if fs::create_dir_all(dir).is_ok() {
        return Ok(());
    }
    let mut ancestor = dir;
    while !ancestor.exists() {
        ancestor = ancestor.parent().context("no existing ancestor")?;
    }
    if ancestor == Path::new("/") {
        bail!("cannot mirror {}: would have to cover /", dir.display());
    }
    shadow(ancestor).with_context(|| format!("covering {}", ancestor.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

fn shadow(dir: &Path) -> Result<()> {
    // Keeps the original directory reachable after the tmpfs covers it.
    let orig: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(dir)?;
    let entries: Vec<_> = fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    mount(
        Some(Path::new("tmpfs")),
        dir,
        Some("tmpfs"),
        libc::MS_NOSUID | libc::MS_NODEV,
        Some("mode=0755"),
    )?;
    let fd_dir = PathBuf::from(format!("/proc/self/fd/{}", orig.as_raw_fd()));
    for e in entries {
        let name = e.file_name();
        let src = fd_dir.join(&name);
        let dst = dir.join(&name);
        let res = (|| -> io::Result<()> {
            let ty = e.file_type()?;
            if ty.is_symlink() {
                return std::os::unix::fs::symlink(fs::read_link(&src)?, &dst);
            }
            if ty.is_dir() {
                fs::create_dir(&dst)?;
            } else {
                File::create(&dst)?;
            }
            mount(Some(&src), &dst, None, libc::MS_BIND | libc::MS_REC, None)
                .map_err(io::Error::other)
        })();
        if let Err(err) = res {
            eprintln!("xtest-agent sandbox: could not keep {}: {err}", dst.display());
        }
    }
    // The fd must stay open until the mounts above are done; leaking it is
    // fine because it is close-on-exec.
    std::mem::forget(orig);
    Ok(())
}

fn mount(
    src: Option<&Path>,
    dst: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> Result<()> {
    let cstr = |b: &[u8]| CString::new(b).expect("path contains NUL");
    let src = src.map(|p| cstr(p.as_os_str().as_bytes()));
    let dst = cstr(dst.as_os_str().as_bytes());
    let fstype = fstype.map(|s| cstr(s.as_bytes()));
    let data = data.map(|s| cstr(s.as_bytes()));
    let ptr = |c: &Option<CString>| c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
    check(unsafe {
        libc::mount(
            ptr(&src),
            dst.as_ptr(),
            ptr(&fstype),
            flags,
            ptr(&data) as *const libc::c_void,
        )
    })
}

fn check(ret: libc::c_int) -> Result<()> {
    if ret == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error().into())
    }
}
