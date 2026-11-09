# Disk use and cleanup

Agents remove what runs leave behind and keep their caches in check, so a
host doesn't fill up over time.

```toml
[[host]]
name = "build-box"
ssh = "ci@build-box.lan"
store-size = "10G"
```

## Keys

| Key | Where | Default | Meaning |
|---|---|---|---|
| `store-size` | `[[host]]` | `"10G"` | cap on the host's blob store |
| `root` | `[[host]]` | `~/.cache/cargo-xtest` | the agent's data directory; everything below lives here |

## What is stored

| Directory under `root` | Contents |
|---|---|
| `store/` | synced files, stored once by content hash |
| `runs/` | one directory per test run: its sandbox and temp files |
| `agent/` | agent builds, one per version |
| `tools/` | cached tools, such as the `kubectl` for [k3d fixtures](fixtures/k3d.md) |
| `images/` | markers recording when built Docker images were last used |

## When an agent starts

Each agent cleans up when it starts:

| What | Removed when |
|---|---|
| runs, with their containers, clusters and networks | the run is no longer running, unless it was kept with `--keep-fixtures` |
| store blobs, least recently used first | the store is bigger than `store-size`, the blob wasn't used in the last 24 hours, and no running test run uses it |
| agent builds, cached tools, images built for fixtures | unused for 7 days |

Images pulled from a registry are left alone.

## `cargo xtest clean`

```
cargo xtest clean [--host NAME]...
```

Removes everything that no running test run uses, right away: kept runs and
their fixtures, all store blobs, old agent builds, cached tools and built
images. Afterwards, the next run uploads everything again.
