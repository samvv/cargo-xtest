//! Test attributes and fixtures for [cargo-xtest](https://github.com/samvv/cargo-xtest).
//!
//! # Tests
//!
//! `#[xtest::test]` replaces `#[test]`. It expands to an ordinary `#[test]`,
//! so it works wherever `#[test]` does, including unit tests that use
//! private items:
//!
//! ```
//! #[xtest::test]
//! fn adds() {
//!     assert_eq!(2 + 2, 4);
//! }
//! ```
//!
//! # Fixtures
//!
//! A fixture is an async function marked `#[xtest::fixture]`. Its value is
//! handed to every test that takes a parameter with the fixture's name:
//!
//! ```ignore
//! use xtest::fixture::{Docker, Ready, Setup};
//!
//! #[derive(serde::Serialize, serde::Deserialize)]
//! pub struct Redis {
//!     pub addr: std::net::SocketAddr,
//! }
//!
//! #[xtest::fixture(scope = "run")]
//! pub async fn redis(cx: &mut Setup) -> xtest::Result<Redis> {
//!     let c = cx.docker(Docker::image("redis:7-alpine").port(6379).ready(Ready::tcp(6379))).await?;
//!     Ok(Redis { addr: c.addr(6379) })
//! }
//!
//! #[xtest::test]
//! fn pings(redis: &Redis) {
//!     // ...
//! }
//! ```
//!
//! cargo-xtest sets fixtures up on the test host before the tests that need
//! them, and tears them down afterwards. Under plain `cargo test`, tests
//! with fixtures are ignored. See the cargo-xtest documentation
//! (docs/fixtures/rust.md) for scopes, dependencies, teardown and
//! environments.

use std::marker::PhantomData;

pub mod fixture;

/// Marks a function as a test. Parameters are fixtures. See the [crate documentation](crate).
pub use xtest_macros::test;

/// Marks an async function as a fixture.
///
/// Arguments: `scope = "test" | "binary" | "run"` (default `test`),
/// `memory = "512M"` (what the fixture needs, for scheduling),
/// `timeout = "2m"` (for setup, default 5 minutes), and `environment` for a
/// fixture its tests run inside. The first parameter may be
/// `&mut xtest::fixture::Setup`; the others are fixtures it depends on.
pub use xtest_macros::fixture;

/// Marks the teardown of a fixture: `#[xtest::teardown(fixture)]`. It takes
/// an optional `&mut xtest::fixture::Teardown` and the fixture's value.
pub use xtest_macros::teardown;

/// An error from a fixture or teardown.
pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

/// What fixtures and teardowns return.
pub type Result<T = (), E = Error> = std::result::Result<T, E>;

/// A fixture, as `#[xtest::fixture]` declares it: the fixture function's
/// name refers to this. `T` is the fixture's value.
pub struct Fixture<T> {
    id: &'static str,
    name: &'static str,
    _value: PhantomData<fn() -> T>,
}

impl<T> Fixture<T> {
    #[doc(hidden)]
    pub const fn __new(id: &'static str, name: &'static str) -> Fixture<T> {
        Fixture { id, name, _value: PhantomData }
    }

    /// Identifies the fixture across test binaries: its source file and name.
    pub const fn id(&self) -> &'static str {
        self.id
    }

    pub const fn name(&self) -> &'static str {
        self.name
    }
}

/// Used by the code that the attributes generate. Not a public API.
#[doc(hidden)]
pub mod __private;
