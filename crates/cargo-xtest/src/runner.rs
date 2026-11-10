//! Discovering tests, setting up fixtures and spreading tests over hosts.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tokio::sync::OnceCell;
use tokio::task::JoinSet;
use xtest_proto::*;

use crate::cargo::TestBinary;
use crate::client::{Agent, Output};
use crate::config::{format_size, Scope};
use crate::files::FileSet;
use crate::history::Measured;

pub struct FixtureDef {
    pub scope: Scope,
    pub spec: FixtureSpec,
    /// Estimated memory of the fixture itself, in bytes.
    pub memory: u64,
    pub memory_source: MemorySource,
}

impl FixtureDef {
    /// Memory to set aside for one instance in this role. A Docker
    /// environment holds only the tests that run in it, which are charged
    /// for themselves.
    pub fn charge(&self, role: Role) -> u64 {
        match (&self.spec, role) {
            (FixtureSpec::Docker(_), Role::Environment) => 0,
            _ => self.memory,
        }
    }
}

/// History key of a fixture's measured peak.
pub fn fixture_history_key(name: &str) -> String {
    format!("fixture:{name}")
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
    /// How long it took last time, if known.
    pub expected: Option<Duration>,
    /// How many times to rerun it if it fails.
    pub retries: u32,
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

/// A fixture instance: (fixture, is environment, scope key). The scope key is
/// empty for `run` scope, the binary for `binary` scope, and unique per test
/// for `test` scope.
type LeaseKey = (String, bool, String);
type SharedLease = Arc<OnceCell<Result<(u64, Option<String>), String>>>;

/// Memory set aside on a host for a fixture instance, from when the first
/// test that needs it starts until the instance is torn down.
struct Reservation {
    memory: u64,
    /// The agent's lease id, once set up; its measured use counts against `memory`.
    lease: Option<u64>,
}

pub struct HostState {
    pub agent: Agent,
    pub slots: usize,
    leases: tokio::sync::Mutex<HashMap<LeaseKey, SharedLease>>,
    reservations: Mutex<HashMap<LeaseKey, Reservation>>,
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
            reservations: Default::default(),
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
    async fn lease(&self, ctx: &Ctx, name: &str, role: Role, case: &Case, slot: usize) -> Result<(u64, Option<Option<String>>)> {
        let def = &ctx.fixtures[name];
        let key = lease_key(def, name, role, case, slot);
        let res = if def.scope == Scope::Test {
            self.setup(def, name, role).await.map(|(id, hint)| (id, Some(hint)))
        } else {
            let cell = self.leases.lock().await.entry(key.clone()).or_default().clone();
            let res = cell
                .get_or_init(|| async { self.setup(def, name, role).await.map_err(|e| format!("{e:#}")) })
                .await;
            match res {
                Ok((id, _)) => Ok((*id, None)),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        };
        let mut reservations = self.reservations.lock().unwrap();
        match &res {
            Ok((id, _)) => {
                if let Some(r) = reservations.get_mut(&key) {
                    r.lease = Some(*id);
                }
            }
            // Nothing runs, so nothing to set aside.
            Err(_) => {
                reservations.remove(&key);
            }
        }
        res
    }

    /// Fixture instances `case` needs that have no memory set aside on this
    /// host yet, with the memory they need.
    fn unreserved(&self, ctx: &Ctx, case: &Case, slot: usize) -> Vec<(LeaseKey, u64)> {
        let reservations = self.reservations.lock().unwrap();
        fixtures_of(case)
            .filter_map(|(name, role)| {
                let def = &ctx.fixtures[name];
                let key = lease_key(def, name, role, case, slot);
                (!reservations.contains_key(&key)).then(|| (key, def.charge(role)))
            })
            .collect()
    }

    /// Memory that fixtures are still expected to take on top of what they use now.
    fn fixtures_owed(&self, mem: &MemoryInfo) -> u64 {
        self.reservations
            .lock()
            .unwrap()
            .values()
            .map(|r| r.memory.saturating_sub(r.lease.and_then(|l| mem.fixtures.get(&l)).map_or(0, |u| u.current)))
            .sum()
    }

    /// Peaks of the fixture instances still alive, by fixture name.
    pub async fn fixture_peaks(&self) -> Vec<(String, u64)> {
        let Ok(mem) = self.agent.memory().await else { return vec![] };
        let reservations = self.reservations.lock().unwrap();
        reservations
            .iter()
            .filter_map(|((name, _, _), r)| Some((name.clone(), mem.fixtures.get(&r.lease?)?.peak)))
            .filter(|(_, peak)| *peak > 0)
            .collect()
    }
}

pub struct Ctx {
    pub fixtures: HashMap<String, FixtureDef>,
    pub keep_fixtures: bool,
    pub memory_reserve: u64,
    pub reporter: Reporter,
    /// What this run measured, for the history.
    pub measured: Mutex<Measured>,
    /// `--fail-fast`: start no new tests after the first failure.
    pub fail_fast: bool,
    pub stopped: AtomicBool,
}

impl Ctx {
    fn record(&self, host: &str, case: &Case, outcome: &Outcome, attempt: u32) {
        {
            let mut measured = self.measured.lock().unwrap();
            if let Some(peak) = outcome.peak_memory {
                measured.peaks.insert(case.display(), peak);
            }
            if let Some(d) = outcome.ran_for {
                measured.durations.insert(case.display(), d);
            }
        }
        self.reporter.report(host, case, outcome, attempt);
        if self.fail_fast && outcome.status != Status::Pass {
            self.stopped.store(true, Ordering::Relaxed);
        }
    }

    /// Records a fixture instance's peak; the history keeps the largest of this run.
    pub fn record_fixture(&self, name: &str, peak: u64) {
        let mut measured = self.measured.lock().unwrap();
        let e = measured.peaks.entry(fixture_history_key(name)).or_insert(0);
        *e = (*e).max(peak);
    }
}

pub async fn sync(agent: &Agent, files: &FileSet) -> Result<(usize, u64)> {
    let hashes: BTreeSet<Hash> = files.files.values().map(|f| f.hash).collect();
    let Reply::Missing { hashes: missing } = agent.request(Request::HaveBlobs { hashes: hashes.into_iter().collect() }).await? else {
        bail!("unexpected reply to HaveBlobs");
    };
    // Uploads land in the store, and every file that isn't hardlinked is
    // copied into the run's sandbox as well.
    let upload: u64 = missing.iter().map(|h| files.sizes.get(h).copied().unwrap_or(0)).sum();
    let copies: u64 = files.files.values().filter(|f| !f.link).map(|f| files.sizes.get(&f.hash).copied().unwrap_or(0)).sum();
    let needed = upload + copies + (64 << 20);
    if needed > agent.info.disk_free {
        bail!(
            "only {} is free under the agent's root directory, but this run needs about {} \
             ({} to upload). Free some space, e.g. with `cargo xtest clean`, or lower `store-size` in [[host]]",
            format_size(agent.info.disk_free),
            format_size(needed),
            format_size(upload)
        );
    }
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
pub async fn discover(
    agent: &Agent,
    bins: &[Arc<TestBinary>],
    reporter: &Reporter,
) -> Result<Vec<(Arc<TestBinary>, String, bool)>> {
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
            if !r.exit.success() {
                let text = String::from_utf8_lossy(&r.output.all()).into_owned();
                bail!("listing tests in {} failed ({}):\n{text}", bin.id(), r.exit);
            }
            let text = String::from_utf8_lossy(&r.output.of(Stream::Stdout)).into_owned();
            Ok(text.lines().filter_map(|l| l.strip_suffix(": test")).map(String::from).collect())
        };
        let (all, dups) = dedup(parse(agent.run(list(None)).await?)?);
        for name in dups {
            reporter.warn(&format!(
                "{}: `{name}` is listed more than once; running it once. \
                 Is it marked with both #[test] and #[xtest::test]?",
                bin.id()
            ));
        }
        let ignored: BTreeSet<String> = parse(agent.run(list(Some("--ignored"))).await?)?.into_iter().collect();
        for name in all {
            let ig = ignored.contains(&name);
            out.push((bin.clone(), name, ig));
        }
    }
    Ok(out)
}

/// Removes repeated test names, keeping the first. Returns the names and the
/// repeated ones. libtest lists a test twice when two test attributes are
/// applied to it, e.g. `#[test]` above `#[xtest::test]`; `--exact` would then
/// run it twice in one process.
fn dedup(names: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut seen = BTreeSet::new();
    let mut dups = BTreeSet::new();
    let mut unique = Vec::new();
    for name in names {
        if seen.insert(name.clone()) {
            unique.push(name);
        } else {
            dups.insert(name);
        }
    }
    (unique, dups.into_iter().collect())
}

fn lease_key(def: &FixtureDef, name: &str, role: Role, case: &Case, slot: usize) -> LeaseKey {
    let scope = match def.scope {
        Scope::Test => format!("test#{slot}"),
        Scope::Binary => case.bin.exe.display().to_string(),
        Scope::Run => String::new(),
    };
    (name.to_string(), role == Role::Environment, scope)
}

/// The fixtures a test needs, with the role it uses each in.
fn fixtures_of(case: &Case) -> impl Iterator<Item = (&String, Role)> {
    case.run_in.iter().map(|n| (n, Role::Environment)).chain(case.uses.iter().map(|n| (n, Role::Service)))
}

/// Orders the queue: longest expected duration first, so a slow test doesn't
/// start last and make the run longer than it needs to be. Tests without
/// history count as the median of the known durations. Ties, and runs without
/// any history, go biggest memory first: those are the hardest to fit once
/// memory is shared out. The sort is stable, so equal tests keep their
/// discovery order.
pub fn order(cases: &mut [Case]) {
    let mut known: Vec<Duration> = cases.iter().filter_map(|c| c.expected).collect();
    known.sort();
    let median = known.get(known.len() / 2).copied().unwrap_or_default();
    cases.sort_by_key(|c| (std::cmp::Reverse(c.expected.unwrap_or(median)), std::cmp::Reverse(c.memory)));
}

/// Runs every test; returns how many were not started (after `--fail-fast`).
pub async fn run_all(ctx: Arc<Ctx>, hosts: &[Arc<HostState>], mut cases: Vec<Case>) -> usize {
    order(&mut cases);
    let queue = Arc::new(Mutex::new(VecDeque::from(cases)));
    let mut set = JoinSet::new();
    for host in hosts {
        set.spawn(dispatch(ctx.clone(), host.clone(), queue.clone()));
    }
    while set.join_next().await.is_some() {}
    let not_run = queue.lock().unwrap().len();
    not_run
}

/// A test started on a host that may not have reached its full memory use yet.
struct Inflight {
    memory: u64,
    /// The agent's id for the test process, once it is spawned.
    req: Option<u64>,
}

/// Starts tests on one host while it has a free slot and enough memory.
///
/// Free memory is `MemAvailable` minus what running tests and fixtures are
/// still expected to take on top of what they use now (their estimate minus
/// current usage), minus the reserve. A test costs its own estimate plus that
/// of each fixture instance it would create. A test that fits nowhere still
/// runs once the host is idle, so nothing waits forever.
async fn dispatch(ctx: Arc<Ctx>, host: Arc<HostState>, queue: Arc<Mutex<VecDeque<Case>>>) {
    let inflight: Arc<Mutex<HashMap<usize, Inflight>>> = Default::default();
    let mut running = JoinSet::new();
    let mut next_id = 0usize;
    loop {
        while running.len() < host.slots && !queue.lock().unwrap().is_empty() && !ctx.stopped.load(Ordering::Relaxed) {
            let mem = host.agent.memory().await.ok();
            let free = mem.as_ref().map(|m| {
                let owed: u64 = inflight
                    .lock()
                    .unwrap()
                    .values()
                    .map(|f| f.memory.saturating_sub(f.req.and_then(|r| m.running.get(&r).copied()).unwrap_or(0)))
                    .sum();
                let owed = owed + host.fixtures_owed(m);
                m.available.saturating_sub(owed).saturating_sub(ctx.memory_reserve)
            });
            let id = next_id;
            let cost = |c: &Case| c.memory + host.unreserved(&ctx, c, id).iter().map(|(_, m)| m).sum::<u64>();
            let case = {
                let mut q = queue.lock().unwrap();
                let idx = match free {
                    // The agent is unreachable; the test will report that.
                    None => Some(0),
                    Some(free) => q.iter().position(|c| cost(c) <= free).or(running.is_empty().then_some(0)),
                };
                idx.and_then(|i| q.remove(i))
            };
            let Some(case) = case else { break };
            let new_fixtures = host.unreserved(&ctx, &case, id);
            let total_cost = case.memory + new_fixtures.iter().map(|(_, m)| m).sum::<u64>();
            if let Some(m) = &mem {
                if total_cost > m.total {
                    ctx.reporter.warn(&format!(
                        "{} needs {} (with its fixtures) but `{}` only has {} in total",
                        case.display(),
                        format_size(total_cost),
                        host.agent.name,
                        format_size(m.total)
                    ));
                }
            }
            {
                let mut reservations = host.reservations.lock().unwrap();
                for (key, memory) in new_fixtures {
                    reservations.insert(key, Reservation { memory, lease: None });
                }
            }
            next_id += 1;
            inflight.lock().unwrap().insert(id, Inflight { memory: case.memory, req: None });
            let (ctx, host, inflight) = (ctx.clone(), host.clone(), inflight.clone());
            running.spawn(async move {
                let mut attempt = 1;
                let outcome = loop {
                    let track = inflight.clone();
                    let started = move |req| {
                        if let Some(f) = track.lock().unwrap().get_mut(&id) {
                            f.req = Some(req);
                        }
                    };
                    let last = attempt > case.retries;
                    let outcome = run_case(&ctx, &host, &case, id, last, started).await;
                    if outcome.status == Status::Pass || last {
                        break outcome;
                    }
                    ctx.reporter.retry(&host.agent.name, &case, &outcome, attempt);
                    attempt += 1;
                    // The next attempt gets fresh test-scoped fixtures.
                    let fresh = host.unreserved(&ctx, &case, id);
                    let mut reservations = host.reservations.lock().unwrap();
                    for (key, memory) in fresh {
                        reservations.insert(key, Reservation { memory, lease: None });
                    }
                };
                inflight.lock().unwrap().remove(&id);
                ctx.record(&host.agent.name, &case, &outcome, attempt);
            });
        }
        if running.is_empty() {
            if queue.lock().unwrap().is_empty() || ctx.stopped.load(Ordering::Relaxed) {
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
    /// How long the test process ran, if it ran to completion (not for
    /// timeouts or failed fixture setups). Saved for ordering later runs.
    pub ran_for: Option<Duration>,
    pub output: Output,
    pub peak_memory: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Pass,
    Fail(String),
}

/// Runs one attempt of a test. `last` is false while retries remain; only a
/// failed last attempt keeps its fixtures for `--keep-fixtures`.
async fn run_case(ctx: &Ctx, host: &HostState, case: &Case, slot: usize, last: bool, started: impl FnOnce(u64)) -> Outcome {
    let start = Instant::now();
    let mut leases = Vec::new();
    // Test-scoped leases: torn down (or kept) after this test.
    let mut own: Vec<(u64, Option<String>, LeaseKey)> = Vec::new();
    let setup: Result<()> = async {
        for (name, role) in fixtures_of(case) {
            let (id, owned) = host.lease(ctx, name, role, case, slot).await.with_context(|| format!("setting up fixture `{name}`"))?;
            leases.push(id);
            if let Some(hint) = owned {
                own.push((id, hint, lease_key(&ctx.fixtures[name], name, role, case, slot)));
            }
        }
        Ok(())
    }
    .await;
    // Test-scoped instances that were never created (an earlier fixture failed).
    {
        let mut reservations = host.reservations.lock().unwrap();
        reservations.retain(|k, r| !(k.2 == format!("test#{slot}") && r.lease.is_none()));
    }

    let outcome = match setup {
        Err(e) => Outcome {
            status: Status::Fail(format!("{e:#}")),
            duration: start.elapsed(),
            ran_for: None,
            output: Output::default(),
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
            let mut live = LiveLines::default();
            let on_output = |stream, data: &[u8]| {
                if ctx.reporter.live {
                    live.push(stream, data, |stream, line| ctx.reporter.live(&host.agent.name, case, stream, line));
                }
            };
            let result = host.agent.run_tracked(spec, started, on_output).await;
            live.flush(|stream, line| ctx.reporter.live(&host.agent.name, case, stream, line));
            match result {
                Ok(r) => Outcome {
                    status: if r.exit.success() { Status::Pass } else { Status::Fail(r.exit.to_string()) },
                    duration: r.duration,
                    ran_for: matches!(r.exit, Exit::Code(_) | Exit::Signal(_)).then_some(r.duration),
                    output: r.output,
                    peak_memory: r.peak_memory,
                },
                Err(e) => Outcome {
                    status: Status::Fail(format!("{e:#}")),
                    duration: start.elapsed(),
                    ran_for: None,
                    output: Output::default(),
                    peak_memory: None,
                },
            }
        }
    };

    let failed = outcome.status != Status::Pass;
    for (id, hint, key) in own {
        if failed && last && ctx.keep_fixtures {
            let msg = format!(
                "kept fixture for {} on `{}`{}",
                case.display(),
                host.agent.name,
                hint.map(|h| format!(": {h}")).unwrap_or_default()
            );
            host.kept.lock().unwrap().push((id, msg));
        } else {
            if let Ok(Reply::TornDown { peak: Some(peak) }) = host.agent.request(Request::Teardown { lease: id }).await {
                ctx.record_fixture(&key.0, peak);
            }
            host.reservations.lock().unwrap().remove(&key);
        }
    }
    outcome
}

#[derive(Default)]
struct Counts {
    passed: usize,
    /// Passed, but only after failing first.
    flaky: Vec<String>,
    failed: Vec<String>,
}

/// Splits a test's output into lines for `--no-capture`, per stream. A
/// partial line waits for its newline, or for [`flush`](Self::flush).
#[derive(Default)]
struct LiveLines {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl LiveLines {
    fn push(&mut self, stream: Stream, data: &[u8], mut emit: impl FnMut(Stream, &[u8])) {
        let buf = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        buf.extend_from_slice(data);
        let mut start = 0;
        while let Some(nl) = buf[start..].iter().position(|b| *b == b'\n') {
            emit(stream, &buf[start..start + nl]);
            start += nl + 1;
        }
        buf.drain(..start);
    }

    /// Emits what is left of unfinished lines.
    fn flush(&mut self, mut emit: impl FnMut(Stream, &[u8])) {
        for (stream, buf) in [(Stream::Stdout, &mut self.stdout), (Stream::Stderr, &mut self.stderr)] {
            if !buf.is_empty() {
                emit(stream, buf);
                buf.clear();
            }
        }
    }
}

pub struct Reporter {
    color: bool,
    start: Instant,
    host_width: usize,
    counts: Mutex<Counts>,
    /// `--no-capture`: print test output live, line by line.
    pub live: bool,
    /// `--success-output`: also show the output of passing tests.
    pub success_output: bool,
}

impl Reporter {
    pub fn new(host_width: usize) -> Self {
        Reporter {
            color: std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
            start: Instant::now(),
            host_width,
            counts: Default::default(),
            live: false,
            success_output: false,
        }
    }

    /// One line of a test's output, with `--no-capture`. stderr lines use `!`
    /// instead of `|` (and are colored on a terminal).
    fn live(&self, host: &str, case: &Case, stream: Stream, line: &[u8]) {
        let line = String::from_utf8_lossy(line);
        let line = line.strip_suffix('\r').unwrap_or(&line);
        let (sep, text) = match stream {
            Stream::Stdout => ("|", line.to_string()),
            Stream::Stderr => ("!", self.paint("33", line)),
        };
        let prefix = self.paint("2", &format!("{host:<w$} {} {sep}", case.display(), w = self.host_width));
        let _ = writeln!(std::io::stderr().lock(), "{prefix} {text}");
    }

    /// A test's captured output, with stderr colored on a terminal.
    fn write_output(&self, err: &mut impl Write, output: &Output) {
        for (stream, data) in &output.0 {
            match stream {
                Stream::Stdout => {
                    let _ = err.write_all(data);
                }
                Stream::Stderr => {
                    let _ = write!(err, "{}", self.paint("33", &String::from_utf8_lossy(data)));
                }
            }
        }
        if !output.all().ends_with(b"\n") {
            let _ = writeln!(err);
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

    fn result_line(&self, err: &mut impl Write, label: &str, code: &str, host: &str, case: &Case, o: &Outcome) {
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
    }

    /// A failed attempt that will be retried.
    fn retry(&self, host: &str, case: &Case, o: &Outcome, attempt: u32) {
        let mut err = std::io::stderr().lock();
        self.result_line(&mut err, &format!("TRY {attempt} FAIL"), "1;33", host, case, o);
        if let Status::Fail(reason) = &o.status {
            let header = format!("--- {} (try {attempt}: {reason}) ---", case.display());
            let _ = writeln!(err, "{}", self.paint("33", &header));
        }
        if !self.live && !o.output.is_empty() {
            self.write_output(&mut err, &o.output);
        }
    }

    /// The final result of a test, after `attempt` tries.
    fn report(&self, host: &str, case: &Case, o: &Outcome, attempt: u32) {
        let flaky = o.status == Status::Pass && attempt > 1;
        let (label, code) = match &o.status {
            Status::Pass if flaky => ("FLAKY", "1;33"),
            Status::Pass => ("PASS", "1;32"),
            Status::Fail(_) => ("FAIL", "1;31"),
        };
        let mut err = std::io::stderr().lock();
        self.result_line(&mut err, label, code, host, case, o);
        // With --no-capture, the output was already shown as it came.
        let show = !self.live && !o.output.is_empty();
        let mut counts = self.counts.lock().unwrap();
        match &o.status {
            Status::Pass => {
                counts.passed += 1;
                if flaky {
                    counts.flaky.push(format!("{} on `{host}` (passed on try {attempt})", case.display()));
                }
                if show && self.success_output {
                    let _ = writeln!(err, "{}", self.paint("2", &format!("--- {} (output) ---", case.display())));
                    self.write_output(&mut err, &o.output);
                }
            }
            Status::Fail(reason) => {
                let tries = if attempt > 1 { format!(", {attempt} tries") } else { String::new() };
                let _ = writeln!(err, "{}", self.paint("31", &format!("--- {} ({reason}{tries}) ---", case.display())));
                if show {
                    self.write_output(&mut err, &o.output);
                }
                counts.failed.push(format!("{} on `{host}`", case.display()));
            }
        }
    }

    /// Prints the summary; returns whether everything passed.
    pub fn summary(&self, skipped: usize, not_run: usize) -> bool {
        let c = self.counts.lock().unwrap();
        let total = c.passed + c.failed.len();
        let flaky = if c.flaky.is_empty() { String::new() } else { format!(" ({} flaky)", c.flaky.len()) };
        let not_run_note = if not_run > 0 { format!(", {not_run} not run") } else { String::new() };
        eprintln!("------------");
        eprintln!(
            "{} [{:>9.3}s] {total} tests run: {} passed{flaky}, {} failed, {skipped} skipped{not_run_note}",
            self.paint("1;32", &format!("{:>12}", "Summary")),
            self.start.elapsed().as_secs_f64(),
            c.passed,
            c.failed.len()
        );
        for f in &c.flaky {
            eprintln!("{} {f}", self.paint("1;33", &format!("{:>12}", "FLAKY")));
        }
        for f in &c.failed {
            eprintln!("{} {f}", self.paint("1;31", &format!("{:>12}", "FAIL")));
        }
        if not_run > 0 {
            self.warn(&format!("stopped after the first failure (--fail-fast): {not_run} tests not run"));
        }
        c.failed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedup_keeps_the_first_of_each_name() {
        let names = ["b", "a", "b", "c", "b", "a"].map(String::from).to_vec();
        let (unique, dups) = dedup(names);
        assert_eq!(unique, ["b", "a", "c"]);
        assert_eq!(dups, ["a", "b"]);
    }

    fn case(name: &str, memory: u64, expected: Option<u64>) -> Case {
        let bin = TestBinary {
            package: "p".into(),
            version: "0.1.0".into(),
            name: "p".into(),
            kind: "lib".into(),
            exe: "/p".into(),
            manifest_dir: "/".into(),
            harness: true,
        };
        Case {
            bin: Arc::new(bin),
            name: name.into(),
            ignored: false,
            run_in: None,
            uses: vec![],
            timeout: None,
            memory,
            memory_source: MemorySource::Default,
            expected: expected.map(Duration::from_secs),
            retries: 0,
        }
    }

    fn names(cases: &[Case]) -> Vec<&str> {
        cases.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn longest_first_with_unknown_tests_at_the_median() {
        let mut cases = vec![case("a", 1, Some(1)), case("new", 1, None), case("b", 1, Some(30)), case("c", 1, Some(5))];
        order(&mut cases);
        // Known: 1, 5, 30 → median 5; the unknown test ties with `c` and keeps its place.
        assert_eq!(names(&cases), ["b", "new", "c", "a"]);
    }

    #[test]
    fn live_lines_wait_for_newlines() {
        let mut live = LiveLines::default();
        let mut got = Vec::new();
        let mut emit = |s: Stream, l: &[u8]| got.push((s, String::from_utf8_lossy(l).into_owned()));
        live.push(Stream::Stdout, b"one\ntw", &mut emit);
        live.push(Stream::Stderr, b"err", &mut emit);
        live.push(Stream::Stdout, b"o\nthree", &mut emit);
        live.flush(&mut emit);
        let s = |t: &str| t.to_string();
        assert_eq!(
            got,
            [(Stream::Stdout, s("one")), (Stream::Stdout, s("two")), (Stream::Stdout, s("three")), (Stream::Stderr, s("err"))]
        );
    }

    #[test]
    fn without_history_biggest_memory_first() {
        let mut cases = vec![case("small", 1, None), case("big", 9, None), case("mid", 5, None)];
        order(&mut cases);
        assert_eq!(names(&cases), ["big", "mid", "small"]);
    }
}
