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

[fixture.k8s]
kind = "k3d"                         # k3d cluster create / k3d cluster delete
scope = "run"
servers = 1                          # default 1
agents = 1                           # default 0
# image = "rancher/k3s:v1.31.4-k3s1"
args = ["--no-lb"]                   # extra `k3d cluster create` arguments
timeout = "5m"                       # for cluster creation (default 300s)
ready = { cmd = "kubectl wait --for=condition=Ready nodes --all", timeout = "3m" }
export = { K3D_CLUSTER = "{cluster}" }
runner = { image = "archlinux:latest" }  # needed for `run-in`; or `build = {...}`
# network = "mesh"                   # share a docker network with other clusters

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

See [docs/internals.md](docs/internals.md) for the filter syntax, how
environment containers work, and k3d fixtures: templates, several
clusters, and networking between clusters.

## Memory scheduling

Besides its slots, each host only starts a test while there is room for it.
Before starting one, the controller asks the agent for `MemAvailable` and
for what each running test uses right now. It then computes

```
free = MemAvailable − Σ(estimate − current use of each running test) − memory-reserve
```

and starts the biggest queued test that fits. A host with nothing running
always starts the next test, so a test bigger than any host still runs (with
a warning).

A test's estimate is, in order: `memory` from a matching override, the peak
measured in the last run (`<target>/xtest/memory.json`), or the profile's
`memory`. Peaks are measured as:

- plain processes: max RSS from `wait4` (the largest single process of the test);
- in a container of its own (`scope = "test"`): the container cgroup's `memory.peak`;
- in a shared container: the sampled RSS of processes tagged with the test's
  `XTEST_REQ` env var.

The report shows each test's measured peak; `--list` shows the estimate and
where it came from.

## Disk use and cleanup

Each agent cleans up when it starts:

- runs that are no longer running, with their containers, clusters and
  networks (runs kept with `--keep-fixtures` stay);
- least recently used blobs, while the store is bigger than `store-size`.
  Blobs used in the last 24 hours, or still linked into a running test run,
  are never evicted;
- agent builds, cached `kubectl` binaries and images built for fixtures that
  have not been used for 7 days.

`cargo xtest clean [--host NAME]...` removes everything that no running test run
uses, right away: kept runs and their fixtures, all store blobs, old agent
builds, cached tools and built images. Images pulled from a registry are left
alone.

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
