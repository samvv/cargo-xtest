// Uses the `greeter` command fixture (see .config/xtest.toml).
#[test]
#[ignore = "needs the greeter fixture; run with `cargo xtest`"]
fn gets_greeting_from_fixture() {
    let state = std::env::var("GREETER_STATE").unwrap();
    assert_eq!(std::fs::read_to_string(state).unwrap().trim(), "hi");
}
