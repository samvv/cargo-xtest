# cargo-xtest

A test runner for Cargo that runs tests on other hosts and inside fixtures
such as Docker containers.

- **Remote hosts:** runs tests on other machines over SSH, at their original paths.
- **Fixtures:** Docker containers, k3d clusters and setup scripts, always torn down afterwards.
- **Memory-aware scheduling:** starts tests in parallel only while they fit in free memory.
- **Fast sync:** uploads only changed files to a content-addressed store.
- **Isolation:** one process per test, with its own temp directory and timeout.
- **Self-cleaning:** removes leftovers after crashes and keeps caches under a size cap.

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

Hosts only need SSH (any login shell; commands run through `sh -c`). The agent installs itself on first use under
`$XDG_CACHE_HOME/cargo-xtest` (default `~/.cache/cargo-xtest`).

## Configuration: `.config/xtest.toml`

```toml
[profile.default]
target = "x86_64-unknown-linux-musl"   # optional
include = ["tests/data/**"]            # extra files, even if gitignored
timeout = "60s"
memory = "256M"                        # assumed for tests not measured yet
memory-reserve = "512M"                # kept free on every host

[[profile.default.overrides]]
filter = "package(myapp) & test(=backup::tests::creates_archive)"
run-in = "borg"            # environment: the test runs inside it
uses = ["pg"]              # services: run next to the test
include-ignored = true
memory = "2G"              # wins over the measured peak

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
# store-size = "10G"                 # cap on the blob store (default 10G)
```

Without any `[[host]]`, tests run on this machine.

More details:

- [docs/internals.md](docs/internals.md): the filter syntax and how
  environment containers work.
- [docs/fixtures/k3d.md](docs/fixtures/k3d.md): Kubernetes clusters with k3d,
  including several clusters and networking between them.
- [docs/memory-scheduling.md](docs/memory-scheduling.md): how tests are
  started only while they fit in free memory, and how their memory is measured.
- [docs/cleanup.md](docs/cleanup.md): what agents clean up, the store size cap
  and `cargo xtest clean`.

## CLI

```
cargo xtest [FILTERS]... [-E EXPR]... [-p PKG] [--release] [--target T]
            [--profile P] [--host NAME]... [--ignored | --include-ignored]
            [--keep-fixtures] [--list] [--config FILE]
cargo xtest clean [--host NAME]... [--config FILE]
```

A filter that is literally `clean` must be written as `-E 'test(clean)'`.

Exit codes: 0 when all tests pass, 100 when any test fails, 1 on errors,
130 when interrupted.

`--keep-fixtures` keeps the test-scoped fixtures of failed tests and prints
how to inspect them (`docker exec -it ... sh`). `cargo xtest clean` removes
them; so does deleting the run's `keep` file under `<root>/runs/` (the next
run cleans it up).

## Layout

| Crate | Role |
|---|---|
| `crates/cargo-xtest` | CLI, config, build, scheduling, reporting |
| `crates/xtest-agent` | Blob store, sandbox, process spawning, fixtures, cleanup |
| `crates/xtest-proto` | Wire protocol (length-prefixed postcard frames) |
| `examples/demo` | Workspace that exercises every feature |
