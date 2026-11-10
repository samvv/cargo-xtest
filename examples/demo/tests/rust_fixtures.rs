// Tests that use the fixtures in tests/common, written in Rust.
mod common;

use common::*;

#[xtest::test]
fn sets_and_gets(#[fixture(keyspace)] ks: &Keyspace) {
    assert_eq!(redis_cmd(ks.addr, &["SET", &ks.key("a"), "1"]), "+OK");
    assert_eq!(redis_cmd(ks.addr, &["GET", &ks.key("a")]), "$1");
}

#[xtest::test]
fn gets_its_own_keyspace(keyspace: &Keyspace) {
    // Another test's keys are not visible under this prefix.
    assert_eq!(redis_cmd(keyspace.addr, &["GET", &keyspace.key("a")]), "$-1");
    assert_eq!(redis_cmd(keyspace.addr, &["GET", &keyspace.key("created")]), "$1");
}

#[xtest::test]
fn sees_the_exported_env_var(redis: &Redis) {
    assert_eq!(std::env::var("REDIS_ADDR").unwrap(), redis.addr.to_string());
}

#[xtest::test]
fn echoes(echo: &Echo) {
    assert_eq!(echo_roundtrip(echo.addr, "hello"), "hello");
}

#[xtest::test]
fn echoes_again_from_the_same_server(echo: &Echo) {
    assert_eq!(echo_roundtrip(echo.addr, "again"), "again");
}

#[xtest::test]
fn reads_the_greeters_state(greeter: &Greeter) {
    assert_eq!(std::fs::read_to_string(&greeter.state).unwrap().trim(), "hi");
    assert_eq!(std::env::var("GREETER_STATE").unwrap(), greeter.state.display().to_string());
}
