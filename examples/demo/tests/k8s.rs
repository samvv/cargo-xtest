// Use the `k8s` k3d fixture: `cargo xtest --profile k8s` on a host with k3d.
use std::process::Command;

fn kubectl(args: &[&str]) -> String {
    // The fixture puts a kubectl matching the cluster on PATH.
    let out = Command::new("kubectl").args(args).output().expect("kubectl not on PATH");
    assert!(out.status.success(), "kubectl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "needs the k8s fixture"]
fn api_server_is_reachable() {
    let config = std::fs::read_to_string(std::env::var("KUBECONFIG").unwrap()).unwrap();
    let server = config.lines().find_map(|l| l.trim().strip_prefix("server: https://")).unwrap();
    std::net::TcpStream::connect(server.replace("0.0.0.0", "127.0.0.1")).unwrap();
}

#[test]
#[ignore = "needs the k8s fixture"]
fn all_nodes_are_ready() {
    let nodes = kubectl(&["get", "nodes", "--no-headers"]);
    assert_eq!(nodes.lines().count(), 2, "{nodes}");
    assert!(nodes.lines().all(|l| l.split_whitespace().nth(1) == Some("Ready")), "{nodes}");
}

#[test]
#[ignore = "needs the k8s fixture"]
fn can_create_namespace() {
    kubectl(&["create", "namespace", "xtest-demo"]);
    assert!(kubectl(&["get", "namespaces"]).contains("xtest-demo"));
}
