//! Connection to one agent, local or over SSH.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use xtest_proto::*;

use crate::config::Host;

pub enum ProcEvent {
    Output(Stream, Vec<u8>),
    Exited(Exit, Duration, Option<u64>),
}

pub struct RunResult {
    pub exit: Exit,
    pub output: Output,
    pub duration: Duration,
    pub peak_memory: Option<u64>,
}

/// A process's output in the order it arrived, as runs of one stream.
#[derive(Debug, Default, Clone)]
pub struct Output(pub Vec<(Stream, Vec<u8>)>);

impl Output {
    fn push(&mut self, stream: Stream, data: &[u8]) {
        match self.0.last_mut() {
            Some((s, buf)) if *s == stream => buf.extend_from_slice(data),
            _ => self.0.push((stream, data.to_vec())),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|(_, d)| d.is_empty())
    }

    /// Everything written to `stream`.
    pub fn of(&self, stream: Stream) -> Vec<u8> {
        self.0.iter().filter(|(s, _)| *s == stream).flat_map(|(_, d)| d.iter().copied()).collect()
    }

    /// Both streams, interleaved as they arrived.
    pub fn all(&self) -> Vec<u8> {
        self.0.iter().flat_map(|(_, d)| d.iter().copied()).collect()
    }
}

#[derive(Default)]
struct Routes {
    pending: HashMap<u64, oneshot::Sender<Result<Reply, String>>>,
    procs: HashMap<u64, mpsc::UnboundedSender<ProcEvent>>,
}

pub struct Agent {
    pub name: String,
    pub info: HostInfo,
    tx: mpsc::UnboundedSender<ClientMsg>,
    routes: Arc<Mutex<Routes>>,
    next: AtomicU64,
    child: tokio::sync::Mutex<Child>,
}

/// The agent binary that ships with this controller.
fn local_agent_binary(host: &Host) -> Result<PathBuf> {
    if let Some(p) = &host.agent {
        return Ok(p.clone());
    }
    let exe = std::env::current_exe()?;
    let p = exe.with_file_name("xtest-agent");
    if !p.is_file() {
        bail!("agent binary not found at {} (set `agent` for host `{}`)", p.display(), host.name);
    }
    Ok(p)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Command that runs the agent's `sub`command on `host` (installing the
/// agent there first if needed), with the host's root and store size.
async fn agent_command(host: &Host, sub: &[&str]) -> Result<Command> {
    let agent_bin = local_agent_binary(host)?;
    let mut extra: Vec<String> = sub.iter().map(|s| s.to_string()).collect();
    if let Some(size) = &host.store_size {
        extra.extend(["--store-size".into(), crate::config::parse_size(size)?.to_string()]);
    }
    Ok(match &host.ssh {
        None => {
            let mut c = Command::new(&agent_bin);
            c.args(&extra);
            if let Some(root) = &host.root {
                c.args(["--root", root]);
            }
            c
        }
        Some(dest) => {
            let dir = install_remote_agent(host, dest, &agent_bin).await?;
            // Plain flags and numbers: no quoting needed (or wanted, see `ssh`).
            debug_assert!(extra.iter().all(|a| a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')));
            ssh(host, dest, &format!("{dir}; exec \"$D/xtest-agent\" {} --root \"$R\"", extra.join(" ")))
        }
    })
}

/// Runs the agent's garbage collector on `host` now and returns its report.
/// With `all`, everything no live run uses is removed, kept runs included.
pub async fn collect_garbage(host: &Host, all: bool) -> Result<String> {
    let sub: &[&str] = if all { &["gc", "--all"] } else { &["gc"] };
    let out = agent_command(host, sub)
        .await?
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("running the agent on `{}`", host.name))?;
    if !out.status.success() {
        bail!("cleaning `{}` failed", host.name);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Agent {
    pub async fn connect(host: &Host) -> Result<Agent> {
        let mut cmd = agent_command(host, &["serve", "--stdio"]).await?;
        // Own process group: a Ctrl-C in the terminal must reach only us, so
        // we can shut the agent down properly.
        cmd.process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd.spawn().with_context(|| format!("starting agent on `{}`", host.name))?;
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel::<ClientMsg>();
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                if write_frame(&mut stdin, &msg).await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await;
        });

        let routes = Arc::new(Mutex::new(Routes::default()));
        let r = routes.clone();
        let name = host.name.clone();
        tokio::spawn(async move {
            loop {
                let msg = match read_frame::<_, AgentMsg>(&mut stdout).await {
                    Ok(Some(m)) => m,
                    Ok(None) => break,
                    Err(e) => {
                        eprintln!("error: bad message from agent on `{name}`: {e:#}");
                        break;
                    }
                };
                let mut r = r.lock().unwrap();
                match msg {
                    AgentMsg::Reply { req, result } => {
                        if let Some(tx) = r.pending.remove(&req) {
                            let _ = tx.send(result);
                        }
                    }
                    AgentMsg::Output { req, stream, data } => {
                        if let Some(tx) = r.procs.get(&req) {
                            let _ = tx.send(ProcEvent::Output(stream, data));
                        }
                    }
                    AgentMsg::Exited { req, status, duration, peak_memory } => {
                        if let Some(tx) = r.procs.remove(&req) {
                            let _ = tx.send(ProcEvent::Exited(status, duration, peak_memory));
                        }
                    }
                }
            }
            // Connection gone: wake up everyone waiting.
            let mut r = r.lock().unwrap();
            r.pending.clear();
            r.procs.clear();
        });

        let mut agent = Agent {
            name: host.name.clone(),
            info: HostInfo {
                version: 0,
                os: String::new(),
                arch: String::new(),
                cpus: 1,
                run_dir: PathBuf::new(),
                mirroring: false,
                docker: false,
            },
            tx,
            routes,
            next: AtomicU64::new(1),
            child: tokio::sync::Mutex::new(child),
        };
        match agent.request(Request::Hello { version: VERSION }).await? {
            Reply::Hello(info) => agent.info = info,
            other => bail!("unexpected hello reply: {other:?}"),
        }
        Ok(agent)
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Sends a request right away; the returned future waits for the reply.
    pub fn start(&self, body: Request) -> impl std::future::Future<Output = Result<Reply>> + 'static {
        let req = self.id();
        let (tx, rx) = oneshot::channel();
        self.routes.lock().unwrap().pending.insert(req, tx);
        let sent = self.tx.send(ClientMsg { req, body }).is_ok();
        let name = self.name.clone();
        async move {
            let lost = || anyhow!("connection to `{name}` lost");
            if !sent {
                return Err(lost());
            }
            rx.await.map_err(|_| lost())?.map_err(|e| anyhow!("{e}"))
        }
    }

    pub async fn request(&self, body: Request) -> Result<Reply> {
        self.start(body).await
    }

    pub fn spawn(&self, spec: SpawnSpec) -> (u64, mpsc::UnboundedReceiver<ProcEvent>) {
        let req = self.id();
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.lock().unwrap().procs.insert(req, tx);
        let _ = self.tx.send(ClientMsg { req, body: Request::Spawn(spec) });
        (req, rx)
    }

    /// Runs a process to completion and returns its exit and combined output.
    pub async fn run(&self, spec: SpawnSpec) -> Result<RunResult> {
        self.run_tracked(spec, |_| {}, |_, _| {}).await
    }

    /// Like `run`; tells `started` the request id the agent knows the process
    /// by, and passes each piece of output to `on_output` as it arrives.
    pub async fn run_tracked(
        &self,
        spec: SpawnSpec,
        started: impl FnOnce(u64),
        mut on_output: impl FnMut(Stream, &[u8]),
    ) -> Result<RunResult> {
        let (req, mut rx) = self.spawn(spec);
        started(req);
        let mut output = Output::default();
        while let Some(ev) = rx.recv().await {
            match ev {
                ProcEvent::Output(stream, data) => {
                    on_output(stream, &data);
                    output.push(stream, &data);
                }
                ProcEvent::Exited(exit, duration, peak_memory) => {
                    return Ok(RunResult { exit, output, duration, peak_memory })
                }
            }
        }
        bail!("connection to `{}` lost", self.name)
    }

    pub async fn memory(&self) -> Result<MemoryInfo> {
        match self.request(Request::Memory).await? {
            Reply::Memory(m) => Ok(m),
            other => bail!("unexpected reply to Memory: {other:?}"),
        }
    }

    pub async fn shutdown(&self, keep: Vec<u64>) -> Result<()> {
        let res = tokio::time::timeout(Duration::from_secs(60), self.request(Request::Shutdown { keep })).await;
        let mut child = self.child.lock().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
        match res {
            Ok(r) => r.map(|_| ()),
            Err(_) => bail!("agent on `{}` did not shut down in time", self.name),
        }
    }
}

/// Runs a POSIX shell script on the host. The script goes through `sh -c`
/// because the user's login shell may be anything (fish, nu, ...).
fn ssh(host: &Host, dest: &str, script: &str) -> Command {
    // Inside single quotes, fish still treats `\\` and `\'` as escapes.
    debug_assert!(!script.contains('\\'), "remote scripts must not contain backslashes");
    let program = host
        .ssh_program
        .clone()
        .unwrap_or_else(|| vec!["ssh".into(), "-o".into(), "BatchMode=yes".into()]);
    let mut c = Command::new(&program[0]);
    c.args(&program[1..]).arg(dest).arg(format!("sh -c {}", sh_quote(script)));
    c
}

/// Copies the agent to the host unless that exact build is already there.
/// Returns a shell prefix that sets `$R` (the root) and `$D` (the agent's directory).
async fn install_remote_agent(host: &Host, dest: &str, agent_bin: &Path) -> Result<String> {
    let hash = xtest_agent::store::hash_file(agent_bin)?.to_hex();
    let root = match &host.root {
        Some(r) => sh_quote(r),
        None => "\"${XDG_CACHE_HOME:-$HOME/.cache}/cargo-xtest\"".into(),
    };
    let dir = format!("R={root}; D=\"$R/agent/{}\"", &hash[..16]);

    let present = ssh(host, dest, &format!("{dir}; test -x \"$D/xtest-agent\""))
        .stdin(Stdio::null())
        .status()
        .await
        .with_context(|| format!("running ssh for `{}`", host.name))?;
    if !present.success() {
        let script = format!(
            "{dir}; set -e; mkdir -p \"$D\"; cat > \"$D/.upload.$$\"; \
             chmod 755 \"$D/.upload.$$\"; mv \"$D/.upload.$$\" \"$D/xtest-agent\""
        );
        let mut child = ssh(host, dest, &script).stdin(Stdio::piped()).spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&tokio::fs::read(agent_bin).await?).await?;
        drop(stdin);
        if !child.wait().await?.success() {
            bail!("could not install the agent on `{}`", host.name);
        }
    }
    Ok(dir)
}
