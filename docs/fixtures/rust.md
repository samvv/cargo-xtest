# Fixtures in Rust

Fixtures can be written in Rust, next to the tests that use them, with the
`xtest` crate. A fixture is an async function marked `#[xtest::fixture]`; a
test asks for it by taking a parameter with the fixture's name.

```toml
[dev-dependencies]
xtest = { git = "https://github.com/samvv/cargo-xtest" }
serde = { version = "1", features = ["derive"] }
```

```rust
// tests/common/mod.rs
use xtest::fixture::{Docker, Ready, Setup};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Redis {
    pub addr: std::net::SocketAddr,
}

#[xtest::fixture(scope = "run")]
pub async fn redis(cx: &mut Setup) -> xtest::Result<Redis> {
    let c = cx.docker(Docker::image("redis:7-alpine").port(6379).ready(Ready::tcp(6379))).await?;
    Ok(Redis { addr: c.addr(6379) })
}
```

```rust
// tests/cache.rs
mod common;
use common::*;

#[xtest::test]
fn pings(redis: &Redis) {
    // redis.addr is the container's published port
}
```

Nothing goes in `.config/xtest.toml`. The fixture's value is serialized after
setup and handed to each test, so value types derive `Serialize` and
`Deserialize`. Under plain `cargo test`, tests with fixtures are ignored.

The demo has fixtures of every kind in
[examples/demo/tests/common/mod.rs](../../examples/demo/tests/common/mod.rs)
and [examples/demo/tests/k8s_rust.rs](../../examples/demo/tests/k8s_rust.rs).

## `#[xtest::fixture]`

```rust
#[xtest::fixture(scope = "run", memory = "512M", timeout = "2m")]
async fn name(cx: &mut Setup, dep: &Dep) -> xtest::Result<Value>
```

| Argument | Meaning |
|---|---|
| `scope` | `"test"` (default), `"binary"` or `"run"`, as for [TOML fixtures](README.md#scopes) |
| `memory` | memory one instance needs, for [memory scheduling](../memory-scheduling.md#fixtures). Without it, the peak its containers had last time, or nothing on a first run |
| `timeout` | how long setup may take (default 5 minutes) |
| `environment` | the fixture's tests run inside a container it makes; see [Environments](#environments) |

The first parameter may be `&mut Setup`, for asking cargo-xtest for
containers and processes. The other parameters are fixtures this one depends
on: they are set up first, and must live at least as long (a run-scoped
fixture can't use a test-scoped one). Synchronous functions work too.

The function's name refers to the fixture, so it can be imported
(`use common::redis;`) and named in other modules (`#[fixture(common::redis)]`).
It can't be called directly.

## Using fixtures in tests

A parameter of an `#[xtest::test]` is a fixture: by default the one with the
parameter's name, or the one given with `#[fixture(path)]`:

```rust
#[xtest::test]
fn copies(redis: &Redis, #[fixture(common::bucket)] b: Bucket) { ... }
```

Take the value as `&T` or `T`. A wrong name or type is a compile error. A test
gets the fixtures it names and everything they depend on.

## Building blocks

`Setup` asks the agent for things; the agent creates and owns them. They are
removed when the fixture is torn down, also after a crash, and kept with
`--keep-fixtures`.

| Call | What it does | Like |
|---|---|---|
| `cx.docker(Docker::image(..))` | starts a container next to the tests; `Container::addr(port)` is where a published port is reachable | [`kind = "docker"`](docker.md) |
| `Docker::build("tests/docker/Dockerfile")` | the same from a Dockerfile (relative to the package); built once per host and content | `build = { ... }` |
| `cx.docker_environment(..)` | starts a container the tests run in | `run-in` |
| `cx.k3d(K3d::new())` | creates a k3d cluster; tests get `KUBECONFIG` and `kubectl`. `Cluster` has the context, kubeconfig and node IPs | [`kind = "k3d"`](k3d.md) |
| `cx.k3d_environment(K3d::new().runner(image))` | a cluster whose runner container the tests run in | `run-in` |
| `cx.spawn(Command::new(..))` | starts a process that runs until teardown; its output goes to `Process::log()` | [`kind = "command"`](command.md) |
| `cx.export(key, value)` | sets an env var for the tests | `export` |
| `cx.cleanup(argv)` | runs a command at teardown, or after a crash during the next run's cleanup | `teardown` |
| `cx.serve(\|shutdown\| ..)` | keeps Rust code running for the fixture's lifetime, e.g. a mock server | |
| `cx.instance_id()` | unique per fixture instance, e.g. to name a database | |

`Docker` takes `.env(k, v)`, `.port(p)` and `.ready(Ready::tcp(p))` or
`.ready(Ready::cmd([...]))`; `K3d` takes `.servers(n)`, `.agents(n)`,
`.image(..)`, `.arg(..)`, `.network(..)` and `.ready(..)`. Anything else is
your own code, before or after these calls: run migrations, seed data, wait
for something.

A fixture's code runs on the test host, in the same sandbox as the tests, so
it sees the workspace at its original paths. `xtest::fixture::wait_for_tcp`
waits for a port.

### Extending a built-in fixture

To build on a fixture, depend on it:

```rust
#[xtest::fixture(scope = "run")]
pub async fn pg(cx: &mut Setup) -> xtest::Result<PgServer> { ... }

/// A database per test, migrated.
#[xtest::fixture]
pub async fn db(cx: &mut Setup, pg: &PgServer) -> xtest::Result<Db> {
    let name = format!("test_{}", cx.instance_id().replace('-', "_"));
    create_database(&pg.url, &name).await?;
    let url = pg.url_for(&name);
    myapp::migrate(&url).await?;
    Ok(Db { url, name })
}
```

Fixtures are ordinary functions, so reusable ones can also be plain helper
functions that take `&mut Setup`.

## Teardown

Most fixtures need no teardown of their own: what they asked for through
`Setup` is removed automatically, newest first. For logic that needs the
fixture's value, add a teardown:

```rust
#[xtest::teardown(db)]
async fn drop_db(cx: &mut Teardown, db: Db) -> xtest::Result<()> {
    if cx.failed() {
        cx.attach("tables.txt", dump_tables(&db.url).await?)?;
    }
    drop_database(&db.name).await
}
```

It takes an optional `&mut Teardown` and the fixture's value, and returns
`()` or a `Result`.

- It runs before cargo-xtest removes the fixture's containers, so they are
  still reachable, and before a `serve` fixture is stopped.
- `cx.failed()` says whether a test that used the fixture failed.
  `cx.attach(name, data)` shows a file with the test's output (test scope)
  or at the end of the run.
- Fixtures are torn down in reverse order: `db` before the `pg` it uses.
- A failing teardown is a warning; the fixture's resources are removed
  anyway. It has 60 seconds.
- If the run crashes, teardowns don't run; `cx.cleanup` commands and
  everything the agent created are still cleaned up by the next run. Use
  `cx.cleanup` for external state that must not leak.
- With `--keep-fixtures`, the test-scoped fixtures of failed tests are kept:
  neither their teardown nor the removal of their resources runs.

A `serve` fixture gets a `shutdown` future that completes when teardown
starts. It then has 10 seconds to finish before it is killed.

## Environments

A fixture declared with `environment` makes the container its tests run in,
with `cx.docker_environment` or `cx.k3d_environment`:

```rust
#[xtest::fixture(environment, scope = "binary")]
async fn borg(cx: &mut Setup) -> xtest::Result<Borg> {
    cx.docker_environment(Docker::build("tests/docker/borg.Dockerfile")).await?;
    Ok(Borg)
}

#[xtest::test]
fn backs_up(#[fixture(borg)] _env: &Borg) { /* runs inside the container */ }
```

A test runs in at most one environment. Other fixtures can't depend on an
environment.

## Where fixtures can live

A fixture's setup runs as a hidden ignored test (`__xtest_fixture_<name>`),
so it must be in code compiled as a test:

- a test file in `tests/`;
- `tests/common/mod.rs`, included by several test files: one fixture,
  however many binaries include it. A run-scoped one is set up once per host;
- `#[cfg(test)]` code in `src/`, for unit tests, with private access.

Fixtures in library crates (a fixtures crate, or the app's own lib) are not
supported yet: [#7](https://github.com/samvv/cargo-xtest/issues/7).
cargo-xtest says so when a test uses one.

A fixture is identified by its source file and name. Copies compiled into
several binaries must agree on scope, dependencies and the other arguments.

## Together with TOML fixtures

TOML fixtures keep working, and a test can use both. Overrides still apply,
e.g. `include-ignored = true` to run ignored tests in a profile. A Rust
environment and a `run-in` override on the same test is an error.
