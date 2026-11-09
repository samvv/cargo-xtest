// Runs inside the `k8s` fixture's runner container (`run-in = "k8s"`).
use std::process::Command;

#[test]
#[ignore = "needs the k8s fixture"]
fn runs_in_the_runner_container() {
    assert!(std::path::Path::new("/.dockerenv").exists(), "not in a container");
    // The sandbox is still at its original path.
    assert!(std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).exists());
}

#[test]
#[ignore = "needs the k8s fixture"]
fn kubectl_reaches_the_cluster() {
    let out = Command::new("kubectl").args(["get", "nodes", "--no-headers"]).output().expect("kubectl not on PATH");
    let nodes = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(nodes.lines().count(), 2, "{nodes}");
}
