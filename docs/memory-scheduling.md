# Memory scheduling

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

## Configuration

```toml
[profile.default]
memory = "256M"            # assumed for tests not measured yet
memory-reserve = "512M"    # kept free on every host

[[profile.default.overrides]]
filter = "test(=big_import)"
memory = "2G"              # wins over the measured peak
```
