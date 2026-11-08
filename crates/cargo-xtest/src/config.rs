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
    pub overrides: Vec<(Filter, Override)>,
}

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
            }
        }
        for (pname, p) in &self.profile {
            for o in &p.overrides {
                for f in o.run_in.iter().chain(&o.uses) {
                    if !self.fixture.contains_key(f) {
                        bail!("profile `{pname}`: unknown fixture `{f}`");
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

#[cfg(test)]
mod tests {
    use super::*;

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
