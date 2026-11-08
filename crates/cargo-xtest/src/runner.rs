//! Discovering tests, setting up fixtures and spreading tests over hosts.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::sync::OnceCell;
use tokio::task::JoinSet;
use xtest_proto::*;

use crate::cargo::TestBinary;
use crate::client::Agent;
use crate::config::{format_size, Scope};
use crate::files::FileSet;

pub struct FixtureDef {
    pub scope: Scope,
    pub spec: FixtureSpec,
}

/// One test to run.
pub struct Case {
    pub bin: Arc<TestBinary>,
    /// Test name; for `harness = false` binaries, the binary's name.
    pub name: String,
    pub ignored: bool,
    pub run_in: Option<String>,
    pub uses: Vec<String>,
    pub timeout: Option<Duration>,
    /// Estimated memory need in bytes.
    pub memory: u64,
    pub memory_source: MemorySource,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MemorySource {
    /// `memory` set in an override.
    Config,
    /// Peak measured in an earlier run.
    Measured,
    /// Profile default for tests not measured yet.
    Default,
}

impl Case {
    pub fn display(&self) -> String {
        if self.bin.harness {
            format!("{} {}", self.bin.id(), self.name)
        } else {
            self.bin.id()
        }
    }
}

type LeaseKey = (String, bool, String);
type SharedLease = Arc<OnceCell<Result<(u64, Option<String>), String>>>;

pub struct HostState {
    pub agent: Agent,
    pub slots: usize,
    leases: tokio::sync::Mutex<HashMap<LeaseKey, SharedLease>>,
    next_lease: AtomicU64,
    /// Leases kept for debugging, with a message for the user.
    pub kept: Mutex<Vec<(u64, String)>>,
}

impl HostState {
    pub fn new(agent: Agent, slots: usize) -> Self {
        HostState {
            agent,
            slots,
            leases: Default::default(),
            next_lease: AtomicU64::new(1),
            kept: Default::default(),
        }
    }

    async fn setup(&self, def: &FixtureDef, name: &str, role: Role) -> Result<(u64, Option<String>)> {
        let lease = self.next_lease.fetch_add(1, Ordering::Relaxed);
        let exclusive = def.scope == Scope::Test;
        let req = Request::Setup { lease, name: name.into(), role, spec: def.spec.clone(), exclusive };
        match self.agent.request(req).await? {
            Reply::Lease { debug_hint, .. } => Ok((lease, debug_hint)),
            other => bail!("unexpected reply to setup: {other:?}"),
        }
    }

    /// Returns the lease id, and its debug hint if the lease belongs to this test only.
    async fn lease(&self, ctx: &Ctx, name: &str, role: Role, case: &Case) -> Result<(u64, Option<Option<String>>)> {
        let def = &ctx.fixtures[name];
        let scope_key = match def.scope {
            Scope::Test => {
                let (id, hint) = self.setup(def, name, role).await?;
                return Ok((id, Some(hint)));
            }
            Scope::Binary => case.bin.exe.display().to_string(),
            Scope::Run => String::new(),
        };
        let key = (name.to_string(), role == Role::Environment, scope_key);
        let cell = self.leases.lock().await.entry(key).or_default().clone();
        let res = cell
            .get_or_init(|| async { self.setup(def, name, role).await.map_err(|e| format!("{e:#}")) })
            .await;
        match res {
            Ok((id, _)) => Ok((*id, None)),
            Err(e) => bail!("{e}"),
        }
    }
}

pub struct Ctx {
    pub fixtures: HashMap<String, FixtureDef>,
    pub keep_fixtures: bool,
    pub memory_reserve: u64,
    pub reporter: Reporter,
    /// Peak memory measured in this run, by test.
    pub measured: Mutex<BTreeMap<String, u64>>,
}

impl Ctx {
    fn record(&self, host: &str, case: &Case, outcome: &Outcome) {
        if let Some(peak) = outcome.peak_memory {
            self.measured.lock().unwrap().insert(case.display(), peak);
        }
        self.reporter.report(host, case, outcome);
    }
}

pub async fn sync(agent: &Agent, files: &FileSet) -> Result<(usize, u64)> {
    let hashes: BTreeSet<Hash> = files.files.values().map(|f| f.hash).collect();
    let Reply::Missing { hashes: missing } = agent.request(Request::HaveBlobs { hashes: hashes.into_iter().collect() }).await? else {
        bail!("unexpected reply to HaveBlobs");
    };
    let by_hash: HashMap<Hash, &PathBuf> = files.files.values().map(|f| (f.hash, &f.path)).collect();
    let mut bytes = 0;
    for hash in &missing {
        let data = tokio::fs::read(by_hash[hash]).await?;
        bytes += data.len() as u64;
        let mut acks = Vec::new();
        let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(BLOB_CHUNK).collect() };
        let n = chunks.len();
        for (i, chunk) in chunks.into_iter().enumerate() {
            let req = Request::PutBlob {
                hash: *hash,
                offset: (i * BLOB_CHUNK) as u64,
                data: chunk.to_vec(),
                last: i + 1 == n,
            };
            acks.push(agent.start(req));
        }
        for ack in acks {
            ack.await.with_context(|| format!("uploading {}", by_hash[hash].display()))?;
        }
    }
    agent
        .request(Request::Materialize {
            mirrors: files.mirrors.clone(),
            files: files.files.values().cloned().collect(),
        })
        .await
        .context("materializing sandbox")?;
    Ok((missing.len(), bytes))
}

/// Lists the tests in every binary. Returns (binary, test name, ignored).
pub async fn discover(agent: &Agent, bins: &[Arc<TestBinary>]) -> Result<Vec<(Arc<TestBinary>, String, bool)>> {
    let mut out = Vec::new();
    for bin in bins {
        if !bin.harness {
            out.push((bin.clone(), bin.name.clone(), false));
            continue;
        }
        let list = |extra: Option<&str>| {
            let mut args: Vec<String> = vec!["--list".into(), "--format".into(), "terse".into()];
            args.extend(extra.map(String::from));
            SpawnSpec {
                exe: bin.exe.clone(),
                args,
                cwd: bin.manifest_dir.clone(),
                env: bin.env(),
                timeout: Some(Duration::from_secs(60)),
                leases: vec![],
            }
        };
        let parse = |r: crate::client::RunResult| -> Result<Vec<String>> {
            let text = String::from_utf8_lossy(&r.output);
            if !r.exit.success() {
                bail!("listing tests in {} failed ({}):\n{text}", bin.id(), r.exit);
            }
            Ok(text.lines().filter_map(|l| l.strip_suffix(": test")).map(String::from).collect())
        };
        let all = parse(agent.run(list(None)).await?)?;
        let ignored: BTreeSet<String> = parse(agent.run(list(Some("--ignored"))).await?)?.into_iter().collect();
        for name in all {
            let ig = ignored.contains(&name);
            out.push((bin.clone(), name, ig));
        }
    }
    Ok(out)
}

pub async fn run_all(ctx: Arc<Ctx>, hosts: &[Arc<HostState>], mut cases: Vec<Case>) {
    // Biggest first: they are the hardest to fit once memory is shared out.
    // The sort is stable, so equal tests keep their discovery order.
    cases.sort_by_key(|c| std::cmp::Reverse(c.memory));
    let queue = Arc::new(Mutex::new(VecDeque::from(cases)));
    let mut set = JoinSet::new();
    for host in hosts {
        set.spawn(dispatch(ctx.clone(), host.clone(), queue.clone()));
    }
    while set.join_next().await.is_some() {}
}

/// A test started on a host that may not have reached its full memory use yet.
struct Inflight {
    memory: u64,
    /// The agent's id for the test process, once it is spawned.
    req: Option<u64>,
}

/// Starts tests on one host while it has a free slot and enough memory.
///
/// Free memory is `MemAvailable` minus what running tests are still expected
/// to take on top of what they use now (their estimate minus current usage),
/// minus the reserve. A test that fits nowhere still runs once the host is
/// idle, so nothing waits forever.
async fn dispatch(ctx: Arc<Ctx>, host: Arc<HostState>, queue: Arc<Mutex<VecDeque<Case>>>) {
    let inflight: Arc<Mutex<HashMap<usize, Inflight>>> = Default::default();
    let mut running = JoinSet::new();
    let mut next_id = 0usize;
    loop {
        while running.len() < host.slots && !queue.lock().unwrap().is_empty() {
            let mem = host.agent.memory().await.ok();
            let free = mem.as_ref().map(|m| {
                let owed: u64 = inflight
                    .lock()
                    .unwrap()
                    .values()
                    .map(|f| f.memory.saturating_sub(f.req.and_then(|r| m.running.get(&r).copied()).unwrap_or(0)))
                    .sum();
                m.available.saturating_sub(owed).saturating_sub(ctx.memory_reserve)
            });
            let case = {
                let mut q = queue.lock().unwrap();
                let idx = match free {
                    // The agent is unreachable; the test will report that.
                    None => Some(0),
                    Some(free) => q.iter().position(|c| c.memory <= free).or(running.is_empty().then_some(0)),
                };
                idx.and_then(|i| q.remove(i))
            };
            let Some(case) = case else { break };
            if let Some(m) = &mem {
                if case.memory > m.total {
                    ctx.reporter.warn(&format!(
                        "{} needs {} but `{}` only has {} in total",
                        case.display(),
                        format_size(case.memory),
                        host.agent.name,
                        format_size(m.total)
                    ));
                }
            }

            let id = next_id;
            next_id += 1;
            inflight.lock().unwrap().insert(id, Inflight { memory: case.memory, req: None });
            let (ctx, host, inflight) = (ctx.clone(), host.clone(), inflight.clone());
            running.spawn(async move {
                let track = inflight.clone();
                let started = move |req| {
                    if let Some(f) = track.lock().unwrap().get_mut(&id) {
                        f.req = Some(req);
                    }
                };
                let outcome = run_case(&ctx, &host, &case, started).await;
                inflight.lock().unwrap().remove(&id);
                ctx.record(&host.agent.name, &case, &outcome);
            });
        }
        if running.is_empty() {
            if queue.lock().unwrap().is_empty() {
                return;
            }
            continue;
        }
        // Wait for a test to finish, or re-check memory now and then: other
        // processes on the host may free some.
        let waiting = running.len() < host.slots && !queue.lock().unwrap().is_empty();
        tokio::select! {
            _ = running.join_next() => {}
            _ = tokio::time::sleep(Duration::from_millis(250)), if waiting => {}
        }
    }
}

pub struct Outcome {
    pub status: Status,
    pub duration: Duration,
    pub output: Vec<u8>,
    pub peak_memory: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Pass,
    Fail(String),
}

async fn run_case(ctx: &Ctx, host: &HostState, case: &Case, started: impl FnOnce(u64)) -> Outcome {
    let start = Instant::now();
    let mut leases = Vec::new();
    // Test-scoped leases: torn down (or kept) after this test.
    let mut own: Vec<(u64, Option<String>)> = Vec::new();
    let setup: Result<()> = async {
        let wanted = case.run_in.iter().map(|n| (n, Role::Environment)).chain(case.uses.iter().map(|n| (n, Role::Service)));
        for (name, role) in wanted {
            let (id, owned) = host.lease(ctx, name, role, case).await.with_context(|| format!("setting up fixture `{name}`"))?;
            leases.push(id);
            if let Some(hint) = owned {
                own.push((id, hint));
            }
        }
        Ok(())
    }
    .await;

    let outcome = match setup {
        Err(e) => Outcome {
            status: Status::Fail(format!("{e:#}")),
            duration: start.elapsed(),
            output: vec![],
            peak_memory: None,
        },
        Ok(()) => {
            let mut args = Vec::new();
            if case.bin.harness {
                args.extend(["--exact".to_string(), case.name.clone(), "--nocapture".into()]);
                if case.ignored {
                    args.push("--include-ignored".into());
                }
            }
            let spec = SpawnSpec {
                exe: case.bin.exe.clone(),
                args,
                cwd: case.bin.manifest_dir.clone(),
                env: case.bin.env(),
                timeout: case.timeout,
                leases: leases.clone(),
            };
            match host.agent.run_tracked(spec, started).await {
                Ok(r) => Outcome {
                    status: if r.exit.success() { Status::Pass } else { Status::Fail(r.exit.to_string()) },
                    duration: r.duration,
                    output: r.output,
                    peak_memory: r.peak_memory,
                },
                Err(e) => Outcome {
                    status: Status::Fail(format!("{e:#}")),
                    duration: start.elapsed(),
                    output: vec![],
                    peak_memory: None,
                },
            }
        }
    };

    let failed = outcome.status != Status::Pass;
    for (id, hint) in own {
        if failed && ctx.keep_fixtures {
            let msg = format!(
                "kept fixture for {} on `{}`{}",
                case.display(),
                host.agent.name,
                hint.map(|h| format!(": {h}")).unwrap_or_default()
            );
            host.kept.lock().unwrap().push((id, msg));
        } else {
            let _ = host.agent.request(Request::Teardown { lease: id }).await;
        }
    }
    outcome
}

#[derive(Default)]
struct Counts {
    passed: usize,
    failed: Vec<String>,
}

pub struct Reporter {
    color: bool,
    start: Instant,
    host_width: usize,
    counts: Mutex<Counts>,
}

impl Reporter {
    pub fn new(host_width: usize) -> Self {
        Reporter {
            color: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            start: Instant::now(),
            host_width,
            counts: Default::default(),
        }
    }

    fn paint(&self, code: &str, s: &str) -> String {
        if self.color { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
    }

    pub fn status(&self, verb: &str, msg: &str) {
        eprintln!("{} {msg}", self.paint("1;32", &format!("{verb:>12}")));
    }

    pub fn warn(&self, msg: &str) {
        eprintln!("{} {msg}", self.paint("1;33", "warning:"));
    }

    fn report(&self, host: &str, case: &Case, o: &Outcome) {
        let (label, code) = match &o.status {
            Status::Pass => ("PASS", "1;32"),
            Status::Fail(_) => ("FAIL", "1;31"),
        };
        let mut err = std::io::stderr().lock();
        let mem = o.peak_memory.map(format_size).unwrap_or_else(|| "-".into());
        let _ = writeln!(
            err,
            "{} [{:>9.3}s {mem:>10}] {:<w$} {}",
            self.paint(code, &format!("{label:>12}")),
            o.duration.as_secs_f64(),
            host,
            case.display(),
            w = self.host_width
        );
        let mut counts = self.counts.lock().unwrap();
        match &o.status {
            Status::Pass => counts.passed += 1,
            Status::Fail(reason) => {
                let _ = writeln!(err, "{}", self.paint("31", &format!("--- {} ({reason}) ---", case.display())));
                if !o.output.is_empty() {
                    let _ = err.write_all(&o.output);
                    if !o.output.ends_with(b"\n") {
                        let _ = writeln!(err);
                    }
                }
                counts.failed.push(format!("{} on `{host}`", case.display()));
            }
        }
    }

    /// Prints the summary; returns whether everything passed.
    pub fn summary(&self, skipped: usize) -> bool {
        let c = self.counts.lock().unwrap();
        let total = c.passed + c.failed.len();
        eprintln!("------------");
        eprintln!(
            "{} [{:>9.3}s] {total} tests run: {} passed, {} failed, {skipped} skipped",
            self.paint("1;32", &format!("{:>12}", "Summary")),
            self.start.elapsed().as_secs_f64(),
            c.passed,
            c.failed.len()
        );
        for f in &c.failed {
            eprintln!("{} {f}", self.paint("1;31", &format!("{:>12}", "FAIL")));
        }
        c.failed.is_empty()
    }
}
