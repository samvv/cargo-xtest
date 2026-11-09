# Memory scheduling

Besides its slots, each host only starts a test while there is enough free
memory for it and for the fixtures it would create. cargo-xtest learns how
much memory tests and fixtures need by measuring them.

```toml
[profile.default]
memory = "256M"            # assumed for tests not measured yet
memory-reserve = "512M"    # kept free on every host

[[profile.default.overrides]]
filter = "test(=big_import)"
memory = "2G"              # wins over the measured peak

[fixture.k8s]
kind = "k3d"
memory = "1500M"           # the fixture's own estimate
```

## Keys

| Key | Where | Default | Meaning |
|---|---|---|---|
| `memory` | profile | `"256M"` | estimate for tests that haven't been measured yet |
| `memory-reserve` | profile | `"512M"` | memory kept free on every host |
| `memory` | override | none | estimate for the matching tests; wins over the measured peak |
| `memory` | fixture | see [Fixtures](#fixtures) | estimate for one instance of the fixture; wins over the measured peak |

## When a test starts

Before starting a test, the controller asks the host's agent how much memory
is available (`MemAvailable`) and how much each running test uses right now.
It then computes:

```
free = MemAvailable
     − Σ(estimate − current use) over running tests and live fixtures
     − memory-reserve
```

Running tests and fixtures that haven't reached their estimate yet still have
that memory reserved. A test costs its own estimate plus the estimate of each
fixture instance it would create, so a test whose run-scoped cluster already
exists costs less than the first test that needs it. The controller starts
the biggest queued test whose cost fits in `free`, and checks again every
250 ms while tests are waiting.

A host with nothing running always starts its next test, so a test bigger
than any host still runs, with a warning.

## Estimates

A test's estimate is the first of:

1. `memory` from a matching override;
2. the peak measured in the last run, saved in `<target dir>/xtest/memory.json`;
3. the profile's `memory`.

`--list` shows each test's estimate and where it came from. The report after
each test shows its measured peak.

## Fixtures

A fixture instance is charged once, from when the first test that needs it
starts until it is torn down: once per test for `scope = "test"`, and once
per host (or binary) for `run` (or `binary`) scope.

A fixture's estimate is its `memory` key, else the peak measured in the last
run (saved as `fixture:<name>`), else a default:

| Kind | Default | Measured as |
|---|---|---|
| `docker` service | 256 MiB | the container's cgroup |
| `docker` environment | not charged | its memory is the tests running in it, which are charged themselves |
| `k3d` | 512 MiB per node | the sum of the node containers' cgroups (not the runner) |
| `command` | 0 | not measured; set `memory` if it starts something big |

Fixture memory is sampled every 500 ms, without file cache the kernel can
reclaim. The report lists each fixture's peak at the end of the run, and
`--list` shows the fixtures' estimates.

## How peaks are measured

| The test runs | Peak |
|---|---|
| as a plain process | max RSS from `wait4`: the largest single process of the test |
| in a container of its own (`scope = "test"`) | the container cgroup's `memory.peak` |
| in a shared container | the RSS of the test's processes, sampled every 50 ms; they are found by the `XTEST_REQ` env var each test gets |

## Limitations

- A test made of several processes at once is measured by its largest one.
- Only the last peak is kept, per test or fixture name.
