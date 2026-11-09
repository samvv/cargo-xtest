//! `cargo xtest`: builds tests locally, ships them to one or more hosts and
//! runs every test in its own process, optionally inside fixtures.

mod cargo;
mod client;
mod config;
mod files;
mod filter;
mod history;
mod runner;

use std::collections::HashMap;
use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use sha2::{Digest, Sha256};
use xtest_proto::{CommandSpec, DockerSpec, FixtureSpec, ImageSource, K3dSpec, Ready};

use crate::config::{Config, FixtureKind, Host};
use crate::filter::{Filter, Subject};
use crate::config::format_size;
use crate::runner::{Case, Ctx, FixtureDef, HostState, MemorySource, Reporter};

#[derive(Parser, Debug)]
#[command(name = "cargo xtest", version, about = "Run cargo tests on other hosts and inside fixtures")]
struct Args {
    /// Run only tests whose name contains one of these strings.
    filters: Vec<String>,
    /// Filter expression, e.g. `package(db) & test(/integration/)`. Repeatable (any may match).
    #[arg(short = 'E', long = "filter-expr")]
    exprs: Vec<String>,
    /// Package(s) to test.
    #[arg(short, long = "package")]
    packages: Vec<String>,
    #[arg(long)]
    workspace: bool,
    #[arg(short = 'F', long, value_delimiter = ',')]
    features: Vec<String>,
    #[arg(long)]
    all_features: bool,
    #[arg(long)]
    no_default_features: bool,
    #[arg(short, long)]
    release: bool,
    /// Target triple to build for (overrides the profile).
    #[arg(long)]
    target: Option<String>,
    /// Config file to use instead of `.config/xtest.toml` in the workspace root.
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Profile from `.config/xtest.toml`.
    #[arg(long, default_value = "default")]
    profile: String,
    /// Run only on these hosts.
    #[arg(long = "host")]
    hosts: Vec<String>,
    /// Run only ignored tests.
    #[arg(long, conflicts_with = "include_ignored")]
    ignored: bool,
    /// Run ignored tests too.
    #[arg(long)]
    include_ignored: bool,
    /// Keep fixtures of failed tests for debugging.
    #[arg(long)]
    keep_fixtures: bool,
    /// List the tests that would run, then exit.
    #[arg(long)]
    list: bool,
}

fn main() -> ExitCode {
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    if argv.get(1).is_some_and(|a| a == "xtest") {
        argv.remove(1);
    }
    let args = Args::parse_from(argv);
    match run(args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn run(args: Args) -> Result<ExitCode> {
    let ws = cargo::workspace()?;
    let cfg = Config::load(&ws.root, args.config.as_deref())?;
    let profile = cfg.resolve(&args.profile)?;
    let exprs = args.exprs.iter().map(|e| Filter::parse(e).with_context(|| format!("filter `{e}`"))).collect::<Result<Vec<_>>>()?;
    let hosts = select_hosts(&cfg, &args.hosts)?;

    let target = args.target.clone().or(profile.target.clone());
    let built = cargo::build(&cargo::CargoArgs {
        packages: args.packages.clone(),
        workspace: args.workspace,
        features: args.features.clone(),
        all_features: args.all_features,
        no_default_features: args.no_default_features,
        release: args.release,
        target: target.clone(),
    })?;
    let files = files::collect(&ws.root, &ws.target_dir, &profile.include, &built.executables)?;
    let fixtures = fixture_defs(&cfg, &ws.root, &files)?;
    let mut history = history::History::load(&ws.target_dir);

    let width = hosts.iter().map(|h| h.name.len()).max().unwrap_or(5);
    let reporter = Reporter::new(width);
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let total: u64 = files.sizes.values().sum();
        reporter.status(
            "Syncing",
            &format!("{} files ({:.1} MB) to {} host(s)", files.files.len(), total as f64 / 1e6, hosts.len()),
        );
        let files = Arc::new(files);
        let mut set = tokio::task::JoinSet::new();
        for (i, h) in hosts.into_iter().enumerate() {
            let files = files.clone();
            set.spawn(async move {
                let agent = client::Agent::connect(&h).await.with_context(|| format!("host `{}`", h.name))?;
                let (n, bytes) = runner::sync(&agent, &files).await.with_context(|| format!("syncing to `{}`", h.name))?;
                let slots = h.slots.unwrap_or(agent.info.cpus as usize).max(1);
                anyhow::Ok((i, HostState::new(agent, slots), n, bytes))
            });
        }
        let mut connected = Vec::new();
        let mut first_err = None;
        while let Some(r) = set.join_next().await {
            match r? {
                Ok(x) => connected.push(x),
                Err(e) => first_err = first_err.or(Some(e)),
            }
        }
        connected.sort_by_key(|x| x.0);
        let hosts: Vec<Arc<HostState>> = connected
            .into_iter()
            .map(|(_, h, n, bytes)| {
                let i = &h.agent.info;
                reporter.status(
                    "Connected",
                    &format!(
                        "`{}` ({}/{}, {} slots, uploaded {n} blobs / {:.1} MB)",
                        h.agent.name, i.os, i.arch, h.slots, bytes as f64 / 1e6
                    ),
                );
                Arc::new(h)
            })
            .collect();

        let result = async {
            if let Some(e) = first_err {
                return Err(e);
            }
            for h in &hosts {
                check_host(&h.agent, target.as_deref(), &reporter)?;
            }
            let bins: Vec<_> = built.tests.into_iter().map(Arc::new).collect();
            let found = runner::discover(&hosts[0].agent, &bins).await?;

            let mut cases = Vec::new();
            let mut skipped = 0;
            for (bin, name, ignored) in found {
                let subject = Subject { package: &bin.package, binary: &bin.name, kind: &bin.kind, test: &name };
                let mut case = Case {
                    bin: bin.clone(),
                    name: name.clone(),
                    ignored,
                    run_in: None,
                    uses: vec![],
                    timeout: profile.timeout,
                    memory: profile.memory,
                    memory_source: MemorySource::Default,
                };
                if let Some(&peak) = history.peaks.get(&case.display()) {
                    case.memory = peak;
                    case.memory_source = MemorySource::Measured;
                }
                let mut include_ignored = args.include_ignored;
                for (f, o) in &profile.overrides {
                    if f.matches(&subject) {
                        if let Some(e) = &o.run_in {
                            case.run_in = Some(e.clone());
                        }
                        case.uses.extend(o.uses.iter().cloned());
                        if let Some(ii) = o.include_ignored {
                            include_ignored = ii;
                        }
                        if let Some(t) = &o.timeout {
                            case.timeout = Some(config::parse_duration(t)?);
                        }
                        if let Some(m) = &o.memory {
                            case.memory = config::parse_size(m)?;
                            case.memory_source = MemorySource::Config;
                        }
                    }
                }
                let selected = (args.filters.is_empty() || args.filters.iter().any(|f| name.contains(f.as_str())))
                    && (exprs.is_empty() || exprs.iter().any(|e| e.matches(&subject)))
                    && if args.ignored { ignored } else { !ignored || include_ignored };
                if selected {
                    case.uses.dedup();
                    cases.push(case);
                } else {
                    skipped += 1;
                }
            }

            if args.list {
                for c in &cases {
                    let source = match c.memory_source {
                        MemorySource::Config => "config",
                        MemorySource::Measured => "measured",
                        MemorySource::Default => "default",
                    };
                    let mut extra = format!(" [memory: {} ({source})]", format_size(c.memory));
                    if let Some(e) = &c.run_in {
                        extra += &format!(" [run-in: {e}]");
                    }
                    if !c.uses.is_empty() {
                        extra += &format!(" [uses: {}]", c.uses.join(", "));
                    }
                    println!("{}{extra}", c.display());
                }
                return Ok(None);
            }

            let slots: usize = hosts.iter().map(|h| h.slots).sum();
            reporter.status(
                "Starting",
                &format!(
                    "{} tests across {slots} slots, keeping {} free per host ({skipped} skipped)",
                    cases.len(),
                    format_size(profile.memory_reserve)
                ),
            );
            let ctx = Arc::new(Ctx {
                fixtures,
                keep_fixtures: args.keep_fixtures,
                memory_reserve: profile.memory_reserve,
                reporter,
                measured: Default::default(),
            });
            tokio::select! {
                _ = runner::run_all(ctx.clone(), &hosts, cases) => {}
                _ = tokio::signal::ctrl_c() => {
                    ctx.reporter.warn("interrupted, cleaning up");
                    return Ok(Some(ExitCode::from(130)));
                }
            }
            let ok = ctx.reporter.summary(skipped);
            let measured = std::mem::take(&mut *ctx.measured.lock().unwrap());
            if let Err(e) = history.save(measured) {
                ctx.reporter.warn(&format!("could not save memory history: {e:#}"));
            }
            Ok(Some(ExitCode::from(if ok { 0 } else { 100 })))
        }
        .await;

        // Always shut agents down, so they tear down fixtures and sandboxes.
        for h in &hosts {
            let kept = std::mem::take(&mut *h.kept.lock().unwrap());
            for (_, msg) in &kept {
                eprintln!("{msg}");
            }
            if let Err(e) = h.agent.shutdown(kept.into_iter().map(|(id, _)| id).collect()).await {
                eprintln!("warning: {e:#}");
            }
        }
        Ok(result?.unwrap_or(ExitCode::SUCCESS))
    })
}

fn select_hosts(cfg: &Config, wanted: &[String]) -> Result<Vec<Host>> {
    let all = if cfg.host.is_empty() {
        vec![Host { name: "local".into(), ssh: None, slots: None, root: None, ssh_program: None, agent: None }]
    } else {
        cfg.host.clone()
    };
    for w in wanted {
        if !all.iter().any(|h| &h.name == w) {
            bail!("no host named `{w}`");
        }
    }
    Ok(all.into_iter().filter(|h| wanted.is_empty() || wanted.contains(&h.name)).collect())
}

/// Maps a target triple to `std::env::consts::ARCH` naming.
fn triple_arch(triple: &str) -> String {
    let a = triple.split('-').next().unwrap_or("");
    if a.starts_with('i') && a.ends_with("86") {
        "x86".into()
    } else if a.starts_with("arm") || a.starts_with("thumb") {
        "arm".into()
    } else if a.starts_with("riscv64") {
        "riscv64".into()
    } else {
        a.into()
    }
}

fn check_host(agent: &client::Agent, target: Option<&str>, reporter: &Reporter) -> Result<()> {
    let want = target.map(triple_arch).unwrap_or_else(|| std::env::consts::ARCH.to_string());
    if agent.info.arch != want {
        bail!(
            "host `{}` is {} but the tests are built for {want}; set `target` in the profile or pass --target",
            agent.name,
            agent.info.arch
        );
    }
    if !agent.info.mirroring {
        reporter.warn(&format!(
            "host `{}` does not allow user namespaces: tests run under {} instead of their original paths, \
             so paths from env!(\"CARGO_MANIFEST_DIR\") may not resolve",
            agent.name,
            agent.info.run_dir.join("root").display()
        ));
    }
    Ok(())
}

fn fixture_defs(cfg: &Config, ws_root: &std::path::Path, files: &files::FileSet) -> Result<HashMap<String, FixtureDef>> {
    let port_re = regex::Regex::new(r"\{port:(\d+)\}").unwrap();
    let mut out = HashMap::new();
    for (name, f) in &cfg.fixture {
        let spec = match f.kind {
            FixtureKind::Docker => {
                let image = image_source(name, f.image.as_ref(), f.build.as_ref(), ws_root, files)?;
                let mut ports = f.ports.clone();
                for v in f.export.values() {
                    for c in port_re.captures_iter(v) {
                        ports.push(c[1].parse().context("bad port number")?);
                    }
                }
                let ready = match &f.ready {
                    None => None,
                    Some(r) => Some(Ready {
                        tcp: r.tcp,
                        cmd: r.cmd.clone().map(|c| c.into_argv()),
                        timeout: r.timeout.as_deref().map(config::parse_duration).transpose()?.unwrap_or(Duration::from_secs(30)),
                    }),
                };
                FixtureSpec::Docker(DockerSpec { image, env: f.env.clone(), ports, ready, export: f.export.clone() })
            }
            FixtureKind::K3d => FixtureSpec::K3d(K3dSpec {
                servers: f.servers.unwrap_or(1),
                agents: f.agents.unwrap_or(0),
                image: f.image.clone(),
                args: f.args.clone(),
                timeout: f.timeout.as_deref().map(config::parse_duration).transpose()?.unwrap_or(Duration::from_secs(300)),
                ready: match &f.ready {
                    None => None,
                    Some(r) => Some(Ready {
                        tcp: None,
                        cmd: r.cmd.clone().map(|c| c.into_argv()),
                        timeout: r.timeout.as_deref().map(config::parse_duration).transpose()?.unwrap_or(Duration::from_secs(120)),
                    }),
                },
                export: f.export.clone(),
                network: f.network.clone(),
                runner: match &f.runner {
                    Some(r) => Some(image_source(&format!("{name}-runner"), r.image.as_ref(), r.build.as_ref(), ws_root, files)?),
                    None => None,
                },
            }),
            FixtureKind::Command => FixtureSpec::Command(CommandSpec {
                setup: f.setup.clone().unwrap(),
                teardown: f.teardown.clone(),
                cwd: ws_root.to_owned(),
            }),
        };
        out.insert(name.clone(), FixtureDef { scope: f.scope, spec });
    }
    Ok(out)
}

/// An image to pull, or one to build from a Dockerfile in the workspace.
/// Built images are tagged with a hash of the Dockerfile and its context.
fn image_source(
    name: &str,
    image: Option<&String>,
    build: Option<&config::BuildCfg>,
    ws_root: &std::path::Path,
    files: &files::FileSet,
) -> Result<ImageSource> {
    let b = match (image, build) {
        (Some(i), _) => return Ok(ImageSource::Pull(i.clone())),
        (None, Some(b)) => b,
        (None, None) => bail!("fixture `{name}`: set `image` or `build`"),
    };
    let dockerfile = ws_root.join(&b.dockerfile);
    let context = ws_root.join(b.context.clone().unwrap_or_else(|| b.dockerfile.parent().map(|p| p.to_owned()).unwrap_or_default()));
    let df = files.files.get(&dockerfile).with_context(|| {
        format!("fixture `{name}`: {} is not synced (missing or gitignored?)", dockerfile.display())
    })?;
    let mut h = Sha256::new();
    h.update(df.hash.0);
    for (p, e) in files.files.range(context.clone()..) {
        if !p.starts_with(&context) {
            break;
        }
        h.update(p.strip_prefix(&context).unwrap().as_os_str().as_encoded_bytes());
        h.update(e.hash.0);
    }
    let digest = hex::encode(h.finalize());
    let repo: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    Ok(ImageSource::Build { dockerfile, context, tag: format!("xtest/{repo}:{}", &digest[..16]) })
}
