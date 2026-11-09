# cargo-xtest internals

Details behind the configuration in the [README](../README.md): how tests are
selected, and how environment containers work. k3d fixtures have their own
page: [fixtures/k3d.md](fixtures/k3d.md).

## Filters

Filters choose which tests an override applies to (`filter = "..."` in
`.config/xtest.toml`) and which tests run (`-E` on the command line).

### Predicates

| Predicate | Matches | Example |
|---|---|---|
| `package(..)` | the package name | `package(myapp)` |
| `binary(..)` | the test binary (target) name, e.g. `k8s` for `tests/k8s.rs` | `binary(k8s)` |
| `kind(..)` | the target kind: `lib`, `bin`, `test`, `bench`, `example` or `proc-macro` | `kind(test)` |
| `test(..)` | the test's name, e.g. `backup::tests::creates_archive` | `test(archive)` |
| `all()`, `none()` | every test, or none | `all()` |

### Matchers

| Syntax | Meaning |
|---|---|
| `=value` | exactly `value` |
| `~value` | contains `value` |
| `/regex/` | matches the regular expression |
| `value` (no prefix) | contains `value` for `test()`; exactly `value` for the others |

### Operators

From weakest to strongest binding: `|` (or `or`), `&` (or `and`), and `!`
(or `not`). Use parentheses to group.

```
package(db) & test(/integration/) & !kind(bench)
binary(k8s) | binary(k8s_env)
```

Several `-E` flags are combined with *or*: a test runs if any of them matches.

## Environment containers

A Docker fixture used with `run-in` is the test's *environment*: the test
process runs inside the container (through `docker exec`). The container:

- runs as your uid and gid, with `USER` and `LOGNAME` set;
- uses the host network (`--network host`);
- has the run's sandbox mounted at its original absolute paths, so the test
  sees the same paths as on your machine;
- starts with `sleep infinity` as entrypoint, so it stays up until teardown.

The test binary is built on your machine and runs unchanged in the image. The
image therefore needs a glibc at least as new as yours, unless you build for a
musl target (`target = "x86_64-unknown-linux-musl"`).
