# cargo-xtest

A test runner for Cargo that runs tests on other hosts and inside fixtures
such as Docker containers.

- **Remote hosts:** runs tests on other machines over SSH, at their original paths.
- **Fixtures:** Docker containers, k3d clusters and setup scripts, always torn down afterwards.
- **Memory-aware scheduling:** starts tests in parallel only while they fit in free memory.
- **Fast sync:** uploads only changed files to a content-addressed store.
- **Isolation:** one process per test, with its own temp directory and timeout.
- **Self-cleaning:** removes leftovers after crashes and keeps caches under a size cap.

```rust
use xtest::fixture::{Docker, Ready, Setup, Teardown};

#[derive(serde::Serialize, serde::Deserialize)]
struct Postgres {
    url: String,
}

/// One Postgres container for the whole run.
#[xtest::fixture(scope = "run", memory = "256M")]
async fn postgres(cx: &mut Setup) -> xtest::Result<Postgres> {
    let c = cx
        .docker(
            Docker::image("postgres:17")
                .env("POSTGRES_PASSWORD", "x")
                .port(5432)
                .ready(Ready::cmd(["pg_isready", "-U", "postgres"])),
        )
        .await?;
    Ok(Postgres { url: format!("postgres://postgres:x@{}/postgres", c.addr(5432)) })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Db {
    url: String,
    name: String,
    admin_url: String,
}

/// A fresh, migrated database for every test.
#[xtest::fixture]
async fn db(cx: &mut Setup, postgres: &Postgres) -> xtest::Result<Db> {
    let name = format!("test_{}", cx.instance_id().replace('-', "_"));
    myapp::db::create_database(&postgres.url, &name).await?;
    let url = postgres.url.replace("/postgres", &format!("/{name}"));
    myapp::db::migrate(&url).await?;
    Ok(Db { url, name, admin_url: postgres.url.clone() })
}

#[xtest::teardown(db)]
async fn drop_db(cx: &mut Teardown, db: Db) -> xtest::Result<()> {
    if cx.failed() {
        cx.attach("users.txt", myapp::db::dump_table(&db.url, "users").await?)?;
    }
    myapp::db::drop_database(&db.admin_url, &db.name).await?;
    Ok(())
}

#[xtest::test]
fn signs_up(db: &Db) {
    let app = myapp::App::connect(&db.url).unwrap();
    app.sign_up("alice@example.com").unwrap();
    assert!(app.user("alice@example.com").is_some());
}
```

## Installation

```
cargo install --git https://github.com/samvv/cargo-xtest cargo-xtest
```

This installs two binaries into `~/.cargo/bin`: `cargo-xtest` and
`xtest-agent`. Keep them together: cargo-xtest copies its own agent to each
host. From a checkout, use `cargo install --path crates/cargo-xtest`.

Then run `cargo xtest` in any Cargo workspace.

**Requirements:**

- **Your machine:** Linux, with Rust and Cargo.
- **Test hosts:** Linux, reachable with `ssh` without a password prompt.
  Nothing needs to be installed there. A host needs the same architecture as
  your machine, and a glibc at least as new as yours. It also needs user
  namespaces, so tests can see their files at the original paths.
- **For fixtures:** Docker on the host for [Docker fixtures](docs/fixtures/docker.md),
  plus `k3d` for [k3d fixtures](docs/fixtures/k3d.md).

## How it works

cargo-xtest builds your tests locally, uploads what changed to each host, and
runs every test in its own process there. Tests see their files at the
original paths, so `env!("CARGO_MANIFEST_DIR")` and `CARGO_BIN_EXE_*` keep
working. Hosts only need SSH; the agent installs itself on first use. See
[docs/internals.md](docs/internals.md) for the details.

## Configuration

cargo-xtest reads `.config/xtest.toml` in the workspace root. Without it,
tests run on this machine with no fixtures.

```toml
# A Postgres container, shared by all tests of the run.
[fixture.pg]
kind = "docker"
image = "postgres:17"
scope = "run"
env = { POSTGRES_PASSWORD = "x" }
ready = { cmd = ["pg_isready", "-U", "postgres"] }
export = { DATABASE_URL = "postgres://postgres:x@{host}:{port:5432}/postgres" }

# The tests in tests/db.rs use it.
[[profile.default.overrides]]
filter = "binary(db)"
uses = ["pg"]

# Run the tests on build-box, over SSH, instead of locally.
[[host]]
name = "build-box"
ssh = "ci@build-box.lan"
```

The tests find the database through the exported variable:

```rust
// tests/db.rs
#[test]
fn connects() {
    let url = std::env::var("DATABASE_URL").unwrap();
    // ...
}
```

Put your own settings, such as your test hosts, in
`.config/xtest.local.toml` and add it to `.gitignore`. It is merged over the
shared file; see [local overrides](docs/configuration.md#local-overrides-xtestlocaltoml).

The [documentation](docs/README.md) covers the rest:
[every configuration key](docs/configuration.md), [filters](docs/filters.md),
[fixtures](docs/fixtures/README.md) ([Docker](docs/fixtures/docker.md),
[command](docs/fixtures/command.md), [k3d](docs/fixtures/k3d.md), [Rust](docs/fixtures/rust.md)),
[memory scheduling](docs/memory-scheduling.md) and [cleanup](docs/cleanup.md).

## Writing tests

Plain `#[test]` functions work as they are. The optional `xtest` crate adds
[`#[xtest::test]`](docs/xtest-test.md), a drop-in replacement for `#[test]`,
and [fixtures written in Rust](docs/fixtures/rust.md): async functions that
start containers, clusters and processes through cargo-xtest, with your own
setup and teardown logic around them.

## CLI

### `cargo xtest`

Builds the tests, runs them on the hosts and reports the results.

```
cargo xtest [OPTIONS] [FILTERS]...
```

| Flag | Description |
|---|---|
| `FILTERS...` | Run only tests whose name contains one of these strings |
| `-E`, `--filter-expr <EXPR>` | Run only tests matching this [filter expression](docs/filters.md), e.g. `package(db) & test(/integration/)`. Repeatable: a test runs if any expression matches |
| `-p`, `--package <PKG>` | Test only this package. Repeatable |
| `--workspace` | Test every package in the workspace |
| `-F`, `--features <FEATURES>` | Comma-separated features to enable |
| `--all-features` | Enable all features |
| `--no-default-features` | Disable the default features |
| `-r`, `--release` | Build in release mode |
| `--target <TRIPLE>` | Target triple to build for (overrides the profile's `target`) |
| `--profile <NAME>` | Profile from `.config/xtest.toml` (default: `default`) |
| `--host <NAME>` | Run only on this host. Repeatable |
| `--ignored` | Run only ignored tests |
| `--include-ignored` | Run ignored tests too |
| `--retries <N>` | Rerun a failing test up to `N` times; overrides `retries` in the config. A test that passes on a retry is reported as `FLAKY` |
| `--fail-fast` | Start no new tests after the first failure; running tests finish, the rest are reported as not run |
| `--keep-fixtures` | Keep the test-scoped fixtures of failed tests, and print how to inspect them (e.g. `docker exec -it ... sh`); see [fixtures](docs/fixtures/README.md#debugging---keep-fixtures) |
| `--list` | List the tests that would run, in the order they would start, with their memory and duration estimates, then exit |
| `--no-capture` | Show test output live, line by line, prefixed with the host and test. stderr lines are marked with `!` instead of `\|`. Alias: `--nocapture` |
| `--success-output` | Also show the output of passing tests (by default, output is shown only for failures) |
| `-j`, `--jobs <N>` | Run at most `N` tests at once on each host; `-j1` keeps `--no-capture` output from interleaving |
| `--rehash` | Hash every file again instead of trusting the [hash cache](docs/internals.md#hash-cache) |
| `--config <FILE>` | Config file to use instead of `.config/xtest.toml` in the workspace root. `<name>.local.toml` next to it is merged over it. |

Exit codes: 0 when all tests pass, 100 when any test fails, 1 on errors,
130 when interrupted.

A filter that is literally `clean` must be written as `-E 'test(clean)'`.

### `cargo xtest clean`

Removes everything on the hosts that no running test run uses: kept runs and
their fixtures, store blobs, old agent builds, cached tools and built images.
See [docs/cleanup.md](docs/cleanup.md).

```
cargo xtest clean [OPTIONS]
```

| Flag | Description |
|---|---|
| `--host <NAME>` | Clean only this host. Repeatable |
| `--config <FILE>` | Config file to use instead of `.config/xtest.toml` in the workspace root |

Fixtures kept with `--keep-fixtures` can also be released one run at a time:
delete the run's `keep` file under `<root>/runs/`, and the next run cleans it up.
