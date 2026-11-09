//! `.config/xtest.toml`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::filter::Filter;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub profile: BTreeMap<String, Profile>,
    #[serde(default)]
    pub fixture: BTreeMap<String, Fixture>,
    #[serde(default)]
    pub host: Vec<Host>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Profile {
    pub target: Option<String>,
    pub include: Option<Vec<String>>,
    pub timeout: Option<String>,
    /// Memory assumed for a test that has not been measured yet.
    pub memory: Option<String>,
    /// Memory kept free on every host; tests only start while this much remains.
    pub memory_reserve: Option<String>,
    #[serde(default)]
    pub overrides: Vec<Override>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Override {
    pub filter: String,
    pub run_in: Option<String>,
    #[serde(default)]
    pub uses: Vec<String>,
    pub include_ignored: Option<bool>,
    pub timeout: Option<String>,
    /// Memory this test needs; wins over the measured peak.
    pub memory: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Test,
    Binary,
    Run,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FixtureKind {
    Docker,
    Command,
    K3d,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Fixture {
    pub kind: FixtureKind,
    #[serde(default)]
    pub scope: Scope,
    // docker
    pub image: Option<String>,
    pub build: Option<BuildCfg>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub ports: Vec<u16>,
    pub ready: Option<ReadyCfg>,
    #[serde(default)]
    pub export: BTreeMap<String, String>,
    // command
    pub setup: Option<String>,
    pub teardown: Option<String>,
    // k3d
    pub servers: Option<u32>,
    pub agents: Option<u32>,
    #[serde(default)]
    pub args: Vec<String>,
    /// How long cluster creation may take.
    pub timeout: Option<String>,
    /// Container tests run in when a k3d cluster is their environment.
    pub runner: Option<RunnerCfg>,
    /// Docker network shared with other k3d fixtures that name the same one.
    pub network: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerCfg {
    pub image: Option<String>,
    pub build: Option<BuildCfg>,
}

impl Fixture {
    /// Names of the fields that are set, for checking they fit the kind.
    fn set_fields(&self) -> Vec<&'static str> {
        let mut f = Vec::new();
        let mut add = |set: bool, name| if set { f.push(name) };
        add(self.image.is_some(), "image");
        add(self.build.is_some(), "build");
        add(!self.env.is_empty(), "env");
        add(!self.ports.is_empty(), "ports");
        add(self.ready.is_some(), "ready");
        add(!self.export.is_empty(), "export");
        add(self.setup.is_some(), "setup");
        add(self.teardown.is_some(), "teardown");
        add(self.servers.is_some(), "servers");
        add(self.agents.is_some(), "agents");
        add(!self.args.is_empty(), "args");
        add(self.timeout.is_some(), "timeout");
        add(self.runner.is_some(), "runner");
        add(self.network.is_some(), "network");
        f
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildCfg {
    pub dockerfile: PathBuf,
    pub context: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyCfg {
    pub tcp: Option<u16>,
    pub cmd: Option<StringOrList>,
    pub timeout: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    String(String),
    List(Vec<String>),
}

impl StringOrList {
    pub fn into_argv(self) -> Vec<String> {
        match self {
            StringOrList::String(s) => vec!["sh".into(), "-c".into(), s],
            StringOrList::List(l) => l,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Host {
    pub name: String,
    /// `user@host`. Without it the host is this machine.
    pub ssh: Option<String>,
    pub slots: Option<usize>,
    /// Agent data directory on the host.
    pub root: Option<String>,
    pub ssh_program: Option<Vec<String>>,
    /// Local path of the agent binary to install on the host.
    pub agent: Option<PathBuf>,
}

/// A profile with the default profile's values filled in and filters parsed.
pub struct Resolved {
    pub target: Option<String>,
    pub include: Vec<String>,
    pub timeout: Option<Duration>,
    pub memory: u64,
    pub memory_reserve: u64,
    pub overrides: Vec<(Filter, Override)>,
}

pub const DEFAULT_MEMORY: u64 = 256 << 20;
pub const DEFAULT_MEMORY_RESERVE: u64 = 512 << 20;

impl Config {
    pub fn load(workspace_root: &Path, explicit: Option<&Path>) -> Result<Config> {
        let path = match explicit {
            Some(p) => p.to_owned(),
            None => workspace_root.join(".config/xtest.toml"),
        };
        if explicit.is_none() && !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate().with_context(|| format!("in {}", path.display()))?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for (name, f) in &self.fixture {
            let allowed: &[&str] = match f.kind {
                FixtureKind::Docker => &["image", "build", "env", "ports", "ready", "export"],
                FixtureKind::Command => &["setup", "teardown"],
                FixtureKind::K3d => &["image", "servers", "agents", "args", "timeout", "ready", "export", "runner", "network"],
            };
            if let Some(bad) = f.set_fields().into_iter().find(|x| !allowed.contains(x)) {
                bail!("fixture `{name}`: `{bad}` is not supported by {:?} fixtures", f.kind);
            }
            if let (FixtureKind::K3d, Some(r)) = (f.kind, &f.ready) {
                if r.tcp.is_some() {
                    bail!("fixture `{name}`: k3d fixtures support `ready.cmd`, not `ready.tcp`");
                }
            }
            match f.kind {
                FixtureKind::Docker => {
                    if f.image.is_some() == f.build.is_some() {
                        bail!("fixture `{name}`: set exactly one of `image` or `build`");
                    }
                }
                FixtureKind::Command => {
                    if f.setup.is_none() {
                        bail!("fixture `{name}`: command fixtures need `setup`");
                    }
                }
                FixtureKind::K3d => {
                    if let Some(n) = &f.network {
                        let ok = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
                        if n.is_empty() || n.len() > 32 || !n.chars().all(ok) {
                            bail!("fixture `{name}`: `network` must be 1-32 letters, digits, `-` or `_`");
                        }
                    }
                    if let Some(r) = &f.runner {
                        if r.image.is_some() == r.build.is_some() {
                            bail!("fixture `{name}`: set exactly one of `runner.image` or `runner.build`");
                        }
                    }
                }
            }
        }
        for (pname, p) in &self.profile {
            for o in &p.overrides {
                for f in o.run_in.iter().chain(&o.uses) {
                    if !self.fixture.contains_key(f) {
                        bail!("profile `{pname}`: unknown fixture `{f}`");
                    }
                }
                if let Some(f) = o.run_in.as_ref().and_then(|n| self.fixture.get(n).map(|f| (n, f))) {
                    match f.1.kind {
                        FixtureKind::Command => bail!("profile `{pname}`: command fixture `{}` cannot be used with `run-in`", f.0),
                        FixtureKind::K3d if f.1.runner.is_none() => bail!(
                            "profile `{pname}`: k3d fixture `{}` needs `runner = {{ image = \"...\" }}` to be used with `run-in`",
                            f.0
                        ),
                        _ => {}
                    }
                }
            }
        }
        let mut names = std::collections::HashSet::new();
        for h in &self.host {
            if !names.insert(&h.name) {
                bail!("host `{}` is defined twice", h.name);
            }
        }
        Ok(())
    }

    pub fn resolve(&self, name: &str) -> Result<Resolved> {
        let default = self.profile.get("default").cloned().unwrap_or_default();
        let p = match name {
            "default" => default.clone(),
            _ => self.profile.get(name).cloned().with_context(|| format!("no profile `{name}`"))?,
        };
        let timeout = p.timeout.clone().or(default.timeout.clone());
        let memory = p.memory.clone().or(default.memory.clone());
        let reserve = p.memory_reserve.clone().or(default.memory_reserve.clone());
        let mut overrides = Vec::new();
        let chain = if name == "default" { vec![&p] } else { vec![&default, &p] };
        for prof in chain {
            for o in &prof.overrides {
                let f = Filter::parse(&o.filter).with_context(|| format!("filter `{}`", o.filter))?;
                overrides.push((f, o.clone()));
            }
        }
        Ok(Resolved {
            target: p.target.clone().or(default.target.clone()),
            include: p.include.clone().or(default.include.clone()).unwrap_or_default(),
            timeout: timeout.as_deref().map(parse_duration).transpose()?,
            memory: memory.as_deref().map(parse_size).transpose()?.unwrap_or(DEFAULT_MEMORY),
            memory_reserve: reserve.as_deref().map(parse_size).transpose()?.unwrap_or(DEFAULT_MEMORY_RESERVE),
            overrides,
        })
    }
}

/// Parses `500ms`, `30s`, `2m`, `1h`, or a bare number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().with_context(|| format!("bad duration `{s}`"))?;
    let secs = match unit.trim() {
        "ms" => n / 1000.0,
        "" | "s" => n,
        "m" => n * 60.0,
        "h" => n * 3600.0,
        _ => bail!("bad duration unit in `{s}`"),
    };
    Ok(Duration::from_secs_f64(secs))
}

/// Parses `512M`, `4G`, `1.5GiB`, `100KB`, or a bare number of bytes.
/// Units are binary: `M`, `MB` and `MiB` all mean 1024² bytes.
pub fn parse_size(s: &str) -> Result<u64> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().with_context(|| format!("bad size `{s}`"))?;
    let unit = unit.trim().to_ascii_uppercase();
    let shift = match unit.trim_end_matches("IB").trim_end_matches('B') {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => bail!("bad size unit in `{s}`"),
    };
    Ok((n * (1u64 << shift) as f64) as u64)
}

pub fn format_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= (1u64 << 30) as f64 {
        format!("{:.1} GiB", b / (1u64 << 30) as f64)
    } else {
        format!("{:.1} MiB", b / (1u64 << 20) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("512M").unwrap(), 512 << 20);
        assert_eq!(parse_size("4G").unwrap(), 4 << 30);
        assert_eq!(parse_size("1.5GiB").unwrap(), 3 << 29);
        assert_eq!(parse_size("100kb").unwrap(), 100 << 10);
        assert_eq!(parse_size("1000").unwrap(), 1000);
        assert!(parse_size("5 bananas").is_err());
        assert_eq!(format_size(3 << 29), "1.5 GiB");
    }

    #[test]
    fn rejects_fields_of_other_kinds() {
        let cfg: Config = toml::from_str("[fixture.x]\nkind = \"k3d\"\nsetup = \"true\"\n").unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("`setup` is not supported"), "{err}");
        let cfg: Config = toml::from_str("[fixture.x]\nkind = \"docker\"\nimage = \"a\"\nagents = 2\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn k3d_run_in_needs_a_runner() {
        let base = "[fixture.k8s]\nkind = \"k3d\"\n[[profile.default.overrides]]\nfilter = \"all()\"\nrun-in = \"k8s\"\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert!(cfg.validate().unwrap_err().to_string().contains("needs `runner"));
        let cfg: Config = toml::from_str(&base.replace("kind = \"k3d\"\n", "kind = \"k3d\"\nrunner = { image = \"archlinux\" }\n")).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("60s").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("5").unwrap(), Duration::from_secs(5));
        assert!(parse_duration("5 days").is_err());
    }

    #[test]
    fn parses_the_documented_example() {
        let cfg: Config = toml::from_str(
            r#"
            [profile.default]
            target = "x86_64-unknown-linux-musl"
            include = ["tests/fixtures/**"]
            timeout = "60s"

            [[profile.default.overrides]]
            filter = "package(myapp) & test(=backup::tests::creates_archive)"
            run-in = "borg"
            include-ignored = true

            [fixture.borg]
            kind = "docker"
            build = { dockerfile = "tests/docker/borg.Dockerfile", context = "tests/docker" }

            [fixture.pg]
            kind = "docker"
            image = "postgres:17"
            scope = "run"
            env = { POSTGRES_PASSWORD = "x" }
            ready = { cmd = ["pg_isready", "-U", "postgres"], timeout = "30s" }
            export = { DATABASE_URL = "postgres://postgres:x@{host}:{port:5432}/postgres" }

            [fixture.k8s]
            kind = "k3d"
            scope = "run"
            agents = 1
            args = ["--no-lb"]
            ready = { cmd = "docker exec k3d-{cluster}-server-0 kubectl wait --for=condition=Ready nodes --all" }

            [[host]]
            name = "pi4"
            ssh = "ci@pi4.lan"
            slots = 4
            "#,
        )
        .unwrap();
        cfg.validate().unwrap();
        let p = cfg.resolve("default").unwrap();
        assert_eq!(p.overrides.len(), 1);
        assert_eq!(cfg.fixture["pg"].scope, Scope::Run);
    }
}
