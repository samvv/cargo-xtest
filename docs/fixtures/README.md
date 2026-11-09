# Fixtures

A fixture is something a test needs around it: a database container, a
Kubernetes cluster, a temporary directory made by a script. The agent on the
test's host sets fixtures up when the first test needs them and always tears
them down afterwards, even if the controller dies.

```toml
[fixture.pg]
kind = "docker"
image = "postgres:17"
scope = "run"
export = { DATABASE_URL = "postgres://postgres@{host}:{port:5432}/postgres" }

[[profile.default.overrides]]
filter = "binary(db)"
uses = ["pg"]
```

## Kinds

| Kind | What it is | Page |
|---|---|---|
| `docker` | a Docker container, from an image or a Dockerfile | [docker.md](docker.md) |
| `command` | your own setup and teardown commands | [command.md](command.md) |
| `k3d` | a Kubernetes cluster made with k3d | [k3d.md](k3d.md) |

## Services and environments

An [override](../configuration.md#overrides-profilenameoverrides) attaches
fixtures to the tests it matches, in one of two roles:

| Key | Role | The test |
|---|---|---|
| `uses = ["pg"]` | service | runs on the host, next to the fixture, and gets its `export`ed env vars |
| `run-in = "borg"` | environment | runs *inside* the fixture, e.g. inside the container |

A test has at most one environment, and any number of services. Docker and
k3d fixtures can be either; command fixtures can only be services.

## Scopes

`scope` decides how widely one instance of a fixture is shared:

| Scope | One instance per | Torn down |
|---|---|---|
| `test` (default) | test | right after the test |
| `binary` | test binary, per host | at the end of the run |
| `run` | host | at the end of the run |

Instances are never shared between hosts, or between the two roles: a
fixture used both with `uses` and with `run-in` gets one instance for each.

A fixture that fails to set up fails the tests that need it, with the setup
error. A shared instance is set up only once, so its tests all fail with the
same error.

## Common keys

| Key | Kinds | Meaning |
|---|---|---|
| `ready` | docker, k3d | a check that must pass before tests start; see each kind |
| `export` | docker, k3d | env vars for the tests; values may use the kind's templates |
| `memory` | all | memory one instance needs, for [memory scheduling](../memory-scheduling.md#fixtures) |

## Debugging: `--keep-fixtures`

With `--keep-fixtures`, the test-scoped fixtures of failed tests are kept,
and cargo-xtest prints how to inspect them, e.g. `docker exec -it <name> sh`.
Remove them with `cargo xtest clean`, or delete the run's `keep` file under
`<root>/runs/` and the next run cleans them up.
