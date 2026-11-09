# Internals

How cargo-xtest works inside, for contributors. For using it, start at the
[README](../README.md).

## Crates

| Crate | Role |
|---|---|
| `crates/cargo-xtest` | the controller: CLI, config, build, file sync, scheduling, reporting. Also ships the `xtest-agent` binary. |
| `crates/xtest-agent` | the agent on each host: blob store, sandbox, processes, fixtures, cleanup |
| `crates/xtest-proto` | the wire protocol between the two |
| `examples/demo` | a workspace that exercises every feature |

The controller runs on your machine. It starts one agent per host (over SSH,
or as a local process) and talks to it over the agent's stdin and stdout.

## A run, step by step

1. **Build.** `cargo test --no-run --message-format=json-render-diagnostics`
   builds the tests; the JSON output lists the test binaries. `harness = false`
   targets are found by reading the manifests.
2. **Collect files.** The workspace is walked with `.gitignore` rules, plus the
   profile's `include` globs and the built executables. Every file is hashed
   (SHA-256, in parallel).
3. **Connect.** For an SSH host, the controller checks whether this exact
   agent build is installed (`<root>/agent/<hash>/xtest-agent`), uploads it if
   not, and starts `xtest-agent serve --stdio`. Remote commands are wrapped in
   `sh -c`, so the user's login shell doesn't matter.
4. **Sync.** `HaveBlobs` asks which hashes the host's store is missing; only
   those are uploaded, in 1 MiB `PutBlob` chunks sent without waiting for each
   reply.
5. **Materialize.** The agent builds the run's sandbox from the store; see
   [Store and sandbox](#store-and-sandbox).
6. **Discover.** Each test binary is run with `--list --format terse` (and
   `--ignored`) on the first host. A `harness = false` binary is one test.
7. **Schedule.** Each host has a queue and runs tests while it has free
   slots and memory; see [memory scheduling](memory-scheduling.md).
8. **Run.** Each test is its own process: `<binary> --exact <name>
   --nocapture`, with fixtures set up first if it needs them.
9. **Shut down.** The controller sends `Shutdown`. The agent kills what is
   still running, tears down fixtures (except kept ones), and removes the run
   directory.

## Protocol

Every message is a frame: a little-endian `u32` length, then a
[postcard](https://docs.rs/postcard)-encoded body. The controller sends
`ClientMsg { req, body }`; the agent answers with `AgentMsg` values that refer
back to `req`. `VERSION` is checked in `Hello` and bumped on every
incompatible change.

| Request | Answer |
|---|---|
| `Hello` | `HostInfo`: OS, architecture, CPUs, whether Docker and path mirroring work |
| `HaveBlobs`, `PutBlob` | `Missing` hashes; `Ok` per chunk |
| `Materialize` | `Ok` once the sandbox is built |
| `Spawn` | a stream of `Output` events, then one `Exited` with status, duration and peak memory |
| `Kill` | `Ok` |
| `Setup`, `Teardown` | `Lease` with the fixture's env vars; `Ok` |
| `Memory` | `MemAvailable`, total memory, and the current use of each running test |
| `Shutdown` | the agent cleans up and exits |

## Store and sandbox

**Store.** Files are stored once, by hash, at
`<root>/store/sha256/<2 hex>/<rest>`. An upload is written to `incoming/`,
verified, made read-only (`0555`) and renamed into place, so a blob in the
store is always complete.

**Run directory.** Each agent session gets `<root>/runs/<time>-<pid>/`:

| Path | Contents |
|---|---|
| `lock` | held (`flock`) for as long as the run is alive |
| `root/<absolute path>` | the sandbox: every synced file at its original path |
| `tmp/<request>` | each test process's `TMPDIR` |
| `keep` | present if fixtures were kept with `--keep-fixtures` |

Executables are hardlinked from the store, which is fast and saves space.
Other files are copied and made writable, because tests may change them.

**Path mirroring.** Tests must see their files at the original absolute paths,
or `env!("CARGO_MANIFEST_DIR")` and `CARGO_BIN_EXE_*` break. The agent runs
each test through `xtest-agent sandbox-exec`, which:

1. enters a new user and mount namespace (no root needed), mapping the
   user's uid and gid;
2. bind-mounts each mirror (the workspace root, and the target directory if
   it lies outside the workspace) from `root/` over its original path;
3. if an original path can't be created, mounts a tmpfs over its deepest
   existing ancestor, and bind-mounts the ancestor's original entries back
   into it;
4. executes the test.

If user namespaces aren't available, tests run under the sandbox path, with
a warning.

## Processes

- Each test runs in its own process group, so killing it (on timeout,
  `Kill` or shutdown) sends `SIGKILL` to everything it started.
- stdout and stderr are streamed to the controller, which shows them only for
  failed tests.
- Peak memory comes from `wait4`, a container's cgroup, or sampling; see
  [memory scheduling](memory-scheduling.md#how-peaks-are-measured).

## Fixtures

The controller decides when a fixture is needed, using its
[scope](fixtures/README.md#scopes). The agent sets it up as a *lease* with an
id and tears it down by that id. A `Spawn` lists the leases its test uses:
their env vars are added, and an environment lease decides where the process
runs (`docker exec` into a container instead of `sandbox-exec`).

Docker objects (containers, k3d clusters, networks, built images) carry the
labels `xtest.run` and `xtest.root`, so a later agent can tell which run they
belong to.

## Crash handling

- The agent ignores `SIGINT` and `SIGHUP`, and cleans up when its stdin
  closes. So it still cleans up if the controller is killed or the SSH
  connection drops.
- On Ctrl-C, the controller shuts all agents down properly and exits with
  status 130.
- If an agent itself dies, its run's lock is released. The next agent on
  that host sees an unlocked run directory and removes it, along with its
  labeled containers, clusters and networks. See [cleanup](cleanup.md).
