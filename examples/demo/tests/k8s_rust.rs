// k3d clusters as Rust fixtures, with custom setup logic on top.
// Runs with `cargo xtest --profile k8s` (needs k3d and docker on the host).
use std::process::Command;

use serde::{Deserialize, Serialize};
use xtest::fixture::{K3d, Ready, Setup};

#[derive(Serialize, Deserialize)]
pub struct Cluster {
    pub context: String,
    pub server_ip: String,
}

/// A cluster with the demo's namespace already created.
#[xtest::fixture(scope = "run", memory = "1G", timeout = "5m")]
async fn cluster(cx: &mut Setup) -> xtest::Result<Cluster> {
    let ready = Ready::cmd(["kubectl", "wait", "--for=condition=Ready", "nodes", "--all", "--timeout=10s"])
        .timeout(std::time::Duration::from_secs(180));
    let c = cx.k3d(K3d::new().agents(1).arg("--no-lb").ready(ready)).await?;
    let st = Command::new(c.kubectl())
        .args(["create", "namespace", "xtest-demo"])
        .env("KUBECONFIG", c.kubeconfig())
        .status()?;
    if !st.success() {
        return Err("creating the namespace failed".into());
    }
    Ok(Cluster { context: c.context().to_string(), server_ip: c.server_ip(0).unwrap_or_default().to_string() })
}

#[derive(Serialize, Deserialize)]
pub struct Runner;

/// Tests that use this run inside a container next to a cluster.
#[xtest::fixture(environment, scope = "run", timeout = "5m")]
async fn runner(cx: &mut Setup) -> xtest::Result<Runner> {
    cx.k3d_environment(K3d::new().arg("--no-lb").runner("archlinux:latest")).await?;
    Ok(Runner)
}

fn kubectl(args: &[&str]) -> String {
    let out = Command::new("kubectl").args(args).output().expect("kubectl is not on PATH");
    assert!(out.status.success(), "kubectl {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[xtest::test]
#[ignore = "needs k3d; run with `cargo xtest --profile k8s`"]
fn has_the_namespace(cluster: &Cluster) {
    // KUBECONFIG and kubectl come with the fixture.
    assert!(kubectl(&["get", "namespace", "xtest-demo", "--context", &cluster.context]).contains("xtest-demo"));
    assert!(!cluster.server_ip.is_empty());
}

#[xtest::test]
#[ignore = "needs k3d; run with `cargo xtest --profile k8s`"]
fn runs_next_to_the_cluster(#[fixture(runner)] _env: &Runner) {
    assert!(std::path::Path::new("/.dockerenv").exists(), "not in a container");
    assert_eq!(kubectl(&["get", "nodes", "--no-headers"]).lines().count(), 1);
}
