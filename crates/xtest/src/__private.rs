//! Called by the code that `#[xtest::test]`, `#[xtest::fixture]` and
//! `#[xtest::teardown]` generate.

use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::process::Termination;

use serde::de::DeserializeOwned;
use serde::Serialize;
use xtest_proto::fx::{self, env};

use crate::fixture::{Setup, Shutdown, Teardown};
use crate::Fixture;

pub use xtest_proto::meta::{record, record_len};

/// Keeps a metadata record in the binary: the linker can't drop what
/// running code refers to.
pub fn keep<T>(record: &T) {
    std::hint::black_box(record);
}

/// The value type of a fixture, from its function's return type.
pub trait FixtureOutput {
    type Value;
}

impl<T, E> FixtureOutput for std::result::Result<T, E> {
    type Value = T;
}

pub type Value<R> = <R as FixtureOutput>::Value;

/// What a teardown may return.
pub trait TeardownOutput {
    fn into_result(self) -> Result<(), String>;
}

impl TeardownOutput for () {
    fn into_result(self) -> Result<(), String> {
        Ok(())
    }
}

impl<E: Display> TeardownOutput for Result<(), E> {
    fn into_result(self) -> Result<(), String> {
        self.map_err(|e| e.to_string())
    }
}

/// Fixture values, from the JSON file named by an env var.
pub struct Values(serde_json::Map<String, serde_json::Value>);

impl Values {
    fn load() -> Values {
        let Some(file) = std::env::var_os(env::FIXTURES) else { return Values(Default::default()) };
        let data = std::fs::read(&file).unwrap_or_else(|e| panic!("reading fixture values from {}: {e}", file.to_string_lossy()));
        Values(serde_json::from_slice(&data).expect("fixture values are not JSON"))
    }

    pub fn get<T: DeserializeOwned>(&self, f: &Fixture<T>) -> T {
        let Some(v) = self.0.get(f.id()) else {
            panic!(
                "fixture `{}` was not set up for this test; cargo-xtest may be older than the xtest crate",
                f.name()
            )
        };
        serde_json::from_value(v.clone())
            .unwrap_or_else(|e| panic!("the value of fixture `{}` doesn't match its type: {e}", f.name()))
    }
}

/// Runs a test without fixtures.
pub fn run<T: Termination>(body: fn() -> T) -> T {
    body()
}

/// Runs a test that takes fixtures (`names`, for messages).
pub fn run_with<T: Termination>(names: &[&str], body: impl FnOnce(&Values) -> T) -> T {
    if std::env::var_os(env::XTEST).is_none() {
        panic!("this test uses fixtures ({}); run it with `cargo xtest`", names.join(", "));
    }
    body(&Values::load())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("starting a tokio runtime")
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// The hidden setup test of a fixture. Outside of cargo-xtest it does nothing.
pub fn fixture_main<T, E, F>(f: &Fixture<T>, body: F)
where
    T: Serialize,
    E: Display,
    F: FnOnce(Setup, &'static Values) -> BoxFuture<(Setup, Result<T, E>)>,
{
    if std::env::var(env::MODE).as_deref() != Ok("setup") {
        return;
    }
    let socket = std::env::var_os(env::SOCKET).expect("XTEST_SOCKET is not set");
    // Lives as long as the process; a serving fixture may hold on to it.
    let values: &'static Values = Box::leak(Box::new(Values::load()));
    runtime().block_on(async {
        // Installed early, so SIGTERM never kills the process before
        // `serve` has had a chance to stop.
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("handling SIGTERM");
        let conn = tokio::net::UnixStream::connect(&socket)
            .await
            .unwrap_or_else(|e| panic!("connecting to cargo-xtest's agent: {e}"));
        let (mut cx, result) = body(Setup::new(conn), values).await;
        let value = match result {
            Ok(v) => v,
            Err(e) => panic!("fixture `{}` failed: {e}", f.name()),
        };
        let value = serde_json::to_string(&value)
            .unwrap_or_else(|e| panic!("fixture `{}`: its value can't be serialized: {e}", f.name()));
        let serve = cx.serve.take();
        cx.call(fx::Request::Ready { value, serving: serve.is_some() })
            .await
            .unwrap_or_else(|e| panic!("fixture `{}`: {e}", f.name()));
        if let Some(serve) = serve {
            let shutdown = Shutdown(Box::pin(async move {
                term.recv().await;
            }));
            if let Err(e) = serve(shutdown).await {
                panic!("fixture `{}` stopped serving with an error: {e}", f.name());
            }
        }
    });
}

/// The hidden teardown test of a fixture. Outside of cargo-xtest it does nothing.
pub fn teardown_main<T, R, F>(f: &Fixture<T>, body: F)
where
    T: DeserializeOwned,
    R: TeardownOutput,
    F: FnOnce(Teardown, T) -> BoxFuture<R>,
{
    if std::env::var(env::MODE).as_deref() != Ok("teardown") {
        return;
    }
    let file = std::env::var_os(env::VALUE).expect("XTEST_VALUE is not set");
    let data = std::fs::read(&file).expect("reading the fixture's value");
    let value: T = serde_json::from_slice(&data)
        .unwrap_or_else(|e| panic!("the value of fixture `{}` doesn't match its type: {e}", f.name()));
    let cx = Teardown {
        failed: std::env::var(env::FAILED).as_deref() == Ok("1"),
        attach_dir: std::env::var_os(env::ATTACH_DIR).map(Into::into),
    };
    if let Err(e) = runtime().block_on(body(cx, value)).into_result() {
        panic!("teardown of fixture `{}` failed: {e}", f.name());
    }
}
