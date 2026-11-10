//! Fixtures written in Rust: reading what `#[xtest::test]`,
//! `#[xtest::fixture]` and `#[xtest::teardown]` embedded in the test
//! binaries (see `xtest_proto::meta`), and checking that it fits together.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use xtest_proto::meta::{self, Record};
use xtest_proto::RustSpec;

use crate::cargo::TestBinary;
use crate::config::{self, Scope};

/// How long a Rust fixture's setup may take by default.
pub const DEFAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(300);

/// A `#[xtest::test]` that uses fixtures.
#[derive(Debug, Clone)]
pub struct TestMeta {
    /// Ids of the fixtures it takes, in parameter order.
    pub fixtures: Vec<String>,
    /// Ignored by the user (`#[ignore]`), not just by the macro.
    pub ignored: bool,
}

/// A `#[xtest::fixture]`, as compiled into one binary.
#[derive(Debug, Clone, PartialEq)]
pub struct FixtureMeta {
    /// `<file>#<name>`: the same in every binary that includes the file.
    pub id: String,
    pub name: String,
    pub scope: Scope,
    pub memory: Option<u64>,
    pub timeout: Option<Duration>,
    pub environment: bool,
    pub deps: Vec<String>,
    /// libtest name of the hidden setup test.
    pub setup_test: String,
}

/// A fixture and the binaries that contain it.
#[derive(Debug, Clone)]
pub struct RustFixture {
    pub meta: FixtureMeta,
    /// Each binary with the fixture's setup, and its teardown test if the
    /// binary has one.
    pub bins: Vec<(Arc<TestBinary>, Option<String>)>,
}

impl RustFixture {
    /// The spec to set it up from `bin` (the binary of the test that needs
    /// it), or from the first binary that has it.
    pub fn spec_for(&self, bin: &TestBinary) -> RustSpec {
        let (b, teardown) = self.bins.iter().find(|(b, _)| b.exe == bin.exe).unwrap_or(&self.bins[0]);
        RustSpec {
            id: self.meta.id.clone(),
            exe: b.exe.clone(),
            setup_test: self.meta.setup_test.clone(),
            teardown_test: teardown.clone(),
            cwd: b.manifest_dir.clone(),
            env: b.env(),
            timeout: self.meta.timeout.unwrap_or(DEFAULT_SETUP_TIMEOUT),
            environment: self.meta.environment,
        }
    }
}

#[derive(Debug, Default)]
pub struct Collected {
    /// By test binary and libtest name.
    pub tests: HashMap<(PathBuf, String), TestMeta>,
    /// By fixture id.
    pub fixtures: BTreeMap<String, RustFixture>,
}

/// Whether a libtest name belongs to a hidden fixture test.
pub fn is_hidden(test: &str) -> bool {
    test.rsplit("::").next().is_some_and(|last| last.starts_with("__xtest_"))
}

/// The libtest name of `item` in module `module` (`module_path!()`, which
/// starts with the crate name that libtest leaves out).
fn libtest_name(module: &str, item: &str) -> String {
    match module.split_once("::") {
        Some((_, rest)) => format!("{rest}::{item}"),
        None => item.to_string(),
    }
}

/// Reads the metadata of every test binary and checks it.
pub fn collect(bins: &[Arc<TestBinary>]) -> Result<Collected> {
    let mut out = Collected::default();
    for bin in bins.iter().filter(|b| b.harness) {
        let data = std::fs::read(&bin.exe).with_context(|| format!("reading {}", bin.exe.display()))?;
        add_binary(&mut out, bin, &meta::scan(&data)).with_context(|| format!("in {}", bin.id()))?;
    }
    check(&out)?;
    Ok(out)
}

fn field<'a>(r: &'a Record, key: &str) -> Result<&'a str> {
    r.get(key).with_context(|| format!("xtest metadata without `{key}`; is cargo-xtest older than the xtest crate?"))
}

fn add_binary(out: &mut Collected, bin: &Arc<TestBinary>, records: &[Record]) -> Result<()> {
    let mut teardowns = Vec::new();
    for r in records {
        match field(r, "kind")? {
            "test" => {
                let name = libtest_name(field(r, "module")?, field(r, "name")?);
                let t = TestMeta { fixtures: r.all("uses").map(String::from).collect(), ignored: r.get("ignored") == Some("1") };
                out.tests.insert((bin.exe.clone(), name), t);
            }
            "fixture" => {
                let m = FixtureMeta {
                    id: field(r, "id")?.to_string(),
                    name: field(r, "name")?.to_string(),
                    scope: match field(r, "scope")? {
                        "test" => Scope::Test,
                        "binary" => Scope::Binary,
                        "run" => Scope::Run,
                        s => bail!("unknown fixture scope `{s}`"),
                    },
                    memory: r.get("memory").map(config::parse_size).transpose()?,
                    timeout: r.get("timeout").map(config::parse_duration).transpose()?,
                    environment: r.get("environment") == Some("1"),
                    deps: r.all("dep").map(String::from).collect(),
                    setup_test: libtest_name(field(r, "module")?, field(r, "test")?),
                };
                match out.fixtures.get_mut(&m.id) {
                    Some(f) => {
                        if f.meta != (FixtureMeta { setup_test: f.meta.setup_test.clone(), ..m.clone() }) {
                            bail!(
                                "fixture `{}` ({}) is compiled differently here than in {}, e.g. with other features",
                                m.name,
                                m.id,
                                f.bins[0].0.id()
                            );
                        }
                        f.bins.push((bin.clone(), None));
                    }
                    None => {
                        out.fixtures.insert(m.id.clone(), RustFixture { meta: m, bins: vec![(bin.clone(), None)] });
                    }
                }
            }
            "teardown" => teardowns.push((field(r, "fixture")?.to_string(), libtest_name(field(r, "module")?, field(r, "test")?))),
            // From a newer xtest: whatever it is, this version can't use it.
            _ => {}
        }
    }
    for (fixture, test) in teardowns {
        let f = out.fixtures.get_mut(&fixture).with_context(|| {
            format!("teardown `{test}` is for fixture `{fixture}`, which is not compiled into this binary")
        })?;
        let entry = f.bins.iter_mut().find(|(b, _)| b.exe == bin.exe).unwrap();
        if let Some(prev) = entry.1.replace(test.clone()) {
            bail!("fixture `{}` has two teardowns: `{prev}` and `{test}`", f.meta.name);
        }
    }
    Ok(())
}

fn scope_rank(s: Scope) -> u8 {
    match s {
        Scope::Test => 0,
        Scope::Binary => 1,
        Scope::Run => 2,
    }
}

fn scope_name(s: Scope) -> &'static str {
    match s {
        Scope::Test => "test",
        Scope::Binary => "binary",
        Scope::Run => "run",
    }
}

/// Checks dependencies: they must exist, live at least as long as the
/// fixtures that need them, not be environments, and not form a cycle.
fn check(c: &Collected) -> Result<()> {
    for f in c.fixtures.values() {
        for d in &f.meta.deps {
            let dep = c.fixtures.get(d).with_context(|| missing(&f.meta.name, d))?;
            if scope_rank(dep.meta.scope) < scope_rank(f.meta.scope) {
                bail!(
                    "fixture `{}` (scope {}) can't depend on `{}` (scope {}): a fixture can only use \
                     fixtures that live at least as long",
                    f.meta.name,
                    scope_name(f.meta.scope),
                    dep.meta.name,
                    scope_name(dep.meta.scope)
                );
            }
            if dep.meta.environment {
                bail!("fixture `{}` can't depend on `{}`: it is an environment", f.meta.name, dep.meta.name);
            }
        }
    }
    for id in c.fixtures.keys() {
        closure(c, std::slice::from_ref(id)).with_context(|| format!("fixture `{}`", c.fixtures[id].meta.name))?;
    }
    Ok(())
}

fn missing(user: &str, id: &str) -> String {
    format!(
        "`{user}` uses fixture `{id}`, but no test binary contains its setup. \
         Fixtures must be defined in a crate compiled as a test (tests/, or #[cfg(test)] code); \
         fixtures in library crates are not supported yet (https://github.com/samvv/cargo-xtest/issues/7)"
    )
}

/// `roots` and everything they depend on, dependencies first.
pub fn closure(c: &Collected, roots: &[String]) -> Result<Vec<String>> {
    fn visit(c: &Collected, id: &str, stack: &mut Vec<String>, out: &mut Vec<String>) -> Result<()> {
        if out.iter().any(|o| o == id) {
            return Ok(());
        }
        if stack.iter().any(|s| s == id) {
            let names: Vec<_> = stack.iter().map(|s| c.fixtures[s].meta.name.as_str()).collect();
            bail!("fixtures depend on each other in a cycle: {} -> {}", names.join(" -> "), c.fixtures[id].meta.name);
        }
        let f = c.fixtures.get(id).with_context(|| missing(stack.last().map_or("a test", |s| &c.fixtures[s].meta.name), id))?;
        stack.push(id.to_string());
        for d in &f.meta.deps {
            visit(c, d, stack, out)?;
        }
        stack.pop();
        out.push(id.to_string());
        Ok(())
    }
    let mut out = Vec::new();
    for r in roots {
        visit(c, r, &mut Vec::new(), &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(name: &str) -> Arc<TestBinary> {
        Arc::new(TestBinary {
            package: "p".into(),
            version: "0".into(),
            name: name.into(),
            kind: "test".into(),
            exe: format!("/t/{name}").into(),
            manifest_dir: "/p".into(),
            harness: true,
        })
    }

    fn rec(pairs: &[(&str, &str)]) -> Record {
        Record { pairs: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    fn fixture(id: &str, scope: &str, deps: &[&str]) -> Record {
        let name = id.rsplit('#').next().unwrap();
        let test = format!("__xtest_fixture_{name}");
        let mut pairs = vec![("kind", "fixture"), ("id", id), ("name", name), ("module", "a::common"), ("test", &test), ("scope", scope)];
        pairs.extend(deps.iter().map(|d| ("dep", *d)));
        rec(&pairs)
    }

    #[test]
    fn reads_tests_fixtures_and_teardowns() {
        let a = bin("a");
        let mut c = Collected::default();
        let records = [
            rec(&[("kind", "test"), ("module", "a"), ("name", "t"), ("ignored", "0"), ("uses", "c.rs#db")]),
            fixture("c.rs#pg", "run", &[]),
            fixture("c.rs#db", "test", &["c.rs#pg"]),
            rec(&[("kind", "teardown"), ("fixture", "c.rs#db"), ("module", "a::common"), ("test", "__xtest_teardown_drop")]),
            rec(&[("kind", "from-the-future")]),
        ];
        add_binary(&mut c, &a, &records).unwrap();
        check(&c).unwrap();
        assert_eq!(c.tests[&(a.exe.clone(), "t".into())].fixtures, ["c.rs#db"]);
        let db = &c.fixtures["c.rs#db"];
        assert_eq!(db.meta.setup_test, "common::__xtest_fixture_db");
        let spec = db.spec_for(&a);
        assert_eq!(spec.teardown_test.as_deref(), Some("common::__xtest_teardown_drop"));
        assert_eq!(closure(&c, &["c.rs#db".into()]).unwrap(), ["c.rs#pg", "c.rs#db"]);
        assert!(is_hidden("common::__xtest_fixture_db"));
        assert!(!is_hidden("common::db"));
    }

    #[test]
    fn a_fixture_in_two_binaries_is_one_fixture() {
        let (a, b) = (bin("a"), bin("b"));
        let mut c = Collected::default();
        add_binary(&mut c, &a, &[fixture("c.rs#pg", "run", &[])]).unwrap();
        add_binary(&mut c, &b, &[fixture("c.rs#pg", "run", &[])]).unwrap();
        assert_eq!(c.fixtures["c.rs#pg"].bins.len(), 2);
        assert_eq!(c.fixtures["c.rs#pg"].spec_for(&b).exe, b.exe);

        let err = add_binary(&mut c, &bin("c"), &[fixture("c.rs#pg", "test", &[])]).unwrap_err();
        assert!(err.to_string().contains("compiled differently"), "{err}");
    }

    #[test]
    fn rejects_bad_dependencies() {
        let check_err = |records: &[Record]| {
            let mut c = Collected::default();
            add_binary(&mut c, &bin("a"), records).unwrap();
            format!("{:#}", check(&c).unwrap_err())
        };
        let e = check_err(&[fixture("c.rs#pg", "test", &[]), fixture("c.rs#app", "run", &["c.rs#pg"])]);
        assert!(e.contains("can't depend on `pg` (scope test)"), "{e}");
        let e = check_err(&[fixture("c.rs#x", "run", &["c.rs#y"]), fixture("c.rs#y", "run", &["c.rs#x"])]);
        assert!(e.contains("cycle"), "{e}");
        let e = check_err(&[fixture("c.rs#x", "run", &["lib.rs#y"])]);
        assert!(e.contains("no test binary contains its setup"), "{e}");
    }
}
