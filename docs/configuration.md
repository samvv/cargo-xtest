# Configuration

cargo-xtest reads `.config/xtest.toml` in the workspace root, or the file
given with `--config`, and then your [local overrides](#local-overrides-xtestlocaltoml)
if there are any. Without a config file it uses the defaults below: one host
(this machine) and no fixtures.

```toml
[profile.default]
timeout = "60s"

[[profile.default.overrides]]
filter = "binary(db)"
uses = ["pg"]

[fixture.pg]
kind = "docker"
image = "postgres:17"
scope = "run"
env = { POSTGRES_PASSWORD = "x" }
ready = { cmd = ["pg_isready", "-U", "postgres"], timeout = "30s" }
export = { DATABASE_URL = "postgres://postgres:x@{host}:{port:5432}/postgres" }

[[host]]
name = "build-box"
ssh = "ci@build-box.lan"
```

## Local overrides: `xtest.local.toml`

Settings that belong to you rather than the project, such as your own test
hosts, go in `.config/xtest.local.toml` next to the shared file. Add it to
`.gitignore`:

```gitignore
.config/xtest.local.toml
```

```toml
# .config/xtest.local.toml
[[host]]
name = "workstation"
ssh = "me@workstation.lan"
slots = 16
```

It uses the same format, and is merged over the shared file:

| Section | Effect of the local file |
|---|---|
| `[[host]]` | if it has any hosts, they replace the shared ones |
| `[profile.<name>]` | keys it sets win; its overrides apply after the shared ones |
| `[fixture.<name>]` | a fixture it defines replaces the shared one with that name |

With `--config <dir>/<name>.toml`, the local file is `<dir>/<name>.local.toml`.
Without `--config`, either file may exist without the other.

## Profiles: `[profile.<name>]`

Choose a profile with `--profile <name>`; the default is `default`.

| Key | Default | Meaning |
|---|---|---|
| `target` | none (your machine's) | target triple to build for, e.g. `"x86_64-unknown-linux-musl"`. `--target` wins over it. |
| `include` | `[]` | extra files to sync, as globs relative to the workspace root (e.g. `"tests/data/**"`), even if they are gitignored |
| `timeout` | none | how long one test may run before it is killed |
| `memory` | `"256M"` | memory assumed for a test that hasn't been measured yet; see [memory scheduling](memory-scheduling.md) |
| `memory-reserve` | `"512M"` | memory kept free on every host |
| `overrides` | `[]` | per-test settings; see below |

Other profiles inherit from `default`: a key they don't set takes the
`default` profile's value, and the `default` profile's overrides apply before
their own.

## Overrides: `[[profile.<name>.overrides]]`

Each override applies to the tests its `filter` matches.

| Key | Meaning |
|---|---|
| `filter` | which tests it applies to; required. See [filters](filters.md). |
| `run-in` | a fixture the test runs inside (its *environment*) |
| `uses` | fixtures that run next to the test (its *services*) |
| `include-ignored` | also run these tests when they are `#[ignore]`d |
| `timeout` | timeout for these tests |
| `memory` | memory these tests need; wins over the measured peak |

When several overrides match a test, they apply in order: later values win,
and `uses` lists are combined.

## Fixtures: `[fixture.<name>]`

| Key | Default | Meaning |
|---|---|---|
| `kind` | required | `docker`, `command` or `k3d` |
| `scope` | `"test"` | `test`, `binary` or `run`: how widely one instance is shared; see [scopes](fixtures/README.md#scopes) |

The other keys depend on the kind:
[Docker](fixtures/docker.md#keys), [command](fixtures/command.md#keys) and
[k3d](fixtures/k3d.md#keys). A key that doesn't belong to the fixture's kind
is an error.

## Hosts: `[[host]]`

Without any `[[host]]`, tests run on this machine.

| Key | Default | Meaning |
|---|---|---|
| `name` | required | name used in output and with `--host` |
| `ssh` | none (this machine) | SSH destination, e.g. `"ci@build-box.lan"` |
| `slots` | the host's CPU count | how many tests run there at once (memory permitting) |
| `root` | `$XDG_CACHE_HOME/cargo-xtest` (`~/.cache/cargo-xtest`) | the agent's data directory on the host |
| `store-size` | `"10G"` | cap on the host's blob store; see [cleanup](cleanup.md) |
| `ssh-program` | `["ssh", "-o", "BatchMode=yes"]` | command used to connect |
| `agent` | `xtest-agent` next to `cargo-xtest` | local agent binary to install on the host |

A host must have the architecture the tests are built for. To test on a
different architecture, set `target` and point `agent` at an agent built for
it.

## Values

- **Durations:** `"500ms"`, `"30s"`, `"2m"`, `"1h"`, or a number of seconds.
- **Sizes:** `"512M"`, `"2G"`, `"1.5T"`, or a number of bytes. Units are
  binary: `M`, `MB` and `MiB` all mean 1024² bytes.
- **Commands** (`ready.cmd`): a string, run with `sh -c`, or a list of
  arguments.
