# `#[xtest::test]`

The `xtest` crate provides `#[xtest::test]`, a drop-in replacement for
`#[test]`. Without parameters it behaves exactly like `#[test]`. Its
parameters are [fixtures written in Rust](fixtures/rust.md).

```toml
[dev-dependencies]
xtest = { git = "https://github.com/samvv/cargo-xtest" }
```

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[xtest::test]
    fn doubles() {
        assert_eq!(double(21), 42); // private functions work, as with #[test]
    }

    #[xtest::test]
    #[ignore = "slow"]
    fn slow_one() -> Result<(), String> {
        Ok(())
    }
}
```

## How it works

`#[xtest::test]` expands to an ordinary libtest `#[test]` with the same name,
so it works wherever `#[test]` does: integration tests in `tests/`, unit
tests in `src/` with access to private items, `cargo test`, `cargo xtest`
and your editor's "Run test" button. The body moves into a nested function
that `#[xtest::test]` calls; it sees the same module scope.

Other test attributes go next to it as usual: `#[ignore]`,
`#[should_panic]`. A test may return `()` or anything libtest accepts, such
as `Result<(), E>`.

A test with fixture parameters is also `#[ignore]`d, so plain `cargo test`
skips it; cargo-xtest runs it anyway, unless you ignored it yourself.

## Not supported

These give a compile error:

- `async fn` tests (planned: [#6](https://github.com/samvv/cargo-xtest/issues/6))
- generic tests, `const`, `unsafe` and `extern` functions
- parameters that aren't fixtures, or that take a fixture as `&mut`
- another test attribute below it, such as `#[test]` or `#[tokio::test]`

Don't put `#[test]` *above* `#[xtest::test]` either. The macro can't see it
there, and the test is registered twice. cargo-xtest notices the repeated
name, warns, and runs the test once; plain `cargo test` runs it twice.

Don't import the crate with `use xtest::*`: its `test` then clashes with
the built-in `#[test]`. Write `#[xtest::test]`.
