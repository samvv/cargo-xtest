//! Test attributes for [cargo-xtest](https://github.com/samvv/cargo-xtest).
//!
//! Add this crate as a dev-dependency and mark tests with `#[xtest::test]`
//! instead of `#[test]`:
//!
//! ```
//! #[xtest::test]
//! fn adds() {
//!     assert_eq!(2 + 2, 4);
//! }
//! ```
//!
//! `#[xtest::test]` expands to an ordinary `#[test]`, so the test still runs
//! under libtest: with `cargo test`, `cargo xtest`, and your editor's
//! "Run test" button. It works anywhere `#[test]` does, including unit tests
//! inside `#[cfg(test)] mod tests` that use private items.
//!
//! The other test attributes go next to it as usual: `#[ignore]`,
//! `#[should_panic]`. A test may return `()` or any type libtest accepts,
//! such as `Result<(), E>`.
//!
//! Not supported (yet): `async fn` tests, parameters, generic tests, and
//! combining it with `#[test]` or another test attribute.

/// Marks a function as a test. See the [crate documentation](crate).
pub use xtest_macros::test;

/// Used by the code that `#[xtest::test]` generates. Not a public API.
#[doc(hidden)]
pub mod __private {
    use std::process::Termination;

    /// Runs a test's body. Called by every `#[xtest::test]` test.
    pub fn run<T: Termination>(body: fn() -> T) -> T {
        body()
    }
}
