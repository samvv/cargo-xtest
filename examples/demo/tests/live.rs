// Writes output slowly, for trying `cargo xtest --no-capture`:
//   cargo xtest --no-capture --include-ignored -E 'binary(live)'
use std::io::Write;
use std::time::Duration;

#[test]
#[ignore = "output demo"]
fn reports_progress() {
    for step in 1..=3 {
        println!("step {step} of 3");
        std::thread::sleep(Duration::from_secs(1));
    }
    eprintln!("a warning on stderr");
    print!("no newline at the end");
    std::io::stdout().flush().unwrap();
}
