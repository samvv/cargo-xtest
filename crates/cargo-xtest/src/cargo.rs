//! Building test binaries with cargo and describing them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::BufReader;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use cargo_metadata::{Message, MetadataCommand};

#[derive(Debug, Default, Clone)]
pub struct CargoArgs {
    pub packages: Vec<String>,
    pub workspace: bool,
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub release: bool,
    pub target: Option<String>,
}

impl CargoArgs {
    fn to_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        for p in &self.packages {
            a.extend(["--package".into(), p.clone()]);
        }
        if self.workspace {
            a.push("--workspace".into());
        }
        if !self.features.is_empty() {
            a.extend(["--features".into(), self.features.join(",")]);
        }
        if self.all_features {
            a.push("--all-features".into());
        }
        if self.no_default_features {
            a.push("--no-default-features".into());
        }
        if self.release {
            a.push("--release".into());
        }
        if let Some(t) = &self.target {
            a.extend(["--target".into(), t.clone()]);
        }
        a
    }
}

pub struct Workspace {
    pub root: PathBuf,
    pub target_dir: PathBuf,
}

pub fn workspace() -> Result<Workspace> {
    let md = MetadataCommand::new().no_deps().exec().context("running cargo metadata")?;
    Ok(Workspace {
        root: md.workspace_root.into_std_path_buf(),
        target_dir: md.target_directory.into_std_path_buf(),
    })
}

#[derive(Debug, Clone)]
pub struct TestBinary {
    pub package: String,
    pub version: String,
    /// Target name, e.g. `integration` for `tests/integration.rs`.
    pub name: String,
    /// `lib`, `bin`, `test`, `bench`, `example` or `proc-macro`.
    pub kind: String,
    pub exe: PathBuf,
    pub manifest_dir: PathBuf,
    /// False for `harness = false` targets: they can't list their tests.
    pub harness: bool,
}

impl TestBinary {
    /// Short display id, like nextest: `pkg`, `pkg::integration`, `pkg::bin/tool`.
    pub fn id(&self) -> String {
        match self.kind.as_str() {
            "lib" | "proc-macro" => self.package.clone(),
            "test" => format!("{}::{}", self.package, self.name),
            k => format!("{}::{k}/{}", self.package, self.name),
        }
    }

    /// Env vars cargo sets when it runs a test binary.
    pub fn env(&self) -> BTreeMap<String, String> {
        [
            ("CARGO_MANIFEST_DIR", self.manifest_dir.display().to_string()),
            ("CARGO_PKG_NAME", self.package.clone()),
            ("CARGO_PKG_VERSION", self.version.clone()),
            ("CARGO_CRATE_NAME", self.name.replace('-', "_")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}

pub struct Built {
    pub tests: Vec<TestBinary>,
    /// Every executable cargo produced (tests and the binaries they may run
    /// through `CARGO_BIN_EXE_*`).
    pub executables: Vec<PathBuf>,
}

pub fn build(args: &CargoArgs) -> Result<Built> {
    let md = MetadataCommand::new()
        .no_deps()
        .exec()
        .context("running cargo metadata")?;
    let packages: HashMap<_, _> = md.packages.iter().map(|p| (p.id.clone(), p)).collect();
    let mut no_harness: HashMap<String, HashSet<String>> = HashMap::new();
    for p in &md.packages {
        no_harness.insert(p.name.to_string(), harness_false_targets(p.manifest_path.as_std_path())?);
    }

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut child = Command::new(cargo)
        .args(["test", "--no-run", "--message-format=json-render-diagnostics"])
        .args(args.to_args())
        .stdout(Stdio::piped())
        .spawn()
        .context("running cargo test --no-run")?;
    let reader = BufReader::new(child.stdout.take().unwrap());

    let mut tests = Vec::new();
    let mut executables = Vec::new();
    for msg in Message::parse_stream(reader) {
        let Message::CompilerArtifact(art) = msg? else { continue };
        let Some(exe) = art.executable else { continue };
        let exe = exe.into_std_path_buf();
        executables.push(exe.clone());
        if !art.profile.test {
            continue;
        }
        let pkg = packages.get(&art.package_id).context("artifact from unknown package")?;
        let kinds: Vec<String> = art.target.kind.iter().map(|k| k.to_string()).collect();
        let kind = ["test", "bench", "bin", "example", "proc-macro"]
            .into_iter()
            .find(|k| kinds.iter().any(|x| x == k))
            .unwrap_or("lib")
            .to_string();
        let name = art.target.name.clone();
        let harness = !(matches!(kind.as_str(), "test" | "bench")
            && no_harness.get(pkg.name.as_str()).is_some_and(|s| s.contains(&name)));
        tests.push(TestBinary {
            package: pkg.name.to_string(),
            version: pkg.version.to_string(),
            name,
            kind,
            exe,
            manifest_dir: pkg.manifest_path.parent().unwrap().as_std_path().to_owned(),
            harness,
        });
    }
    if !child.wait()?.success() {
        bail!("build failed");
    }
    tests.sort_by_key(|t| t.id());
    Ok(Built { tests, executables })
}

/// Names of `[[test]]`/`[[bench]]` targets with `harness = false`.
fn harness_false_targets(manifest: &std::path::Path) -> Result<HashSet<String>> {
    let text = std::fs::read_to_string(manifest)?;
    let doc: toml::Value = toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
    let mut out = HashSet::new();
    for section in ["test", "bench"] {
        for t in doc.get(section).and_then(|v| v.as_array()).into_iter().flatten() {
            let harness = t.get("harness").and_then(|h| h.as_bool()).unwrap_or(true);
            if let (false, Some(name)) = (harness, t.get("name").and_then(|n| n.as_str())) {
                out.insert(name.to_string());
            }
        }
    }
    Ok(out)
}
