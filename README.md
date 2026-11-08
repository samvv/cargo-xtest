# cargo-xtest

A test runner for Cargo that runs tests on other hosts and inside fixtures
such as Docker containers.

```
cargo install --path crates/cargo-xtest   # installs cargo-xtest and xtest-agent
cargo xtest                               # in any workspace
```

## How it works

1. Builds tests locally with `cargo test --no-run`.
2. Uploads test binaries, executables and workspace files (respecting
   `.gitignore`) to each host's content-addressed store. Blobs the host
   already has are skipped.
3. Each host gets a sandbox for the run. Tests see it at the **original
   absolute paths** through a user + mount namespace (no root needed), so
   `env!("CARGO_MANIFEST_DIR")` and `CARGO_BIN_EXE_*` keep working.
4. Lists the tests and runs each one in its own process, spread over all
   host slots.
5. Fixtures are set up and torn down by the agent. If the controller dies,
   the agent cleans up anyway; leftovers from a crashed agent are removed
   on its next start.

Hosts only need SSH. The agent installs itself on first use under
`$XDG_CACHE_HOME/cargo-xtest` (default `~/.cache/cargo-xtest`).

## Configuration: `.config/xtest.toml`

```toml
[profile.default]
target = "x86_64-unknown-linux-musl"   # optional
include = ["tests/data/**"]            # extra files, even if gitignored
timeout = "60s"

[[profile.default.overrides]]
filter = "package(myapp) & test(=backup::tests::creates_archive)"
run-in = "borg"            # environment: the test runs inside it
uses = ["pg"]              # services: run next to the test
include-ignored = true

[fixture.borg]
kind = "docker"
build = { dockerfile = "tests/docker/borg.Dockerfile", context = "tests/docker" }
scope = "test"             # test | binary | run

[fixture.pg]
kind = "docker"
image = "postgres:17"
scope = "run"
env = { POSTGRES_PASSWORD = "x" }
ready = { cmd = ["pg_isready", "-U", "postgres"], timeout = "30s" }
export = { DATABASE_URL = "postgres://postgres:x@{host}:{port:5432}/postgres" }

[fixture.tmpstate]
kind = "command"
setup = "./scripts/up.sh"            # prints {"handle": "...", "env": {...}}
teardown = "./scripts/down.sh {handle}"

[[host]]
name = "pi4"
ssh = "ci@pi4.lan"
slots = 4
# root = "/data/xtest"               # agent data directory on the host
```

Without any `[[host]]`, tests run on this machine.

**Filters** (`-E` and `filter =`): `package()`, `binary()`, `kind()`,
`test()`, `all()`, `none()`, combined with `&`, `|`, `!` and parentheses.
Matchers: `=exact`, `~substring`, `/regex/`. Plain `test(x)` matches a
substring; plain values for the other predicates match exactly.

**Environment containers** run as your uid, with `--network host`, the
sandbox mounted at its original paths, and `sleep infinity` as entrypoint.
The test binary is built on your machine, so the image needs a glibc at
least as new as yours, or build for a musl target.

## CLI

```
cargo xtest [FILTERS]... [-E EXPR]... [-p PKG] [--release] [--target T]
            [--profile P] [--host NAME]... [--ignored | --include-ignored]
            [--keep-fixtures] [--list] [--config FILE]
```

Exit codes: 0 when all tests pass, 100 when any test fails, 1 on errors,
130 when interrupted.

`--keep-fixtures` keeps the test-scoped fixtures of failed tests and prints
how to inspect them (`docker exec -it ... sh`). Delete the run's `keep` file
under `<root>/runs/` and the next run cleans it up.

## Layout

| Crate | Role |
|---|---|
| `crates/cargo-xtest` | CLI, config, build, scheduling, reporting |
| `crates/xtest-agent` | Blob store, sandbox, process spawning, fixtures, cleanup |
| `crates/xtest-proto` | Wire protocol (length-prefixed postcard frames) |
| `examples/demo` | Workspace that exercises every feature |
