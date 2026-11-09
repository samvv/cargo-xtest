# Filters

Filters choose which tests run (`-E` on the command line) and which tests an
override applies to (`filter = "..."` in the [configuration](configuration.md)).

```
cargo xtest -E 'package(db) & test(/integration/) & !kind(bench)'
```

## Predicates

| Predicate | Matches | Example |
|---|---|---|
| `package(..)` | the package name | `package(myapp)` |
| `binary(..)` | the test binary (target) name, e.g. `k8s` for `tests/k8s.rs` | `binary(k8s)` |
| `kind(..)` | the target kind: `lib`, `bin`, `test`, `bench`, `example` or `proc-macro` | `kind(test)` |
| `test(..)` | the test's name, e.g. `backup::tests::creates_archive` | `test(archive)` |
| `all()`, `none()` | every test, or none | `all()` |

## Matchers

| Syntax | Meaning |
|---|---|
| `=value` | exactly `value` |
| `~value` | contains `value` |
| `/regex/` | matches the regular expression |
| `value` (no prefix) | contains `value` for `test()`; exactly `value` for the others |

## Operators

From weakest to strongest binding: `|` (or `or`), `&` (or `and`), and `!`
(or `not`). Use parentheses to group.

```
binary(k8s) | binary(k8s_env)
package(db) & !(test(slow) | kind(bench))
```

## On the command line

- Several `-E` flags are combined with *or*: a test runs if any of them
  matches.
- Plain arguments (`cargo xtest foo bar`) select tests whose name contains
  one of them, like `cargo test`. They combine with `-E` using *and*.
- A plain argument that is literally `clean` is read as the `clean` command;
  write `-E 'test(clean)'` instead.
