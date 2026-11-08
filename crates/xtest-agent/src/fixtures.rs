//! Fixture kinds: Docker containers and user-supplied commands.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::process::Command;
use xtest_proto::*;

use crate::session::{Lease, LeaseKind, Session};

pub async fn setup(s: &Session, id: u64, name: &str, role: Role, spec: FixtureSpec) -> Result<Reply> {
    let (lease, debug_hint) = match spec {
        FixtureSpec::Docker(d) => docker_setup(s, id, name, role, d).await?,
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
        LeaseKind::Command { teardown: Some(cmd), cwd } => {
            let args = ["-c".to_string(), cmd];
            let out = s.build_command(Path::new("sh"), &args, &cwd, &BTreeMap::new(), None).output().await;
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
    let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let out = Command::new("docker").args(&args).stdin(Stdio::null()).output().await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(30).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        bail!("docker {} failed:\n{}", args[0].to_string_lossy(), tail.join("\n"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn docker_setup(
    s: &Session,
    id: u64,
    fixture: &str,
    role: Role,
    d: DockerSpec,
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
                    .with_context(|| format!("building image for fixture `{fixture}`"))?;
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
    docker(&args).await.with_context(|| format!("starting fixture `{fixture}`"))?;

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
            return Err(e.context(format!("fixture `{fixture}`")));
        }
    };
    let (container, hint) = match role {
        Role::Environment => (Some(name.clone()), format!("docker exec -it {name} sh")),
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
    let out = s.build_command(Path::new("sh"), &args, &c.cwd, &BTreeMap::new(), None).output().await?;
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
