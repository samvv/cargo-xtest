// Runs inside the `borg` environment from tests/common (a Docker container
// built from tests/docker/borg.Dockerfile), and shares the run-scoped Redis
// with tests/rust_fixtures.rs.
mod common;

use common::*;

#[xtest::test]
fn runs_inside_the_borg_container(#[fixture(borg)] _env: &Borg) {
    let out = std::process::Command::new("borg").arg("--version").output().expect("borg is not installed");
    assert!(out.status.success());
}

#[xtest::test]
fn shares_redis_with_other_binaries(redis: &Redis) {
    assert_eq!(redis_cmd(redis.addr, &["PING"]), "+PONG");
}
