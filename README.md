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

cargo-xtest builds your tests locally, uploads what changed to each host, and
runs every test in its own process there. Tests see their files at the
original paths, so `env!("CARGO_MANIFEST_DIR")` and `CARGO_BIN_EXE_*` keep
working. Hosts only need SSH; the agent installs itself on first use. See
[docs/internals.md](docs/internals.md) for the details.

## Configuration

cargo-xtest reads `.config/xtest.toml` in the workspace root. Without it,
tests run on this machine with no fixtures.

```toml
[profile.default]
timeout = "60s"

# Tests in tests/db.rs get a Postgres container next to them.
[[profile.default.overrides]]
filter = "binary(db)"
uses = ["pg"]

# This test runs inside a container with borg installed.
[[profile.default.overrides]]
filter = "test(=backup::tests::creates_archive)"
run-in = "borg"

[fixture.pg]
kind = "docker"
image = "postgres:17"
scope = "run"
env = { POSTGRES_PASSWORD = "x" }
ready = { cmd = ["pg_isready", "-U", "postgres"], timeout = "30s" }
export = { DATABASE_URL = "postgres://postgres:x@{host}:{port:5432}/postgres" }

[fixture.borg]
kind = "docker"
build = { dockerfile = "tests/docker/borg.Dockerfile" }

[[host]]
name = "build-box"
ssh = "ci@build-box.lan"
```

Put your own settings, such as your test hosts, in
`.config/xtest.local.toml` and add it to `.gitignore`. It is merged over the
shared file; see [local overrides](docs/configuration.md#local-overrides-xtestlocaltoml).

The [documentation](docs/README.md) covers the rest:
[every configuration key](docs/configuration.md), [filters](docs/filters.md),
[fixtures](docs/fixtures/README.md) ([Docker](docs/fixtures/docker.md),
[command](docs/fixtures/command.md), [k3d](docs/fixtures/k3d.md)),
[memory scheduling](docs/memory-scheduling.md) and [cleanup](docs/cleanup.md).

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
| `--keep-fixtures` | Keep the test-scoped fixtures of failed tests, and print how to inspect them (e.g. `docker exec -it ... sh`); see [fixtures](docs/fixtures/README.md#debugging---keep-fixtures) |
| `--list` | List the tests that would run, with their memory estimates, then exit |
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
