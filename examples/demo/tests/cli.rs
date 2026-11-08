#[test]
fn runs_the_binary() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_myapp")).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "myapp 3");
}

#[test]
fn reads_fixture_relative_to_cwd() {
    assert_eq!(std::fs::read_to_string("tests/fixtures/hello.txt").unwrap().trim(), "hello");
}
