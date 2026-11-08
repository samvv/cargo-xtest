// Runs inside the `borg` docker environment (see .config/xtest.toml).
use std::process::Command;

fn borg(args: &[&str], repo: &std::path::Path) {
    let st = Command::new("borg")
        .args(args)
        .env("BORG_REPO", repo)
        .env("BORG_PASSPHRASE", "")
        .env("BORG_BASE_DIR", std::env::temp_dir())
        .status()
        .expect("borg is not installed");
    assert!(st.success(), "borg {args:?} failed");
}

#[test]
#[ignore = "needs borg; run with `cargo xtest`"]
fn creates_archive() {
    let repo = std::env::temp_dir().join("repo");
    borg(&["init", "--encryption=none"], &repo);
    borg(&["create", "::first", "tests/fixtures"], &repo);
    borg(&["list"], &repo);
}
