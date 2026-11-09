# Disk use and cleanup

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

## Configuration

```toml
[[host]]
name = "pi4"
ssh = "ci@pi4.lan"
store-size = "10G"         # cap on the blob store (default 10G)
```
