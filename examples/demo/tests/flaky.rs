// Fails on its first try in a run and passes on the next, for trying
// retries: cargo xtest --retries 2 --include-ignored -E 'binary(flaky)'
//
// Every test process gets its own TMPDIR under the run's tmp directory, so
// the parent directory is shared by all tries in one run.

#[test]
#[ignore = "retry demo"]
fn passes_on_the_second_try() {
    let marker = std::env::temp_dir().parent().unwrap().join("flaky-tried-once");
    if !marker.exists() {
        std::fs::write(&marker, "").unwrap();
        panic!("first try fails on purpose");
    }
}
