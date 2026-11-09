// Two k3d clusters on one shared network (`network = "mesh"`): a pod in
// cluster B calls a NodePort service in cluster A.
// `cargo xtest --profile k8s` on a host with k3d.
use std::process::Command;

fn kubectl(context: &str, args: &[&str]) -> String {
    let out = Command::new("kubectl").arg("--context").arg(context).args(args).output().expect("kubectl not on PATH");
    assert!(out.status.success(), "kubectl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

#[test]
#[ignore = "needs the mesh-a and mesh-b fixtures"]
fn both_clusters_are_in_kubeconfig() {
    let (a, b) = (var("A_CONTEXT"), var("B_CONTEXT"));
    // KUBECONFIG lists both files, so kubectl knows both contexts.
    let out = Command::new("kubectl").args(["config", "get-contexts", "-o", "name"]).output().unwrap();
    assert!(out.status.success(), "kubectl config get-contexts: {}", String::from_utf8_lossy(&out.stderr));
    let contexts = String::from_utf8_lossy(&out.stdout);
    assert!(contexts.contains(&a) && contexts.contains(&b), "{contexts}");
}

#[test]
#[ignore = "needs the mesh-a and mesh-b fixtures"]
fn cluster_b_reaches_a_nodeport() {
    let (a, b) = (var("A_CONTEXT"), var("B_CONTEXT"));
    let script = "mkdir -p /www && echo hello-from-a > /www/index.html && exec httpd -f -p 8080 -h /www";
    kubectl(&a, &["create", "deployment", "web", "--image=busybox:1.36", "--", "sh", "-c", script]);
    kubectl(&a, &["create", "service", "nodeport", "web", "--tcp=8080:8080", "--node-port=30080"]);
    kubectl(&a, &["rollout", "status", "deployment/web", "--timeout=180s"]);

    let url = format!("http://{}:30080/", var("A_SERVER_IP"));
    let probe = format!("for i in $(seq 30); do wget -qO- -T 3 {url} && exit 0; sleep 2; done; exit 1");
    let out = kubectl(&b, &[
        "run", "probe", "--image=busybox:1.36", "--restart=Never", "--rm", "-i",
        "--pod-running-timeout=3m", "--command", "--", "sh", "-c", &probe,
    ]);
    assert!(out.contains("hello-from-a"), "{out}");
}
