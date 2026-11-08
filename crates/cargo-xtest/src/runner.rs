//! Discovering tests, setting up fixtures and spreading tests over hosts.

use std::collections::{BTreeSet, HashMap, VecDeque};
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
use crate::config::Scope;
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
        let req = Request::Setup { lease, name: name.into(), role, spec: def.spec.clone() };
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
    pub reporter: Reporter,
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
        let parse = |(exit, output, _): (Exit, Vec<u8>, Duration)| -> Result<Vec<String>> {
            let text = String::from_utf8_lossy(&output);
            if !exit.success() {
                bail!("listing tests in {} failed ({exit}):\n{text}", bin.id());
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

pub async fn run_all(ctx: Arc<Ctx>, hosts: &[Arc<HostState>], cases: Vec<Case>) {
    let queue = Arc::new(Mutex::new(VecDeque::from(cases)));
    let mut set = JoinSet::new();
    for host in hosts {
        for _ in 0..host.slots {
            let (ctx, host, queue) = (ctx.clone(), host.clone(), queue.clone());
            set.spawn(async move {
                loop {
                    let Some(case) = queue.lock().unwrap().pop_front() else { break };
                    let outcome = run_case(&ctx, &host, &case).await;
                    ctx.reporter.report(&host.agent.name, &case, &outcome);
                }
            });
        }
    }
    while set.join_next().await.is_some() {}
}

pub struct Outcome {
    pub status: Status,
    pub duration: Duration,
    pub output: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Pass,
    Fail(String),
}

async fn run_case(ctx: &Ctx, host: &HostState, case: &Case) -> Outcome {
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
        Err(e) => Outcome { status: Status::Fail(format!("{e:#}")), duration: start.elapsed(), output: vec![] },
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
            match host.agent.run(spec).await {
                Ok((exit, output, duration)) => Outcome {
                    status: if exit.success() { Status::Pass } else { Status::Fail(exit.to_string()) },
                    duration,
                    output,
                },
                Err(e) => Outcome { status: Status::Fail(format!("{e:#}")), duration: start.elapsed(), output: vec![] },
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
        let _ = writeln!(
            err,
            "{} [{:>9.3}s] {:<w$} {}",
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
