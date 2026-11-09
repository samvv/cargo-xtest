# Memory scheduling

Besides its slots, each host only starts a test while there is enough free
memory for it. cargo-xtest learns how much memory each test needs by
measuring it.

```toml
[profile.default]
memory = "256M"            # assumed for tests not measured yet
memory-reserve = "512M"    # kept free on every host

[[profile.default.overrides]]
filter = "test(=big_import)"
memory = "2G"              # wins over the measured peak
```

## Keys

| Key | Where | Default | Meaning |
|---|---|---|---|
| `memory` | profile | `"256M"` | estimate for tests that haven't been measured yet |
| `memory-reserve` | profile | `"512M"` | memory kept free on every host |
| `memory` | override | none | estimate for the matching tests; wins over the measured peak |

## When a test starts

Before starting a test, the controller asks the host's agent how much memory
is available (`MemAvailable`) and how much each running test uses right now.
It then computes:

```
free = MemAvailable − Σ(estimate − current use of each running test) − memory-reserve
```

Running tests that haven't reached their estimate yet still have that memory
reserved. The controller starts the biggest queued test that fits in `free`,
and checks again every 250 ms while tests are waiting.

A host with nothing running always starts its next test, so a test bigger
than any host still runs, with a warning.

## Estimates

A test's estimate is the first of:

1. `memory` from a matching override;
2. the peak measured in the last run, saved in `<target dir>/xtest/memory.json`;
3. the profile's `memory`.

`--list` shows each test's estimate and where it came from. The report after
each test shows its measured peak.

## How peaks are measured

| The test runs | Peak |
|---|---|
| as a plain process | max RSS from `wait4`: the largest single process of the test |
| in a container of its own (`scope = "test"`) | the container cgroup's `memory.peak` |
| in a shared container | the RSS of the test's processes, sampled every 50 ms; they are found by the `XTEST_REQ` env var each test gets |

## Limitations

- Fixture memory isn't charged to the tests that use them: services and k3d
  clusters only show up as lower `MemAvailable` once they run
  ([#5](https://github.com/samvv/cargo-xtest/issues/5)).
- A test made of several processes at once is measured by its largest one.
- Only the last peak is kept, per test name.
