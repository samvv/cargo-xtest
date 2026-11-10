---
name: xtest
description: Write and run Rust tests that need fixtures (Docker containers, k3d clusters, helper processes, mock servers) with cargo-xtest and the `xtest` crate. Use when adding integration tests that need a database, service, container or Kubernetes cluster; when converting tests to `#[xtest::test]`; when writing `#[xtest::fixture]`s; or when running tests on remote hosts with `cargo xtest`.
---

# Testing with cargo-xtest and `xtest`

cargo-xtest is a test runner for Cargo. It builds the tests locally, runs
each test in its own process (on this machine or on SSH hosts), and sets up
the fixtures tests need beforehand and tears them down afterwards. The
`xtest` crate lets you write those fixtures in Rust, next to the tests.

Docs: https://github.com/samvv/cargo-xtest (`docs/fixtures/rust.md` is the
full reference for Rust fixtures).

## Setup

```sh
cargo install --git https://github.com/samvv/cargo-xtest cargo-xtest
```

This installs `cargo-xtest` and `xtest-agent`; keep them together. Needs
Linux, and Docker on the test host for container fixtures (plus `k3d` for
clusters).

```toml
# Cargo.toml of the package with the tests
[dev-dependencies]
xtest = { git = "https://github.com/samvv/cargo-xtest" }
serde = { version = "1", features = ["derive"] }
# Only if fixture code itself uses tokio (listeners, sleeps, ...):
tokio = { version = "1", features = ["net", "io-util", "macros", "time"] }
```

Run with `cargo xtest`. Plain `cargo test` still works, but skips every test
that takes fixtures (they show as ignored).

## Writing a fixture and a test

Put shared fixtures in `tests/common/mod.rs` and include it with
`mod common;` from each test file that needs them:

```rust
// tests/common/mod.rs
#![allow(dead_code)] // not every test binary uses every fixture

use serde::{Deserialize, Serialize};
use xtest::fixture::{Docker, Ready, Setup, Teardown};

#[derive(Serialize, Deserialize)]
pub struct Postgres {
    pub url: String,
}

/// One container for the whole run.
#[xtest::fixture(scope = "run", memory = "256M")]
pub async fn postgres(cx: &mut Setup) -> xtest::Result<Postgres> {
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

#[derive(Serialize, Deserialize)]
pub struct Db {
    pub url: String,
    pub name: String,
    pub admin_url: String,
}

/// A database per test, on the shared server. Depends on `postgres`.
#[xtest::fixture]
pub async fn db(cx: &mut Setup, postgres: &Postgres) -> xtest::Result<Db> {
    let name = format!("test_{}", cx.instance_id().replace('-', "_"));
    create_database(&postgres.url, &name).await?;
    let url = postgres.url.replace("/postgres", &format!("/{name}"));
    Ok(Db { url, name, admin_url: postgres.url.clone() })
}

#[xtest::teardown(db)]
pub async fn drop_db(cx: &mut Teardown, db: Db) -> xtest::Result<()> {
    if cx.failed() {
        cx.attach("tables.txt", dump_tables(&db.url).await?)?;
    }
    drop_database(&db.admin_url, &db.name).await?;
    Ok(())
}
```

```rust
// tests/signup.rs
mod common;
use common::*;

#[xtest::test]
fn signs_up(db: &Db) {
    let app = myapp::App::connect(&db.url).unwrap();
    // ...
}
```

A test parameter *is* a fixture: by default the fixture function with the
parameter's name, otherwise the one named with `#[fixture(path)]`:

```rust
#[xtest::test]
fn copies(db: &Db, #[fixture(common::postgres)] pg: Postgres) { ... }
```

Take values as `&T` or `T`. A wrong name or type is a compile error. A test
gets its fixtures plus everything they depend on.

## `#[xtest::fixture]` reference

```rust
#[xtest::fixture(scope = "run", memory = "512M", timeout = "2m", environment)]
async fn name(cx: &mut Setup, dep_a: &A, dep_b: B) -> xtest::Result<Value>
```

| Argument | Values | Default |
|---|---|---|
| `scope` | `"test"` (one per test), `"binary"` (one per test binary and host), `"run"` (one per host) | `"test"` |
| `memory` | e.g. `"256M"`, `"2G"`: what one instance needs, for scheduling | measured peak of its containers, else 0 |
| `timeout` | e.g. `"30s"`, `"5m"`: how long setup may take | `"5m"` |
| `environment` | tests run *inside* the container this fixture makes | off |

- `&mut Setup` is optional and must be the first parameter. Every other
  parameter is a dependency on another fixture.
- A fixture can only depend on fixtures with the same or a longer scope
  (`run` ≥ `binary` ≥ `test`), and not on an environment. Cycles are
  rejected.
- The return type must be `Result<T, E>` with `E: Display` (use
  `xtest::Result<T>`; `?` works with any `std::error::Error`).
- `T` must be `Serialize + DeserializeOwned`. The value is sent as JSON to
  every test process, so store **addresses, URLs, names and paths**, never
  connections, handles or clients. Tests open their own connections.
- After the macro, the fixture's name is a `const fn` returning a
  descriptor: it can be imported (`use common::db;`) and named in paths, but
  not called.

## `Setup`: what a fixture can ask for

Everything below is created and owned by cargo-xtest's agent. It is removed
at teardown, after crashes (by the next run), and kept with
`--keep-fixtures`. Prefer these over starting things yourself: a container
you start with the docker CLI directly, or a process you spawn and leave
running, is **not** cleaned up.

| Call | Returns | Notes |
|---|---|---|
| `cx.docker(Docker::image("img:tag"))` | `Container` | runs next to the tests |
| `cx.docker(Docker::build("tests/docker/Dockerfile"))` | `Container` | path relative to the package; context = its dir, or `.context(dir)`; built once per host and content |
| `cx.docker_environment(Docker::...)` | `()` | tests run inside it; requires `environment` |
| `cx.k3d(K3d::new())` | `Cluster` | tests get `KUBECONFIG` and `kubectl` on `PATH` automatically |
| `cx.k3d_environment(K3d::new().runner("img"))` | `Cluster` | tests run in the runner container; requires `environment` |
| `cx.spawn(Command::new("prog").args([..]))` | `Process` | runs until teardown; output in `process.log()` |
| `cx.export("KEY", value).await?` | | env var for the tests that use the fixture |
| `cx.cleanup(["prog", "arg"]).await?` | | runs at teardown, or after a crash; runs on the host from `/` |
| `cx.serve(\|shutdown\| async move { ...; Ok(()) })` | | keeps Rust code running for the fixture's lifetime |
| `cx.instance_id()` | `&str` | unique per instance; letters, digits, `-` |

Builders:

- `Docker`: `.env(k, v)`, `.port(p)`, `.ready(r)`, `.context(dir)`.
- `Ready`: `Ready::tcp(port)` (the port is published automatically),
  `Ready::cmd([...])` (inside the container for Docker, on the host for
  k3d), `.and_cmd([...])`, `.timeout(Duration)`. The default timeout is 30s.
- `Container`: `.addr(port)` gives `SocketAddr` on 127.0.0.1, `.host_port(port)`,
  `.name()`. **Ports must be published** with `.port(p)` or `Ready::tcp(p)`;
  `addr` panics otherwise.
- `K3d`: `.servers(n)`, `.agents(n)`, `.image("rancher/k3s:...")`, `.arg("--no-lb")`,
  `.network("shared-name")`, `.ready(Ready::cmd([...]))`, `.timeout(d)`, `.runner(img)`.
- `Cluster`: `.kubeconfig()`, `.context()`, `.name()`, `.kubectl()`,
  `.server_ip(n)`, `.agent_ip(n)`, `.network()`.
- `Command`: `.arg`, `.args`, `.env`, `.current_dir`.
- `xtest::fixture::wait_for_tcp(addr, timeout).await?` waits for a port.

Custom logic goes around these calls: run migrations, seed data, call
`kubectl` with `cluster.kubectl()` and `cluster.kubeconfig()`, poll until
something is up. For a short command, `std::process::Command` is fine; for a
long-running one, use `cx.spawn`.

### Serving fixtures (mock servers in Rust)

```rust
#[xtest::fixture(scope = "binary")]
pub async fn api(cx: &mut Setup) -> xtest::Result<Api> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    cx.serve(move |shutdown| async move {
        tokio::select! {
            _ = shutdown => {}
            _ = serve_forever(listener) => {}
        }
        Ok(())
    });
    Ok(Api { addr })
}
```

`shutdown` completes when teardown starts; the server then has 10 seconds.

### Environments

```rust
#[xtest::fixture(environment, scope = "binary")]
async fn borg(cx: &mut Setup) -> xtest::Result<Borg> {
    cx.docker_environment(Docker::build("tests/docker/borg.Dockerfile")).await?;
    Ok(Borg)
}

#[xtest::test]
fn backs_up(#[fixture(borg)] _env: &Borg) { /* runs inside the container */ }
```

The workspace is mounted at its original paths inside the container, and the
container uses the host network. The test binary is built on the developer's
machine, so the image needs a glibc at least as new as that machine's.
cargo-xtest checks this and explains if it doesn't fit. A test runs in at most
one environment.

## Teardown

Usually none is needed: what the fixture asked for through `Setup` is
removed automatically, newest first, after fixtures that depend on it.

```rust
#[xtest::teardown(fixture_name)]
async fn name(cx: &mut Teardown, value: Value) -> xtest::Result<()>
```

- `&mut Teardown` is optional; the value can be `T` or `&T`; sync is fine.
  It may return `()` or a `Result`.
- It runs before the fixture's containers are removed (they are still
  reachable) and before a `serve` fixture is stopped.
- `cx.failed()` says whether a test that used it failed;
  `cx.attach(name, bytes)` shows a file with that test's output.
- It has 60 seconds. If it fails, that's a warning, and resources are
  removed anyway. It does **not** run after a crash: use `cx.cleanup` for
  external state that must never leak.

## Rules that are easy to get wrong

- **Where fixtures live:** only in code compiled as a test: files in
  `tests/`, `tests/common/mod.rs`, or `#[cfg(test)]` modules in `src/` (which
  can use private items). Fixtures in a library crate or a separate fixtures
  crate are **not supported** yet (issue #7). cargo-xtest reports "no test
  binary contains its setup" when it sees one.
- **Shared data isolation:** tests run in parallel, each in its own process.
  With a `run`/`binary`-scoped server, give each test its own namespace
  through a test-scoped fixture: a database, key prefix or Kubernetes
  namespace named after `cx.instance_id()`. Don't hardcode names that two
  tests or two retries could both create.
- **No async tests:** `#[xtest::test] async fn` is a compile error (issue
  #6). Fixtures and teardowns can be async; inside a test, use blocking
  clients or build a runtime yourself.
- **No double test attributes:** don't combine `#[test]`, `#[tokio::test]`
  or `#[rstest]` with `#[xtest::test]`. If `#[test]` is above it, the test is
  registered twice; cargo-xtest warns about it.
- **No glob import:** don't write `use xtest::*;`. Its `test` clashes with
  the built-in `#[test]`. Write `#[xtest::test]`, `#[xtest::fixture]`.
- **No mutable borrows:** a fixture parameter can't be `&mut T`. Take `T`
  if you need to mutate your copy.
- **Fixture metadata must agree:** the same fixture compiled into several
  binaries (via `mod common;`) must have the same scope, dependencies and
  arguments in each, e.g. no `cfg`/feature differences. cargo-xtest reports
  "compiled differently" otherwise.
- **`#[ignore]` still works:** cargo-xtest runs fixture tests even though
  the macro marks them ignored for plain `cargo test`. Your own `#[ignore]`
  is respected; use a profile override with `include-ignored = true` to run
  such tests in one profile only (e.g. tests that need k3d).
- **Memory:** set `memory` on fixtures that start something big (databases,
  clusters). Until a peak has been measured, a fixture counts as 0, and
  cargo-xtest may start more tests at once than fit in memory.

## Configuration (`.config/xtest.toml`, optional)

Rust fixtures need no configuration. The file is for profiles, hosts and
per-test settings:

```toml
[profile.default]
timeout = "60s"
retries = 1

# Run ignored tests of one binary in the `k8s` profile only.
[[profile.k8s.overrides]]
filter = "binary(k8s)"
include-ignored = true
timeout = "5m"
```

Put hosts in `.config/xtest.local.toml` (add it to `.gitignore`):

```toml
[[host]]
name = "box"
ssh = "user@box.lan"
```

Fixtures can also be defined in TOML (`[fixture.name]` with
`kind = "docker" | "k3d" | "command"`) and attached with `uses`/`run-in`
overrides; see the docs. Rust fixtures and TOML fixtures can be mixed.

## Running and debugging

```sh
cargo xtest                                  # everything, default profile
cargo xtest signs_up                         # tests whose name contains this
cargo xtest -E 'binary(signup) & test(/^db_/)'   # filter expressions
cargo xtest --list                           # tests, their fixtures, memory estimates
cargo xtest --no-capture -j1                 # live output, one test at a time
cargo xtest --profile k8s --host box         # another profile, one host
cargo xtest --keep-fixtures                  # keep fixtures of failed tests, print how to inspect them
cargo xtest --retries 2 --fail-fast
cargo xtest clean                            # remove kept runs, caches, built images on the hosts
```

Exit codes: 0 when all tests pass, 100 when a test fails, 1 on errors.

- **When a fixture's setup fails**, its tests fail with `setting up fixture
  `name (file)`: ...` followed by the setup's output, including its panic
  message.
- **`--list`** shows the fixtures each test will get, in setup order.
- **With `--keep-fixtures`**, the message names the containers
  (`docker exec -it <name> sh`, `docker logs <name>`), the kubeconfig, and the
  setup log.

## Checklist for adding fixture tests

1. Add the `xtest` and `serde` dev-dependencies (and `tokio` if fixture code uses it).
2. Write fixtures in `tests/common/mod.rs` (with `#![allow(dead_code)]`), or
   in the test file itself, with serializable value types.
3. Choose scopes deliberately: an expensive server gets `run` scope, and a
   per-test namespace on top of it gets `test` scope with a teardown.
4. Set `memory` on big fixtures.
5. Write tests as `#[xtest::test] fn name(fixture: &Value)`.
6. `cargo build --tests`: fixture names and types are checked at compile time.
7. `cargo xtest --list`: confirm each test gets the fixtures you expect.
8. `cargo xtest`: run, and then check that nothing is left behind
   (`docker ps -a --filter label=xtest.fixture` should be empty).
