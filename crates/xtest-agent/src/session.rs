//! One controller connection: a run directory, its processes and its leases.
//! Whatever happens to the connection, `cleanup` tears everything down.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::Command;
use tokio::sync::mpsc;
use xtest_proto::*;

use crate::sandbox::{Mirror, Plan};
use crate::memory::{self, Sampler};
use crate::{fixtures, gc, sandbox, store::Store};

pub struct Session {
    pub root: PathBuf,
    pub run_id: String,
    pub run_dir: PathBuf,
    pub docker: bool,
    mirroring: bool,
    agent_exe: PathBuf,
    store: Store,
    out: mpsc::UnboundedSender<Option<AgentMsg>>,
    state: Mutex<State>,
    /// The glibc each test binary needs, by host path (scanned once).
    glibc_needs: Mutex<HashMap<PathBuf, Option<crate::compat::Glibc>>>,
    /// Shared store lock, held from `HaveBlobs` until `Materialize` is done,
    /// so garbage collection can't evict blobs this run is about to link.
    store_lock: Mutex<Option<fs::File>>,
    /// Ids for what Rust fixtures create (containers, clusters), far above
    /// the controller's lease ids so names never clash.
    next_part: std::sync::atomic::AtomicU64,
}

#[derive(Default)]
struct State {
    mirrors: Vec<PathBuf>,
    /// Running processes by request id.
    procs: HashMap<u64, Proc>,
    leases: HashMap<u64, Lease>,
}

struct Proc {
    pgid: i32,
    /// Set when the process runs in a container: its memory is measured there.
    cgroup: Option<PathBuf>,
    /// The container is used by this process only, so all of it counts.
    exclusive: bool,
}

pub struct Lease {
    pub kind: LeaseKind,
    pub env: BTreeMap<String, String>,
    /// Set for environment leases: processes using it run in this container.
    pub container: Option<Container>,
    /// Directories put in front of `PATH` (they must be visible in
    /// environment containers too, so they live under the run's tmp dir).
    pub path: Vec<PathBuf>,
    /// The fixture's own memory (its containers, not the tests in them).
    pub memory: Option<Arc<memory::FixtureMemory>>,
    /// A Rust fixture's id and value, handed to the tests that use it.
    pub value: Option<(String, serde_json::Value)>,
}

impl Lease {
    pub fn new(kind: LeaseKind) -> Lease {
        Lease { kind, env: BTreeMap::new(), container: None, path: vec![], memory: None, value: None }
    }
}

#[derive(Clone)]
pub struct Container {
    pub name: String,
    pub image: String,
    /// The image's C library, to check test binaries against.
    pub libc: crate::compat::Libc,
    pub cgroup: Option<PathBuf>,
    /// Only one test uses it, so the container's peak is that test's peak.
    pub exclusive: bool,
    /// `PATH` inside the container, so leases can extend it.
    pub path: String,
}

pub enum LeaseKind {
    Container(String),
    /// A k3d cluster, and the runner container tests use it from (environment role).
    K3d { cluster: String, runner: Option<String> },
    Command { teardown: Option<String>, cwd: PathBuf },
    /// A Rust fixture and everything it asked the agent for.
    Rust(Box<crate::rust_fixture::RustLease>),
    /// A process a Rust fixture started; killed on teardown.
    Process(tokio::process::Child),
    /// A command saved under `<run>/cleanup/` that runs on teardown, or
    /// during garbage collection if the run died.
    Cleanup(PathBuf),
}

pub async fn serve(root: PathBuf, store_size: u64) -> Result<()> {
    // The controller decides when we stop; a terminal Ctrl-C or a dropped
    // SSH session must not skip cleanup.
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    fs::create_dir_all(root.join("runs"))?;
    let root = root.canonicalize()?;
    let docker = std::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    // Mark this agent build as used, so other agents' GC keeps it.
    if let Ok(exe) = std::env::current_exe() {
        gc::touch(&exe);
    }
    gc::collect(&root, docker, &gc::Policy::startup(store_size));

    let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
    let run_id = format!("{secs}-{}", std::process::id());
    let run_dir = root.join("runs").join(&run_id);
    fs::create_dir_all(&run_dir)?;
    let _lock = gc::try_lock(&run_dir.join("lock")).context("locking run directory")?;
    fs::create_dir_all(run_dir.join("root"))?;
    fs::create_dir_all(run_dir.join("tmp"))?;

    let agent_exe = std::env::current_exe()?;
    let mirroring = sandbox::probe(&agent_exe);

    let (tx, mut rx) = mpsc::unbounded_channel::<Option<AgentMsg>>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(Some(msg)) = rx.recv().await {
            if write_frame(&mut stdout, &msg).await.is_err() {
                break;
            }
        }
    });

    let s = Arc::new(Session {
        store: Store::open(&root)?,
        root,
        run_id,
        run_dir,
        docker,
        mirroring,
        agent_exe,
        out: tx,
        state: Mutex::default(),
        store_lock: Mutex::default(),
        glibc_needs: Mutex::default(),
        next_part: std::sync::atomic::AtomicU64::new(1_000_000),
    });

    let mut stdin = tokio::io::stdin();
    let shutdown = loop {
        let msg = match read_frame::<_, ClientMsg>(&mut stdin).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("xtest-agent: bad frame from controller: {e:#}");
                None
            }
        };
        let Some(ClientMsg { req, body }) = msg else { break None };
        match body {
            Request::Shutdown { keep, failed } => break Some((req, keep, failed)),
            body => s.clone().handle(req, body).await,
        }
    };

    let (keep, failed) = shutdown.as_ref().map(|(_, k, f)| (k.clone(), f.clone())).unwrap_or_default();
    let reports = s.cleanup(&keep, &failed).await;
    if let Some((req, _, _)) = shutdown {
        s.reply(req, Ok(Reply::ShutDown { reports }));
    }
    let _ = s.out.send(None);
    let _ = writer.await;
    Ok(())
}

impl Session {
    fn send(&self, msg: AgentMsg) {
        let _ = self.out.send(Some(msg));
    }

    fn reply(&self, req: u64, result: Result<Reply, String>) {
        self.send(AgentMsg::Reply { req, result });
    }

    async fn handle(self: Arc<Self>, req: u64, body: Request) {
        let err = |e: anyhow::Error| format!("{e:#}");
        match body {
            Request::Hello { version } => {
                let res = if version != VERSION {
                    Err(format!("protocol mismatch: controller {version}, agent {VERSION}"))
                } else {
                    Ok(Reply::Hello(HostInfo {
                        version: VERSION,
                        os: std::env::consts::OS.into(),
                        arch: std::env::consts::ARCH.into(),
                        cpus: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
                        run_dir: self.run_dir.clone(),
                        mirroring: self.mirroring,
                        docker: self.docker,
                        glibc: crate::compat::running_glibc().map(|g| (g.0, g.1)),
                        disk_free: disk_free(&self.root),
                    }))
                };
                self.reply(req, res);
            }
            Request::HaveBlobs { hashes } => {
                {
                    let mut held = self.store_lock.lock().unwrap();
                    if held.is_none() {
                        // Blocks only while another agent's GC is evicting blobs.
                        *held = gc::lock(&gc::store_lock_path(&self.root), libc::LOCK_SH);
                    }
                }
                let hashes = hashes.into_iter().filter(|h| !self.store.use_blob(h)).collect();
                self.reply(req, Ok(Reply::Missing { hashes }));
            }
            Request::PutBlob { hash, offset, data, last } => {
                let res = self.store.write_chunk(&hash, offset, &data, last);
                self.reply(req, res.map(|_| Reply::Ok).map_err(err));
            }
            Request::Materialize { mirrors, files } => {
                let res = self.materialize(mirrors, &files);
                // Every blob is linked or copied now (or never will be).
                self.store_lock.lock().unwrap().take();
                self.reply(req, res.map(|_| Reply::Ok).map_err(err));
            }
            Request::Spawn(spec) => {
                tokio::spawn(async move { self.run(req, spec).await });
            }
            Request::Kill { id } => {
                if let Some(p) = self.state.lock().unwrap().procs.get(&id) {
                    kill_group(p.pgid);
                }
                self.reply(req, Ok(Reply::Ok));
            }
            Request::Memory => {
                let res = self.memory();
                self.reply(req, res.map(Reply::Memory).map_err(|e| e.to_string()));
            }
            Request::Setup { lease, name, role, spec, exclusive, deps } => {
                tokio::spawn(async move {
                    let res = fixtures::setup(&self, lease, &name, role, spec, exclusive, &deps).await;
                    self.reply(req, res.map_err(err));
                });
            }
            Request::Teardown { lease, failed } => {
                tokio::spawn(async move {
                    let l = self.state.lock().unwrap().leases.remove(&lease);
                    let mut peak = None;
                    let mut report = TeardownReport::default();
                    if let Some(l) = l {
                        peak = l.memory.as_ref().map(|m| m.sample().peak).filter(|p| *p > 0);
                        report = fixtures::teardown(&self, l, failed).await;
                    }
                    self.reply(req, Ok(Reply::TornDown { peak, report }));
                });
            }
            Request::Shutdown { .. } => unreachable!("handled by the read loop"),
        }
    }

    /// Where `path` (as the tests see it) lives on this host.
    pub fn host_path(&self, path: &Path) -> PathBuf {
        self.run_dir.join("root").join(path.strip_prefix("/").unwrap_or(path))
    }

    pub fn mirrors(&self) -> Vec<PathBuf> {
        self.state.lock().unwrap().mirrors.clone()
    }

    pub fn add_lease(&self, id: u64, lease: Lease) {
        self.state.lock().unwrap().leases.insert(id, lease);
    }

    /// A fresh id for something a Rust fixture creates.
    pub fn next_part(&self) -> u64 {
        self.next_part.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    /// The values of the Rust fixtures among `leases`, by fixture id.
    pub fn fixture_values(&self, leases: &[u64]) -> Result<serde_json::Map<String, serde_json::Value>> {
        let state = self.state.lock().unwrap();
        let mut out = serde_json::Map::new();
        for id in leases {
            let l = state.leases.get(id).with_context(|| format!("unknown lease {id}"))?;
            if let Some((fixture, value)) = &l.value {
                out.insert(fixture.clone(), value.clone());
            }
        }
        Ok(out)
    }

    fn memory(&self) -> std::io::Result<MemoryInfo> {
        let (available, total) = memory::meminfo()?;
        let procs: Vec<(u64, i32, Option<PathBuf>, bool)> = self
            .state
            .lock()
            .unwrap()
            .procs
            .iter()
            .map(|(r, p)| (*r, p.pgid, p.cgroup.clone(), p.exclusive))
            .collect();
        let pgids = procs.iter().filter(|p| p.2.is_none()).map(|p| p.1).collect();
        let rss = memory::pgroup_rss(&pgids);
        let running = procs
            .into_iter()
            .map(|(req, pgid, cgroup, exclusive)| {
                let used = match (cgroup, exclusive) {
                    (Some(cg), true) => memory::read_u64(&cg.join("memory.current")).unwrap_or(0),
                    (Some(cg), false) => memory::tagged_rss(&cg, req),
                    (None, _) => rss.get(&pgid).copied().unwrap_or(0),
                };
                (req, used)
            })
            .collect();
        let fixtures = self
            .state
            .lock()
            .unwrap()
            .leases
            .iter()
            .filter_map(|(id, l)| Some((*id, l.memory.as_ref()?.usage())))
            .collect();
        Ok(MemoryInfo { available, total, running, fixtures })
    }

    fn materialize(&self, mirrors: Vec<PathBuf>, files: &[FileEntry]) -> Result<()> {
        for m in &mirrors {
            fs::create_dir_all(self.host_path(m))?;
        }
        for f in files {
            let blob = self.store.blob_path(&f.hash);
            let dst = self.host_path(&f.path);
            fs::create_dir_all(dst.parent().unwrap())?;
            match fs::remove_file(&dst) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
                _ => {}
            }
            if f.link && fs::hard_link(&blob, &dst).is_ok() {
                continue;
            }
            fs::copy(&blob, &dst).with_context(|| format!("materializing {}", f.path.display()))?;
            // Copies are private to this run, so tests may write to them.
            fs::set_permissions(&dst, fs::Permissions::from_mode((f.mode & 0o777) | 0o200))?;
        }
        self.state.lock().unwrap().mirrors = mirrors;
        Ok(())
    }

    /// Path as the process will see it: unchanged when mirroring works (or
    /// in a container), otherwise redirected into the sandbox.
    fn view(&self, path: &Path, in_container: bool) -> PathBuf {
        if self.mirroring || in_container {
            return path.to_owned();
        }
        let inside = self.mirrors().iter().any(|m| path.starts_with(m));
        if inside { self.host_path(path) } else { path.to_owned() }
    }

    pub fn build_command(
        &self,
        exe: &Path,
        args: &[String],
        cwd: &Path,
        env: &BTreeMap<String, String>,
        container: Option<&str>,
    ) -> Command {
        let mut cmd;
        if let Some(ctr) = container {
            cmd = Command::new("docker");
            cmd.arg("exec").arg("-w").arg(cwd);
            for (k, v) in env {
                cmd.arg("-e").arg(format!("{k}={v}"));
            }
            cmd.arg(ctr).arg(exe).args(args);
        } else if self.mirroring {
            let plan = Plan {
                mirrors: self
                    .mirrors()
                    .into_iter()
                    .map(|dst| Mirror { src: self.host_path(&dst), dst })
                    .collect(),
                cwd: cwd.to_owned(),
            };
            cmd = Command::new(&self.agent_exe);
            cmd.arg("sandbox-exec")
                .arg(serde_json::to_string(&plan).unwrap())
                .arg("--")
                .arg(exe)
                .args(args)
                .envs(env);
        } else {
            cmd = Command::new(self.view(exe, false));
            cmd.args(args).current_dir(self.view(cwd, false)).envs(env);
        }
        cmd.process_group(0).stdin(Stdio::null());
        cmd
    }

    /// The env vars and environment container for a process using `leases`.
    /// `base` is the process's own env, used to extend `PATH`.
    pub fn lease_env(
        &self,
        leases: &[u64],
        base: &BTreeMap<String, String>,
    ) -> Result<(BTreeMap<String, String>, Option<Container>)> {
        let state = self.state.lock().unwrap();
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let mut container: Option<Container> = None;
        let mut path = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for id in leases.iter().filter(|id| seen.insert(**id)) {
            let l = state.leases.get(id).with_context(|| format!("unknown lease {id}"))?;
            for (k, v) in &l.env {
                match env.get_mut(k) {
                    // Several clusters: kubectl reads every file in the list.
                    Some(prev) if k == "KUBECONFIG" => {
                        if !prev.split(':').any(|p| p == v) {
                            *prev = format!("{prev}:{v}");
                        }
                    }
                    _ => {
                        env.insert(k.clone(), v.clone());
                    }
                }
            }
            path.extend(l.path.iter().map(|p| p.display().to_string()));
            if let Some(c) = &l.container {
                if container.replace(c.clone()).is_some() {
                    bail!("a process can only run in one environment");
                }
            }
        }
        if !path.is_empty() {
            let rest = match &container {
                Some(c) => c.path.clone(),
                None => base.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default(),
            };
            path.push(rest);
            env.insert("PATH".into(), path.join(":"));
        }
        Ok((env, container))
    }

    async fn run(&self, req: u64, spec: SpawnSpec) {
        let start = Instant::now();
        let (status, peak_memory) = self
            .run_inner(req, spec)
            .await
            .unwrap_or_else(|e| (Exit::SpawnFailed(format!("{e:#}")), None));
        self.state.lock().unwrap().procs.remove(&req);
        self.send(AgentMsg::Exited { req, status, duration: start.elapsed(), peak_memory });
    }

    async fn run_inner(&self, req: u64, spec: SpawnSpec) -> Result<(Exit, Option<u64>)> {
        let tmp = self.run_dir.join("tmp").join(req.to_string());
        fs::create_dir_all(&tmp)?;
        let (lease_env, container) = self.lease_env(&spec.leases, &spec.env)?;
        let mut env = spec.env.clone();
        env.extend(lease_env);
        env.insert("TMPDIR".into(), tmp.display().to_string());
        // Lets the agent find this test's processes inside a shared container.
        env.insert("XTEST_REQ".into(), req.to_string());
        env.insert(fx::env::XTEST.into(), "1".into());
        let values = self.fixture_values(&spec.leases)?;
        if !values.is_empty() {
            // Under the run's tmp dir, which environment containers see too.
            let file = tmp.join("xtest-fixtures.json");
            fs::write(&file, serde_json::to_vec(&values)?)?;
            env.insert(fx::env::FIXTURES.into(), file.display().to_string());
        }

        if let Some(c) = &container {
            self.check_libc(&spec.exe, c)?;
        }
        let ctr_name = container.as_ref().map(|c| c.name.as_str());
        let mut cmd = self.build_command(&spec.exe, &spec.args, &spec.cwd, &env, ctr_name);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("spawning {}", spec.exe.display()))?;
        let pid = child.id() as i32;
        let cgroup = container.as_ref().and_then(|c| c.cgroup.clone());
        let exclusive = container.as_ref().is_some_and(|c| c.exclusive);
        self.state.lock().unwrap().procs.insert(req, Proc { pgid: pid, cgroup: cgroup.clone(), exclusive });

        let pumps = [
            pump(child.stdout.take().unwrap(), req, Stream::Stdout, self.out.clone()),
            pump(child.stderr.take().unwrap(), req, Stream::Stderr, self.out.clone()),
        ];
        // Reaped with wait4 below, which also reports peak memory.
        drop(child);
        let sampler = cgroup.clone().filter(|_| !exclusive).map(|cg| Sampler::start(cg, req));

        let mut waiter = tokio::task::spawn_blocking(move || memory::wait4(pid));
        let (waited, timed_out) = match spec.timeout {
            Some(t) => match tokio::time::timeout(t, &mut waiter).await {
                Ok(r) => (r, false),
                Err(_) => {
                    kill_group(pid);
                    (waiter.await, true)
                }
            },
            None => (waiter.await, false),
        };
        let (status, max_rss) = waited.context("waiting for process")??;
        // Stragglers in the group would keep the pipes open.
        kill_group(pid);
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            for p in pumps {
                let _ = p.await;
            }
        })
        .await;
        let _ = gc::remove_tree(&tmp);

        let peak = match (&container, sampler) {
            (None, _) => Some(max_rss),
            // A container of its own: its whole peak is this test's.
            (Some(_), None) if exclusive => cgroup.and_then(|cg| memory::read_u64(&cg.join("memory.peak"))),
            (Some(_), Some(s)) => s.finish().await,
            // No cgroup to look at. Only the `docker exec` client is our
            // child, so its RSS means nothing.
            (Some(_), None) => None,
        };

        let exit = if timed_out {
            Exit::Timeout
        } else if libc::WIFEXITED(status) {
            Exit::Code(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            Exit::Signal(libc::WTERMSIG(status))
        } else {
            Exit::Code(-1)
        };
        Ok((exit, peak))
    }

    async fn cleanup(&self, keep: &[u64], failed: &[u64]) -> Vec<(u64, TeardownReport)> {
        let (procs, mut leases) = {
            let mut st = self.state.lock().unwrap();
            let procs: Vec<i32> = st.procs.values().map(|p| p.pgid).collect();
            (procs, st.leases.drain().collect::<Vec<_>>())
        };
        for pgid in procs {
            kill_group(pgid);
        }
        // Newest first: a fixture is set up after the ones it depends on.
        leases.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
        let mut kept = false;
        let mut reports = Vec::new();
        for (id, lease) in leases {
            if keep.contains(&id) {
                kept = true;
            } else {
                let report = fixtures::teardown(self, lease, failed.contains(&id)).await;
                if !report.is_empty() {
                    reports.push((id, report));
                }
            }
        }
        if self.docker {
            fixtures::remove_networks(self).await;
        }
        if kept {
            let _ = fs::write(
                self.run_dir.join("keep"),
                "Kept for debugging. Delete this file and the next run cleans it up.\n",
            );
        } else if let Err(e) = gc::remove_tree(&self.run_dir) {
            eprintln!("xtest-agent: removing {}: {e}", self.run_dir.display());
        }
        reports
    }
}

fn pump(
    mut r: impl Read + Send + 'static,
    req: u64,
    stream: Stream,
    tx: mpsc::UnboundedSender<Option<AgentMsg>>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = tx.send(Some(AgentMsg::Output { req, stream, data: buf[..n].to_vec() }));
                }
            }
        }
    })
}

impl Session {
    /// Fails with an explanation if `exe` (built on the developer's machine)
    /// can't run in the container's image because of its C library.
    fn check_libc(&self, exe: &Path, c: &Container) -> Result<()> {
        use crate::compat::Libc;
        let host_exe = self.host_path(exe);
        let needed = *self
            .glibc_needs
            .lock()
            .unwrap()
            .entry(host_exe.clone())
            .or_insert_with(|| crate::compat::required_glibc(&host_exe).ok().flatten());
        let Some(needed) = needed else { return Ok(()) };
        let name = exe.file_name().unwrap_or_default().to_string_lossy();
        match c.libc {
            Libc::Glibc(have) if have < needed => bail!(
                "{name} needs glibc {needed} or newer, but image `{}` has glibc {have}: \
                 use an image with a newer glibc, or build the tests for a musl target",
                c.image
            ),
            Libc::Musl => bail!(
                "{name} needs glibc {needed}, but image `{}` uses musl: \
                 use a glibc-based image, or build the tests for a musl target",
                c.image
            ),
            _ => Ok(()),
        }
    }
}

/// Free space for unprivileged users on the filesystem of `path`.
fn disk_free(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return u64::MAX };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return u64::MAX; // unknown: don't block anything
    }
    st.f_bavail as u64 * st.f_frsize as u64
}

pub fn kill_group(pgid: i32) {
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}
