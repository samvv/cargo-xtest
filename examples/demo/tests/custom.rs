// harness = false: cargo-xtest runs the binary once as a single test.
fn main() {
    assert_eq!(myapp::add(40, 2), 42);
    println!("custom harness ok");
}
