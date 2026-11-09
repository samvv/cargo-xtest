# Docker fixtures

A `kind = "docker"` fixture runs a container, either from an image or built
from a Dockerfile in your workspace. The host needs Docker.

```toml
# A service: tests run next to it.
[fixture.pg]
kind = "docker"
image = "postgres:17"
scope = "run"
env = { POSTGRES_PASSWORD = "x" }
ready = { cmd = ["pg_isready", "-U", "postgres"], timeout = "30s" }
export = { DATABASE_URL = "postgres://postgres:x@{host}:{port:5432}/postgres" }

# An environment: tests run inside it.
[fixture.borg]
kind = "docker"
build = { dockerfile = "tests/docker/borg.Dockerfile", context = "tests/docker" }
```

## Keys

| Key | Default | Meaning |
|---|---|---|
| `image` | | image to run, e.g. `"postgres:17"` |
| `build` | | `{ dockerfile = "...", context = "..." }`: build the image from your workspace. `context` defaults to the Dockerfile's directory. |
| `env` | `{}` | env vars for the container |
| `ports` | `[]` | container ports to publish (services only) |
| `ready` | none | `{ tcp = PORT, cmd = "...", timeout = "..." }`; see [Ready check](#ready-check). The timeout defaults to 30 s. |
| `export` | `{}` | env vars for the tests; values may use [templates](#templates) (services only) |

Set exactly one of `image` and `build`.

## Built images

A `build` image is tagged `xtest/<fixture>:<hash>`, where the hash covers the
Dockerfile and every file in the context. It is built on each host the first
time it's needed, and again only when one of those files changes. Built
images unused for 7 days are removed; see [cleanup](../cleanup.md).

## Services

With `uses`, the container runs in the background, and its `ports` (plus the
`ready.tcp` port) are published on `127.0.0.1` at random free ports. Tests
find them through `export`.

### Templates

| Template | Value |
|---|---|
| `{host}` | the address to connect to: `127.0.0.1` |
| `{port:N}` | the host port that container port `N` is published on |

```toml
ports = [6379]
export = { REDIS_ADDR = "{host}:{port:6379}" }
```

## Environment containers

With `run-in`, the test process runs inside the container (through
`docker exec`). The container:

- runs as your uid and gid, with `USER` and `LOGNAME` set;
- uses the host network (`--network host`), so `ports` and `export` aren't
  needed;
- has the run's sandbox mounted at its original absolute paths, so the test
  sees the same paths as on your machine;
- starts with `sleep infinity` as entrypoint, so it stays up until teardown.

The test binary is built on your machine and runs unchanged in the image. The
image therefore needs a glibc at least as new as yours, unless you build for a
musl target (`target = "x86_64-unknown-linux-musl"`).

## Ready check

Before tests start, the agent waits until every part of `ready` passes:

| Part | Passes when |
|---|---|
| `tcp = PORT` | the published container port accepts a TCP connection (services only) |
| `cmd = "..."` | the command exits 0 inside the container (`docker exec`) |

If the container exits before it is ready, setup fails with the last lines of
its logs.
