# Internals

How cargo-xtest works inside, for contributors. For using it, start at the
[README](../README.md).

## Crates

| Crate | Role |
|---|---|
| `crates/cargo-xtest` | the controller: CLI, config, build, file sync, scheduling, reporting. Also ships the `xtest-agent` binary. |
| `crates/xtest-agent` | the agent on each host: blob store, sandbox, processes, fixtures, cleanup |
| `crates/xtest-proto` | the wire protocol between the two |
| `crates/xtest`, `crates/xtest-macros` | `#[xtest::test]`, used in the tests themselves |
| `examples/demo` | a workspace that exercises every feature |

The controller runs on your machine. It starts one agent per host (over SSH,
or as a local process) and talks to it over the agent's stdin and stdout.

## A run, step by step

1. **Build.** `cargo test --no-run --message-format=json-render-diagnostics`
   builds the tests; the JSON output lists the test binaries. `harness = false`
   targets are found by reading the manifests.
2. **Collect files.** The workspace is walked with `.gitignore` rules, plus the
   profile's `include` globs and the built executables. Files are hashed
   (SHA-256, in parallel), except those whose hash is in the cache; see
   [Hash cache](#hash-cache).
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
7. **Schedule.** All hosts take tests from one queue, ordered longest first
   (see [Test order](#test-order)). Each host starts tests while it has free
   slots and memory; see [memory scheduling](memory-scheduling.md).
8. **Run.** Each test is its own process: `<binary> --exact <name>
   --nocapture`, with fixtures set up first if it needs them.
9. **Shut down.** The controller sends `Shutdown`. The agent kills what is
   still running, tears down fixtures (except kept ones), and removes the run
   directory.

## Rust fixtures

`#[xtest::test]`, `#[xtest::fixture]` and `#[xtest::teardown]` each embed a
record in the binary: a `#[used]` static byte array built at compile time
(`xtest_proto::meta`), which the generated code refers to so the linker keeps
it. After the build, the controller scans every test binary for these
records: which tests use which fixtures, and each fixture's scope,
dependencies and memory. Nothing has to run for that.

A fixture's setup and teardown are hidden ignored tests
(`__xtest_fixture_<name>`, `__xtest_teardown_<name>`), left out of the test
list. To set a fixture up, the agent runs its setup test with `--exact` and a
Unix socket in `XTEST_SOCKET`. The fixture asks for containers, clusters and
processes over that socket (`xtest_proto::fx`); the agent creates them with
its usual code, as *parts* of the fixture's lease, and finally gets the
fixture's value as JSON. Tests get the values of their fixtures in a JSON
file named by `XTEST_FIXTURES`.

Processes a fixture runs carry `XTEST_RUN=<run id>`, and `cx.cleanup`
commands are saved under `<run>/cleanup/`, so garbage collection can kill
and clean up after a run whose agent crashed.

## Test order

The queue is ordered by how long each test took in the last run, longest
first. A slow test that starts last makes the whole run as long as that
test, so starting it first lets the short ones fill the other slots
meanwhile. Tests without a recorded duration count as the median of the
known durations. Ties, and runs without any history, go biggest memory
first, since those tests are the hardest to fit later.

Each host starts the first test in the queue that fits in its free memory,
so the order is a preference, not a guarantee. `--list` prints tests in this
order, with their expected durations.

Durations and memory peaks are kept in `<target dir>/xtest/history.json`.
A duration is the time the test process ran, not counting fixture setup. It
isn't saved for tests that timed out, tests whose fixtures failed, or
interrupted runs.

## Hash cache

`<target dir>/xtest/hashes.json` maps each file's path to its hash, together
with its *stamp*: size, mtime, ctime, inode and device. A cached hash is used
only while the stamp is unchanged. ctime can't be set by users, so a file whose
content changed but whose mtime was put back is still hashed again.

Two cases are never cached, and are hashed again on the next run:

- **Racy files:** the mtime or ctime is within 2 seconds of when the stamp was
  read. On filesystems with coarse timestamps, the file could change again
  within the same tick without its stamp changing (git's index has the same
  problem).
- **Files that changed while being hashed:** the stamp differs before and after.

The cache is rewritten after every run (atomically, through a temp file), so
removed files drop out. A missing, corrupt or older-format cache is ignored.
`--rehash` ignores the cache for one run. The `Syncing` line shows how many
files were hashed and how many came from the cache.

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
| `Setup`, `Teardown` | `Lease` with the fixture's env vars; `TornDown` with its peak memory |
| `Memory` | `MemAvailable`, total memory, the current use of each running test, and the current and peak use of each fixture |
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
| `tmp/<request>` | each test process's `TMPDIR`, and its fixture-values file; in an environment container only the fixture-values file, which the container sees through a mount |
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
- stdout and stderr are streamed to the controller as they are written, as
  separate streams. The controller keeps them in order and shows them for
  failed tests (stderr colored), for every test with `--success-output`, or
  line by line as they arrive with `--no-capture`.
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
