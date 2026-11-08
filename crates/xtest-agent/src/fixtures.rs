//! Fixture kinds: Docker containers and user-supplied commands.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::process::Command;
use xtest_proto::*;

use crate::memory;
use crate::session::{Container, Lease, LeaseKind, Session};

pub async fn setup(s: &Session, id: u64, name: &str, role: Role, spec: FixtureSpec, exclusive: bool) -> Result<Reply> {
    let (lease, debug_hint) = match spec {
        FixtureSpec::Docker(d) => docker_setup(s, id, name, role, d, exclusive).await?,
        FixtureSpec::K3d(k) => {
            if role == Role::Environment {
                bail!("k3d fixtures can only be services (`uses`), not environments (`run-in`)");
            }
            k3d_setup(s, id, name, k).await?
        }
        FixtureSpec::Command(c) => {
            if role == Role::Environment {
                bail!("command fixtures can only be services (`uses`), not environments (`run-in`)");
            }
            (command_setup(s, c).await?, None)
        }
    };
    let env = lease.env.clone();
    s.add_lease(id, lease);
    Ok(Reply::Lease { env, debug_hint })
}

pub async fn teardown(s: &Session, lease: Lease) {
    match lease.kind {
        LeaseKind::Container(name) => {
            let _ = docker(["rm", "-f", &name]).await;
        }
        LeaseKind::K3d(cluster) => {
            if let Err(e) = tool("k3d", ["cluster", "delete", &cluster]).await {
                eprintln!("xtest-agent: deleting k3d cluster {cluster}: {e:#}");
            }
        }
        LeaseKind::Command { teardown: Some(cmd), cwd } => {
            let args = ["-c".to_string(), cmd];
            let cmd = s.build_command(Path::new("sh"), &args, &cwd, &BTreeMap::new(), None);
            let out = Command::from(cmd).output().await;
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => eprintln!("xtest-agent: fixture teardown failed: {}", String::from_utf8_lossy(&o.stderr).trim()),
                Err(e) => eprintln!("xtest-agent: fixture teardown failed: {e}"),
            }
        }
        LeaseKind::Command { teardown: None, .. } => {}
    }
}

async fn docker<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    tool("docker", args).await
}

/// Runs a host tool and returns its stdout, or an error with the end of its output.
async fn tool<I, S>(program: &str, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let out = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {program} (is it installed on this host?)"))?;
    if !out.status.success() {
        let sub = args.first().map(|a| a.to_string_lossy().into_owned()).unwrap_or_default();
        bail!("{program} {sub} failed:\n{}", tail(&out.stdout, &out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The last lines of a command's output, for error messages.
fn tail(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!("{}{}", String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr));
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

/// Name of the k3d cluster for a lease. Contains the run id so leftover
/// clusters can be matched to their run; short enough for k3d's name limit.
pub fn k3d_cluster_name(run_id: &str, lease: u64) -> String {
    format!("xt{lease}-{run_id}")
}

async fn k3d_setup(s: &Session, id: u64, fixture: &str, k: K3dSpec) -> Result<(Lease, Option<String>)> {
    if !s.docker {
        bail!("fixture `{fixture}` needs docker for k3d, which is not available on this host");
    }
    let cluster = k3d_cluster_name(&s.run_id, id);
    let kubeconfig = s.run_dir.join("tmp").join(format!("kubeconfig-{id}.yaml"));
    let fill = |t: &str| t.replace("{cluster}", &cluster).replace("{kubeconfig}", &kubeconfig.display().to_string());

    let mut args: Vec<String> = vec![
        "cluster".into(),
        "create".into(),
        cluster.clone(),
        "--wait".into(),
        "--timeout".into(),
        format!("{}s", k.timeout.as_secs().max(1)),
        "--kubeconfig-update-default=false".into(),
        "--kubeconfig-switch-context=false".into(),
        "--servers".into(),
        k.servers.max(1).to_string(),
        "--agents".into(),
        k.agents.to_string(),
    ];
    if let Some(image) = &k.image {
        args.extend(["--image".into(), image.clone()]);
    }
    // Labels on the server nodes let the agent find leftover clusters later.
    for label in [format!("xtest.run={}", s.run_id), format!("xtest.root={}", s.root.display())] {
        args.extend(["--runtime-label".into(), format!("{label}@server:*")]);
    }
    args.extend(k.args.iter().cloned());

    let created = tool("k3d", &args).await.context("creating k3d cluster");
    let ready = async {
        created?;
        let config = tool("k3d", ["kubeconfig", "get", &cluster]).await?;
        std::fs::write(&kubeconfig, config)?;
        let mut env = BTreeMap::from([("KUBECONFIG".to_string(), kubeconfig.display().to_string())]);
        if let Some(r) = &k.ready {
            let cmd: Vec<String> = r.cmd.iter().flatten().map(|a| fill(a)).collect();
            wait_for_command(s, &cmd, &env, r.timeout).await?;
        }
        env.extend(k.export.iter().map(|(name, v)| (name.clone(), fill(v))));
        anyhow::Ok(env)
    }
    .await;
    match ready {
        Ok(env) => {
            let hint = format!("KUBECONFIG={} kubectl get pods -A", kubeconfig.display());
            Ok((Lease { kind: LeaseKind::K3d(cluster), env, container: None }, Some(hint)))
        }
        Err(e) => {
            let _ = tool("k3d", ["cluster", "delete", &cluster]).await;
            Err(e)
        }
    }
}

/// Runs `cmd` on the host (with the sandbox's paths) until it exits 0.
async fn wait_for_command(
    s: &Session,
    cmd: &[String],
    env: &BTreeMap<String, String>,
    timeout: std::time::Duration,
) -> Result<()> {
    let Some((exe, args)) = cmd.split_first() else { return Ok(()) };
    let cwd = s.mirrors().into_iter().next().unwrap_or_else(|| "/".into());
    let deadline = Instant::now() + timeout;
    loop {
        let out = Command::from(s.build_command(Path::new(exe), args, &cwd, env, None)).output().await?;
        if out.status.success() {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("not ready after {timeout:?}; `{}` said:\n{}", cmd.join(" "), tail(&out.stdout, &out.stderr));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn docker_setup(
    s: &Session,
    id: u64,
    fixture: &str,
    role: Role,
    d: DockerSpec,
    exclusive: bool,
) -> Result<(Lease, Option<String>)> {
    if !s.docker {
        bail!("fixture `{fixture}` needs docker, which is not available on this host");
    }
    let image = match &d.image {
        ImageSource::Pull(image) => image.clone(),
        ImageSource::Build { dockerfile, context, tag } => {
            if docker(["image", "inspect", tag]).await.is_err() {
                let df = s.host_path(dockerfile);
                let ctx = s.host_path(context);
                docker(["build".as_ref(), "-t".as_ref(), tag.as_ref(), "-f".as_ref(), df.as_os_str(), ctx.as_os_str()])
                    .await
                    .context("building image")?;
            }
            tag.clone()
        }
    };

    let name = format!("xtest-{}-{id}", s.run_id);
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone()];
    for label in [
        format!("xtest.run={}", s.run_id),
        format!("xtest.root={}", s.root.display()),
        format!("xtest.fixture={fixture}"),
    ] {
        args.extend(["--label".into(), label]);
    }
    for (k, v) in &d.env {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    let mut ports = d.ports.clone();
    match role {
        Role::Environment => {
            let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
            args.extend(["--user".into(), format!("{uid}:{gid}"), "--network".into(), "host".into()]);
            // The uid usually has no passwd entry in the image; tools that
            // look up the user name fall back to these.
            for var in ["USER", "LOGNAME"] {
                if let (Ok(v), false) = (std::env::var(var), d.env.contains_key(var)) {
                    args.extend(["-e".into(), format!("{var}={v}")]);
                }
            }
            // The sandbox appears at the original paths, just like outside docker.
            for m in s.mirrors() {
                args.extend(["-v".into(), format!("{}:{}", s.host_path(&m).display(), m.display())]);
            }
            let tmp = s.run_dir.join("tmp");
            args.extend(["-v".into(), format!("{0}:{0}", tmp.display())]);
            args.extend(["--entrypoint".into(), "sleep".into(), image, "infinity".into()]);
            ports.clear();
        }
        Role::Service => {
            ports.extend(d.ready.as_ref().and_then(|r| r.tcp));
            ports.sort();
            ports.dedup();
            for p in &ports {
                args.extend(["-p".into(), format!("127.0.0.1::{p}")]);
            }
            args.push(image);
        }
    }
    docker(&args).await.context("starting container")?;

    let started = async {
        let mut mapped = BTreeMap::new();
        for p in &ports {
            let out = docker(["port".to_string(), name.clone(), format!("{p}/tcp")]).await?;
            let host_port: u16 = out
                .lines()
                .find_map(|l| l.rsplit(':').next()?.trim().parse().ok())
                .with_context(|| format!("no host port for container port {p}"))?;
            mapped.insert(*p, host_port);
        }
        if let Some(r) = &d.ready {
            wait_ready(&name, r, &mapped).await?;
        }
        let env = d.export.iter().map(|(k, v)| (k.clone(), render(v, &mapped))).collect();
        anyhow::Ok(env)
    }
    .await;

    let env = match started {
        Ok(env) => env,
        Err(e) => {
            let _ = docker(["rm", "-f", &name]).await;
            return Err(e);
        }
    };
    let (container, hint) = match role {
        Role::Environment => {
            let pid = docker(["inspect", "-f", "{{.State.Pid}}", &name]).await?;
            let cgroup = pid.trim().parse().ok().and_then(memory::cgroup_of);
            let c = Container { name: name.clone(), cgroup, exclusive };
            (Some(c), format!("docker exec -it {name} sh"))
        }
        Role::Service => (None, format!("docker logs {name}")),
    };
    Ok((Lease { kind: LeaseKind::Container(name), env, container }, Some(hint)))
}

async fn wait_ready(name: &str, r: &Ready, mapped: &BTreeMap<u16, u16>) -> Result<()> {
    let deadline = Instant::now() + r.timeout;
    loop {
        let running = docker(["inspect", "-f", "{{.State.Running}}", name]).await?;
        if running.trim() != "true" {
            let logs = Command::new("docker").args(["logs", "--tail", "30", name]).output().await?;
            bail!(
                "container exited before it was ready:\n{}{}",
                String::from_utf8_lossy(&logs.stdout),
                String::from_utf8_lossy(&logs.stderr)
            );
        }
        let mut ok = true;
        if let Some(p) = r.tcp {
            let port = mapped[&p];
            ok &= tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok();
        }
        if let (true, Some(cmd)) = (ok, &r.cmd) {
            let mut args = vec!["exec".to_string(), name.to_string()];
            args.extend(cmd.iter().cloned());
            ok &= docker(&args).await.is_ok();
        }
        if ok {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("not ready after {:?}", r.timeout);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Fills in `{host}` and `{port:N}` in an exported value.
fn render(template: &str, ports: &BTreeMap<u16, u16>) -> String {
    let mut s = template.replace("{host}", "127.0.0.1");
    for (p, hp) in ports {
        s = s.replace(&format!("{{port:{p}}}"), &hp.to_string());
    }
    s
}

async fn command_setup(s: &Session, c: CommandSpec) -> Result<Lease> {
    #[derive(Deserialize, Default)]
    struct Out {
        handle: Option<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    }
    let args = ["-c".to_string(), c.setup.clone()];
    let cmd = s.build_command(Path::new("sh"), &args, &c.cwd, &BTreeMap::new(), None);
    let out = Command::from(cmd).output().await?;
    if !out.status.success() {
        bail!("setup command `{}` failed:\n{}", c.setup, String::from_utf8_lossy(&out.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: Out = if stdout.trim().is_empty() {
        Out::default()
    } else {
        serde_json::from_str(stdout.trim())
            .with_context(|| format!("setup command must print JSON, got: {}", stdout.trim()))?
    };
    let handle = parsed.handle.unwrap_or_default();
    let teardown = c.teardown.map(|t| t.replace("{handle}", &shell_quote(&handle)));
    Ok(Lease { kind: LeaseKind::Command { teardown, cwd: c.cwd }, env: parsed.env, container: None })
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_templates() {
        let ports = [(5432, 49153)].into();
        assert_eq!(render("postgres://x@{host}:{port:5432}/db", &ports), "postgres://x@127.0.0.1:49153/db");
    }

    #[test]
    fn quotes_for_shell() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
