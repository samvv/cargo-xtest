//! Fixtures written in Rust (`#[xtest::fixture]`).
//!
//! A fixture's setup is a hidden ignored test in a test binary. The agent
//! runs it with `--exact` and serves its requests on a Unix socket
//! ([`xtest_proto::fx`]): containers, clusters and processes are created by
//! the agent and become *parts* of the fixture's lease, so they are removed
//! like any other fixture, also after a crash. Setup ends when the fixture
//! sends its value. A fixture that serves something (`cx.serve`) keeps its
//! process running until teardown.
//!
//! Teardown runs the fixture's teardown test (if it has one) with the value,
//! stops a serving process, then removes the parts newest first.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use xtest_proto::fx::{self, env as fxenv};
use xtest_proto::*;

use crate::fixtures;
use crate::memory::FixtureMemory;
use crate::session::{kill_group, Lease, LeaseKind, Session};

/// Env var on every process a Rust fixture runs: the run's id.
pub const RUN_TAG: &str = "XTEST_RUN";

/// How long a teardown function may run.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a serving fixture gets to stop after SIGTERM.
const STOP_GRACE: Duration = Duration::from_secs(10);

pub struct RustLease {
    name: String,
    /// The fixture's directory under the run's tmp dir: socket, logs, value.
    dir: PathBuf,
    /// What the fixture asked for, oldest first.
    parts: Vec<Lease>,
    /// The setup process, while it serves (`cx.serve`).
    serving: Option<tokio::process::Child>,
    teardown: Option<TeardownRun>,
}

struct TeardownRun {
    exe: PathBuf,
    test: String,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
}

/// What a setup has created so far.
#[derive(Default)]
struct Built {
    parts: Vec<Lease>,
    exports: BTreeMap<String, String>,
    hints: Vec<String>,
    /// Index of the part tests run in.
    environment: Option<usize>,
}

/// Arguments that run one hidden test of a test binary.
fn test_args(test: &str) -> Vec<String> {
    ["--exact", test, "--include-ignored", "--nocapture", "--test-threads", "1"].map(String::from).to_vec()
}

pub async fn setup(
    s: &Session,
    id: u64,
    name: &str,
    spec: RustSpec,
    exclusive: bool,
    deps: &[u64],
) -> Result<(Lease, Option<String>)> {
    let dir = s.run_dir.join("tmp").join(format!("fx{id}"));
    fs::create_dir_all(dir.join("tmp"))?;
    let sock = dir.join("sock");
    let listener = UnixListener::bind(&sock).with_context(|| format!("listening on {}", sock.display()))?;

    let values = dir.join("deps.json");
    fs::write(&values, serde_json::to_vec(&s.fixture_values(deps)?)?)?;
    let (dep_env, _) = s.lease_env(deps, &spec.env)?;
    let mut env = spec.env.clone();
    env.extend(dep_env);
    env.insert(fxenv::XTEST.into(), "1".into());
    env.insert(fxenv::INSTANCE.into(), format!("{}-{id}", s.run_id));
    // Lets garbage collection find the processes of a run that died.
    env.insert(RUN_TAG.into(), s.run_id.clone());
    env.insert("TMPDIR".into(), dir.join("tmp").display().to_string());
    let mut setup_env = env.clone();
    setup_env.insert(fxenv::MODE.into(), "setup".into());
    setup_env.insert(fxenv::SOCKET.into(), sock.display().to_string());
    setup_env.insert(fxenv::FIXTURES.into(), values.display().to_string());

    let log = dir.join("setup.log");
    let mut cmd = s.build_command(&spec.exe, &test_args(&spec.setup_test), &spec.cwd, &setup_env, None);
    let out = fs::File::create(&log)?;
    cmd.stdout(out.try_clone()?).stderr(out);
    let mut child = tokio::process::Command::from(cmd).spawn().with_context(|| format!("starting {}", spec.exe.display()))?;

    let mut built = Built::default();
    let driven = tokio::time::timeout(spec.timeout, drive(s, id, name, &dir, exclusive, &listener, &mut child, &mut built)).await;
    let _ = fs::remove_file(&sock);
    let result = match driven {
        Ok(r) => r,
        Err(_) => Err(anyhow!("setup didn't finish within {:?}", spec.timeout)),
    }
    .and_then(|(value, serving)| {
        match (spec.environment, built.environment.is_some()) {
            (true, false) => bail!(
                "it is declared as an `environment`, but its setup called neither \
                 `cx.docker_environment` nor `cx.k3d_environment`"
            ),
            (false, true) => bail!("its setup made an environment, but it isn't declared with `#[xtest::fixture(environment)]`"),
            _ => Ok((value, serving)),
        }
    });

    let (value, serving) = match result {
        Ok(r) => r,
        Err(e) => {
            if let Some(pid) = child.id() {
                kill_group(pid as i32);
            }
            let _ = child.wait().await;
            for part in built.parts.into_iter().rev() {
                fixtures::teardown(s, part, false).await;
            }
            let output = fs::read(&log).unwrap_or_default();
            let output = fixtures::tail(&output, &[]);
            return Err(if output.trim().is_empty() { e } else { anyhow!("{e:#}; its output:\n{output}") });
        }
    };

    // The lease: the parts' env and PATH (e.g. KUBECONFIG and kubectl from a
    // cluster), then the fixture's own exports.
    let mut env_out: BTreeMap<String, String> = BTreeMap::new();
    let mut path = Vec::new();
    let mut cgroups = Vec::new();
    for part in &mut built.parts {
        for (k, v) in &part.env {
            match env_out.get_mut(k) {
                Some(prev) if k == "KUBECONFIG" => *prev = format!("{prev}:{v}"),
                _ => {
                    env_out.insert(k.clone(), v.clone());
                }
            }
        }
        path.extend(part.path.iter().cloned());
        // Measured once, for the whole fixture.
        if let Some(m) = part.memory.take() {
            cgroups.extend(m.cgroups().iter().cloned());
        }
    }
    env_out.extend(built.exports);
    let container = built.environment.and_then(|i| built.parts[i].container.clone());
    let memory = (!cgroups.is_empty()).then(|| FixtureMemory::watch(cgroups));

    let mut hints = built.hints;
    hints.push(format!("setup output: {}", log.display()));
    let mut teardown_env = env.clone();
    teardown_env.extend(env_out.clone());
    let lease = Lease {
        env: env_out,
        container,
        path,
        memory,
        value: Some((spec.id.clone(), value)),
        ..Lease::new(LeaseKind::Rust(Box::new(RustLease {
            name: name.to_string(),
            dir,
            parts: built.parts,
            serving: serving.then_some(child),
            teardown: spec.teardown_test.map(|test| TeardownRun { exe: spec.exe, test, cwd: spec.cwd, env: teardown_env }),
        })))
    };
    Ok((lease, Some(hints.join("; "))))
}

/// Serves the fixture's requests until it is ready. Returns its value and
/// whether it keeps serving.
#[allow(clippy::too_many_arguments)]
async fn drive(
    s: &Session,
    id: u64,
    name: &str,
    dir: &Path,
    exclusive: bool,
    listener: &UnixListener,
    child: &mut tokio::process::Child,
    built: &mut Built,
) -> Result<(serde_json::Value, bool)> {
    let stream = tokio::select! {
        r = listener.accept() => r?.0,
        status = child.wait() => {
            bail!("the setup test exited ({}) without starting the fixture; is it built with an older xtest?", status?)
        }
    };
    let (mut rd, mut wr) = stream.into_split();
    loop {
        // The socket closes when the process exits, so this ends too.
        let Some(req) = read_frame::<_, fx::Request>(&mut rd).await? else {
            let status = child.wait().await?;
            bail!("setup failed ({status})");
        };
        let reply = match req {
            fx::Request::Ready { value, serving } => {
                let value: serde_json::Value = serde_json::from_str(&value).context("the fixture's value is not JSON")?;
                write_frame(&mut wr, &fx::Reply::Ok).await?;
                if !serving {
                    let status = child.wait().await?;
                    if !status.success() {
                        bail!("setup failed after it was ready ({status})");
                    }
                }
                return Ok((value, serving));
            }
            req => handle(s, id, name, dir, exclusive, req, built).await.unwrap_or_else(|e| fx::Reply::Error(format!("{e:#}"))),
        };
        write_frame(&mut wr, &reply).await?;
    }
}

async fn handle(
    s: &Session,
    id: u64,
    name: &str,
    dir: &Path,
    exclusive: bool,
    req: fx::Request,
    built: &mut Built,
) -> Result<fx::Reply> {
    let role = |environment: bool| {
        if environment && built.environment.is_some() {
            bail!("a fixture can make only one environment");
        }
        Ok(if environment { Role::Environment } else { Role::Service })
    };
    Ok(match req {
        fx::Request::Docker { spec, environment } => {
            let role = role(environment)?;
            let (lease, hint, ports) = fixtures::docker_setup(s, s.next_part(), name, role, spec, exclusive).await?;
            let LeaseKind::Container(container) = &lease.kind else { unreachable!("docker leases are containers") };
            let reply = fx::Reply::Container { name: container.clone(), ports };
            built.hints.extend(hint);
            add(built, lease, environment);
            reply
        }
        fx::Request::K3d { spec, environment } => {
            let role = role(environment)?;
            let part = s.next_part();
            let cluster = fixtures::k3d_cluster_name(&s.run_id, part);
            let network = spec.network.as_ref().map(|n| fixtures::k3d_network_name(&s.run_id, n));
            let (lease, hint) = fixtures::k3d_setup(s, part, name, role, spec, exclusive).await?;
            let kubeconfig = PathBuf::from(lease.env.get("KUBECONFIG").cloned().unwrap_or_default());
            let vars = fixtures::k3d_vars(&cluster, &kubeconfig, network.as_deref()).await.values;
            let reply = fx::Reply::Cluster { vars, bin: lease.path[0].clone() };
            built.hints.extend(hint);
            add(built, lease, environment);
            reply
        }
        fx::Request::Spawn { exe, args, cwd, mut env } => {
            env.insert(RUN_TAG.into(), s.run_id.clone());
            let log = dir.join(format!("process-{}.log", built.parts.len()));
            let mut cmd = s.build_command(&exe, &args, &cwd, &env, None);
            let out = fs::File::create(&log)?;
            cmd.stdout(out.try_clone()?).stderr(out);
            let child = tokio::process::Command::from(cmd).spawn().with_context(|| format!("starting {}", exe.display()))?;
            let pid = child.id().unwrap_or_default();
            built.parts.push(Lease::new(LeaseKind::Process(child)));
            fx::Reply::Process { pid, log }
        }
        fx::Request::Export { key, value } => {
            built.exports.insert(key, value);
            fx::Reply::Ok
        }
        fx::Request::Cleanup { argv } => {
            if argv.is_empty() {
                bail!("a cleanup command needs at least a program");
            }
            let dir = s.run_dir.join("cleanup");
            fs::create_dir_all(&dir)?;
            let file = dir.join(format!("{id}-{}.json", built.parts.len()));
            fs::write(&file, serde_json::to_vec(&CleanupCmd { argv })?)?;
            built.parts.push(Lease::new(LeaseKind::Cleanup(file)));
            fx::Reply::Ok
        }
        fx::Request::Ready { .. } => unreachable!("handled by `drive`"),
    })
}

fn add(built: &mut Built, lease: Lease, environment: bool) {
    if environment {
        built.environment = Some(built.parts.len());
    }
    built.parts.push(lease);
}

/// Tears a Rust fixture down: its teardown function, then a serving
/// process, then its parts, newest first.
pub async fn teardown(s: &Session, r: RustLease, value: Option<(String, serde_json::Value)>, failed: bool) -> TeardownReport {
    let mut report = TeardownReport::default();
    if let (Some(t), Some((_, value))) = (&r.teardown, &value) {
        if let Err(e) = run_teardown(s, &r.dir, t, value, failed).await {
            report.error = Some(format!("teardown of fixture `{}` failed: {e:#}", r.name));
        }
        if let Ok(entries) = fs::read_dir(r.dir.join("attach")) {
            let mut files: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            files.sort();
            for f in files {
                let name = f.file_name().unwrap_or_default().to_string_lossy().into_owned();
                report.attachments.push((name, fs::read(&f).unwrap_or_default()));
            }
        }
    }
    if let Some(mut child) = r.serving {
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            if tokio::time::timeout(STOP_GRACE, child.wait()).await.is_err() {
                kill_group(pid as i32);
                let _ = child.wait().await;
            }
        }
    }
    for part in r.parts.into_iter().rev() {
        fixtures::teardown(s, part, failed).await;
    }
    report
}

async fn run_teardown(s: &Session, dir: &Path, t: &TeardownRun, value: &serde_json::Value, failed: bool) -> Result<()> {
    let value_file = dir.join("value.json");
    fs::write(&value_file, serde_json::to_vec(value)?)?;
    let attach = dir.join("attach");
    fs::create_dir_all(&attach)?;
    let mut env = t.env.clone();
    env.insert(fxenv::MODE.into(), "teardown".into());
    env.insert(fxenv::VALUE.into(), value_file.display().to_string());
    env.insert(fxenv::FAILED.into(), if failed { "1" } else { "0" }.into());
    env.insert(fxenv::ATTACH_DIR.into(), attach.display().to_string());
    let log = dir.join("teardown.log");
    let mut cmd = s.build_command(&t.exe, &test_args(&t.test), &t.cwd, &env, None);
    let out = fs::File::create(&log)?;
    cmd.stdout(out.try_clone()?).stderr(out).stdin(Stdio::null());
    let mut child = tokio::process::Command::from(cmd).spawn()?;
    let status = match tokio::time::timeout(TEARDOWN_TIMEOUT, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            if let Some(pid) = child.id() {
                kill_group(pid as i32);
            }
            let _ = child.wait().await;
            bail!("timed out after {TEARDOWN_TIMEOUT:?}");
        }
    };
    if !status.success() {
        let output = fs::read(&log).unwrap_or_default();
        bail!("{status}; output:\n{}", fixtures::tail(&output, &[]));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct CleanupCmd {
    argv: Vec<String>,
}

/// Runs a saved cleanup command (on the host, from `/`), then removes it.
/// Also used by garbage collection for runs that died.
pub fn run_cleanup(file: &Path) {
    let Ok(data) = fs::read(file) else { return };
    match serde_json::from_slice::<CleanupCmd>(&data) {
        Ok(CleanupCmd { argv }) if !argv.is_empty() => {
            let out = std::process::Command::new(&argv[0]).args(&argv[1..]).current_dir("/").stdin(Stdio::null()).output();
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => eprintln!(
                    "xtest-agent: cleanup command {argv:?} failed ({}): {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => eprintln!("xtest-agent: cleanup command {argv:?}: {e}"),
            }
        }
        _ => eprintln!("xtest-agent: unreadable cleanup command {}", file.display()),
    }
    let _ = fs::remove_file(file);
}

/// Kills the processes Rust fixtures of the runs `dead` started, found by
/// their [`RUN_TAG`]. They outlive their agent only if it crashed.
pub fn kill_leftover_processes(dead: &[String]) {
    if dead.is_empty() {
        return;
    }
    let tags: Vec<Vec<u8>> = dead.iter().map(|id| format!("{RUN_TAG}={id}").into_bytes()).collect();
    let Ok(entries) = fs::read_dir("/proc") else { return };
    for e in entries.filter_map(|e| e.ok()) {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        // Only our own processes are readable, which is all we may kill anyway.
        let Ok(environ) = fs::read(e.path().join("environ")) else { continue };
        if environ.split(|b| *b == 0).any(|var| tags.iter().any(|t| var == t.as_slice())) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

/// Runs the cleanup commands a dead run left behind.
pub fn run_leftover_cleanups(run_dir: &Path) {
    let Ok(entries) = fs::read_dir(run_dir.join("cleanup")) else { return };
    let mut files: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    // Newest first, as in a normal teardown.
    files.sort();
    for f in files.into_iter().rev() {
        run_cleanup(&f);
    }
}
