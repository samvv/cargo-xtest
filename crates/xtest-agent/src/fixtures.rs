//! Fixture kinds: Docker containers, k3d clusters, user-supplied commands,
//! and Rust fixtures (see [`crate::rust_fixture`]), which are built from the
//! first two.

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

pub async fn setup(
    s: &Session,
    id: u64,
    name: &str,
    role: Role,
    spec: FixtureSpec,
    exclusive: bool,
    deps: &[u64],
) -> Result<Reply> {
    let (lease, debug_hint) = match spec {
        FixtureSpec::Docker(d) => {
            let (lease, hint, _) = docker_setup(s, id, name, role, d, exclusive).await?;
            (lease, hint)
        }
        FixtureSpec::Rust(r) => crate::rust_fixture::setup(s, id, name, r, exclusive, deps).await?,
        FixtureSpec::K3d(k) => k3d_setup(s, id, name, role, k, exclusive).await?,
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

/// Removes what a lease holds. Only Rust fixtures have something to report.
pub async fn teardown(s: &Session, lease: Lease, failed: bool) -> TeardownReport {
    match lease.kind {
        LeaseKind::Rust(r) => return Box::pin(crate::rust_fixture::teardown(s, *r, lease.value, failed)).await,
        LeaseKind::Process(mut child) => {
            if let Some(pid) = child.id() {
                crate::session::kill_group(pid as i32);
            }
            let _ = child.wait().await;
        }
        LeaseKind::Cleanup(file) => crate::rust_fixture::run_cleanup(&file),
        LeaseKind::Container(name) => {
            let _ = docker(["rm", "-f", &name]).await;
        }
        LeaseKind::K3d { cluster, runner } => {
            if let Some(r) = runner {
                let _ = docker(["rm", "-f", &r]).await;
            }
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
    TeardownReport::default()
}

pub(crate) async fn docker<I, S>(args: I) -> Result<String>
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
pub(crate) fn tail(stdout: &[u8], stderr: &[u8]) -> String {
    let text = format!("{}{}", String::from_utf8_lossy(stdout), String::from_utf8_lossy(stderr));
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(30)..].join("\n")
}

/// Name of the k3d cluster for a lease. Contains the run id so leftover
/// clusters can be matched to their run; short enough for k3d's name limit.
pub fn k3d_cluster_name(run_id: &str, lease: u64) -> String {
    format!("xt{lease}-{run_id}")
}

pub(crate) async fn k3d_setup(
    s: &Session,
    id: u64,
    fixture: &str,
    role: Role,
    k: K3dSpec,
    exclusive: bool,
) -> Result<(Lease, Option<String>)> {
    if !s.docker {
        bail!("fixture `{fixture}` needs docker for k3d, which is not available on this host");
    }
    if role == Role::Environment && k.runner.is_none() {
        bail!("k3d fixture `{fixture}` needs a `runner` image to be used with `run-in`");
    }
    let cluster = k3d_cluster_name(&s.run_id, id);
    let kubeconfig = s.run_dir.join("tmp").join(format!("kubeconfig-{id}.yaml"));
    let network = k.network.as_ref().map(|n| k3d_network_name(&s.run_id, n));

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
    if let Some(net) = &network {
        args.extend(["--network".into(), net.clone()]);
    }
    // Labels on the server nodes let the agent find leftover clusters later.
    for label in run_labels(s) {
        args.extend(["--runtime-label".into(), format!("{label}@server:*")]);
    }
    args.extend(k.args.iter().cloned());

    let created = async {
        if let Some(net) = &network {
            ensure_network(s, net).await.context("creating the shared network")?;
        }
        tool("k3d", &args).await.context("creating k3d cluster")
    }
    .await;
    let mut runner: Option<Container> = None;
    let ready = async {
        created?;
        let config = tool("k3d", ["kubeconfig", "get", &cluster]).await?;
        std::fs::write(&kubeconfig, config)?;
        let vars = k3d_vars(&cluster, &kubeconfig, network.as_deref()).await;
        let bin = provide_kubectl(s, &cluster, id).await.context("providing kubectl")?;
        let env = BTreeMap::from([("KUBECONFIG".to_string(), kubeconfig.display().to_string())]);
        let lease = Lease { env, path: vec![bin], ..Lease::new(LeaseKind::K3d { cluster: cluster.clone(), runner: None }) };
        if let Some(r) = &k.ready {
            let cmd: Vec<String> = r.cmd.iter().flatten().map(|a| fill(a, &vars)).collect::<Result<_>>()?;
            let mut env = lease.env.clone();
            // Same PATH the tests get, so the ready command can use kubectl.
            let path = std::env::var("PATH").unwrap_or_default();
            env.insert("PATH".into(), format!("{}:{path}", lease.path[0].display()));
            wait_for_command(s, &cmd, &env, r.timeout).await?;
        }
        if let (Role::Environment, Some(image)) = (role, &k.runner) {
            let image = ensure_image(s, image).await.context("runner image")?;
            runner = Some(start_environment(s, id, fixture, &image, &BTreeMap::new(), exclusive).await?);
        }
        let mut lease = lease;
        for (name, v) in &k.export {
            lease.env.insert(name.clone(), fill(v, &vars).with_context(|| format!("in export `{name}`"))?);
        }
        anyhow::Ok(lease)
    }
    .await;
    match ready {
        Ok(mut lease) => {
            let hint = match &runner {
                Some(c) => format!("docker exec -it {} sh", c.name),
                None => format!("KUBECONFIG={} {}/kubectl get pods -A", kubeconfig.display(), lease.path[0].display()),
            };
            // The cluster's nodes (not the runner, whose memory is the tests').
            let mut cgroups = Vec::new();
            if let Ok(ids) = docker(["ps", "-q", "--filter", &format!("label=k3d.cluster={cluster}")]).await {
                for id in ids.split_whitespace() {
                    cgroups.extend(container_cgroup(id).await);
                }
            }
            lease.memory = (!cgroups.is_empty()).then(|| memory::FixtureMemory::watch(cgroups));
            lease.kind = LeaseKind::K3d { cluster, runner: runner.as_ref().map(|c| c.name.clone()) };
            lease.container = runner;
            Ok((lease, Some(hint)))
        }
        Err(e) => {
            if let Some(c) = runner {
                let _ = docker(["rm", "-f", &c.name]).await;
            }
            let _ = tool("k3d", ["cluster", "delete", &cluster]).await;
            Err(e)
        }
    }
}

/// Name of the docker network that k3d fixtures naming `network = name` share.
pub fn k3d_network_name(run_id: &str, name: &str) -> String {
    format!("xt-{run_id}-{name}")
}

/// Creates a run's shared network unless another fixture already did. It is
/// removed when the session ends (see [`remove_networks`]).
async fn ensure_network(s: &Session, name: &str) -> Result<()> {
    if docker(["network", "inspect", name]).await.is_ok() {
        return Ok(());
    }
    let mut args = vec!["network".to_string(), "create".into()];
    for l in run_labels(s) {
        args.extend(["--label".into(), l]);
    }
    args.push(name.into());
    match docker(&args).await {
        // Another cluster may have created it at the same moment.
        Err(_) if docker(["network", "inspect", name]).await.is_ok() => Ok(()),
        r => r.map(drop),
    }
}

/// Removes the run's shared networks. One still used by a kept cluster
/// can't be removed; garbage collection takes it later.
pub async fn remove_networks(s: &Session) {
    let Ok(ids) = docker(["network", "ls", "-q", "--filter", &format!("label=xtest.run={}", s.run_id)]).await else {
        return;
    };
    for id in ids.split_whitespace() {
        let _ = docker(["network", "rm", id]).await;
    }
}

/// Values for k3d templates, and why node IPs are missing if they are.
pub(crate) struct Vars {
    pub values: BTreeMap<String, String>,
    nodes_error: Option<String>,
}

/// Values for the k3d templates: names, paths and node IPs on the cluster's
/// network. Failing to look up IPs (e.g. the cluster uses a network of its own
/// via `args`) is only an error once a template needs them.
pub(crate) async fn k3d_vars(cluster: &str, kubeconfig: &Path, network: Option<&str>) -> Vars {
    let net = network.map(String::from).unwrap_or_else(|| format!("k3d-{cluster}"));
    let mut values = BTreeMap::from([
        ("cluster".to_string(), cluster.to_string()),
        ("context".to_string(), format!("k3d-{cluster}")),
        ("kubeconfig".to_string(), kubeconfig.display().to_string()),
        ("network".to_string(), net.clone()),
    ]);
    let out = docker(["network", "inspect", &net, "-f", "{{range .Containers}}{{.Name}}={{.IPv4Address}} {{end}}"]).await;
    let nodes_error = match out {
        Err(e) => Some(format!("could not look up node IPs on network `{net}`: {e:#}")),
        Ok(out) => {
            for entry in out.split_whitespace() {
                let Some((name, addr)) = entry.split_once('=') else { continue };
                let ip = addr.split('/').next().unwrap_or_default();
                let Some(node) = name.strip_prefix(&format!("k3d-{cluster}-")) else { continue };
                if ip.is_empty() {
                    continue; // no IPv4 address (e.g. an IPv6-only network)
                }
                for role in ["server", "agent"] {
                    if let Some(n) = node.strip_prefix(&format!("{role}-")) {
                        values.insert(format!("{role}:{n}"), ip.to_string());
                    }
                }
            }
            if let Some(ip) = values.get("server:0").cloned() {
                values.insert("server-ip".into(), ip);
            }
            None
        }
    };
    Vars { values, nodes_error }
}

/// Whether `key` (without braces) is one of [`K3D_TEMPLATES`]; `N` stands for digits.
fn is_k3d_template(key: &str) -> bool {
    K3D_TEMPLATES.iter().any(|t| {
        let t = &t[1..t.len() - 1];
        match t.strip_suffix('N') {
            Some(prefix) => key
                .strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
            None => key == t,
        }
    })
}

/// Fills `{name}` templates. A template without a value (e.g. `{agent:3}`
/// with one agent) is an error; other braces, such as shell `${X}` or
/// literal JSON, are left alone.
fn fill(text: &str, vars: &Vars) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let key = after.find('}').map(|end| &after[..end]);
        match key.and_then(|k| vars.values.get(k).map(|v| (k, v))) {
            Some((k, v)) => {
                out.push_str(v);
                rest = &after[k.len() + 1..];
            }
            None => {
                if let Some(k) = key.filter(|k| is_k3d_template(k)) {
                    match &vars.nodes_error {
                        Some(e) => bail!("`{{{k}}}` has no value: {e}"),
                        None => bail!("`{{{k}}}` has no value: this cluster has no such node with an IPv4 address"),
                    }
                }
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Makes `kubectl` available from the cluster itself: the k3s binary in the
/// node image is static and acts as kubectl when called by that name. It is
/// copied once per image into `<root>/tools`, then hardlinked into the run's
/// tmp dir (which environment containers can see too). Returns the bin dir.
async fn provide_kubectl(s: &Session, cluster: &str, id: u64) -> Result<std::path::PathBuf> {
    let server = format!("k3d-{cluster}-server-0");
    let image = docker(["inspect", "-f", "{{.Image}}", &server]).await?;
    let image = image.trim().trim_start_matches("sha256:");
    let cached = s.root.join("tools/k3s").join(&image[..image.len().min(16)]).join("k3s");
    let bin = s.run_dir.join("tmp").join(format!("k3d-{id}")).join("bin");
    std::fs::create_dir_all(&bin)?;
    let kubectl = bin.join("kubectl");
    // Twice: another agent's GC may remove an unused cache entry between
    // our check and the link.
    for attempt in 0.. {
        if !cached.exists() {
            std::fs::create_dir_all(cached.parent().unwrap())?;
            // Unique per agent and lease: two clusters may be set up at once.
            let tmp = cached.with_extension(format!("tmp.{}.{id}", std::process::id()));
            docker(["cp".as_ref(), format!("{server}:/bin/k3s").as_ref(), tmp.as_os_str()]).await?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555))?;
            std::fs::rename(&tmp, &cached)?;
        }
        crate::gc::touch(&cached);
        let res = std::fs::hard_link(&cached, &kubectl).or_else(|_| std::fs::copy(&cached, &kubectl).map(drop));
        match res {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && attempt == 0 => continue,
            r => r?,
        }
        break;
    }
    Ok(bin)
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

/// The cgroup of a running container, if its memory can be read.
async fn container_cgroup(name: &str) -> Option<std::path::PathBuf> {
    let pid = docker(["inspect", "-f", "{{.State.Pid}}", name]).await.ok()?;
    pid.trim().parse().ok().and_then(memory::cgroup_of)
}

/// Pulls or builds an image; returns the name to run. A build without a tag
/// (from a Rust fixture) is tagged with a hash of its Dockerfile and context.
async fn ensure_image(s: &Session, image: &ImageSource) -> Result<String> {
    Ok(match image {
        ImageSource::Pull(image) => image.clone(),
        ImageSource::Build { dockerfile, context, tag } => {
            let tag = &if tag.is_empty() { build_tag(s, dockerfile, context)? } else { tag.clone() };
            // Marks the image as used, so GC keeps it (see `gc::collect_images`).
            let marker = crate::gc::image_marker(&s.root, tag);
            std::fs::create_dir_all(marker.parent().unwrap())?;
            std::fs::File::create(&marker)?.set_modified(std::time::SystemTime::now())?;
            if docker(["image", "inspect", tag]).await.is_err() {
                let df = s.host_path(dockerfile);
                let ctx = s.host_path(context);
                let label = format!("xtest.root={}", s.root.display());
                docker([
                    "build".as_ref(),
                    "--label".as_ref(),
                    label.as_ref(),
                    "-t".as_ref(),
                    tag.as_ref(),
                    "-f".as_ref(),
                    df.as_os_str(),
                    ctx.as_os_str(),
                ])
                .await
                .context("building image")?;
            }
            tag.clone()
        }
    })
}

/// Labels that tie docker objects to this run, so GC can find leftovers.
fn run_labels(s: &Session) -> [String; 2] {
    [format!("xtest.run={}", s.run_id), format!("xtest.root={}", s.root.display())]
}

fn labels(s: &Session, fixture: &str) -> Vec<String> {
    let [run, root] = run_labels(s);
    [run, root, format!("xtest.fixture={fixture}")]
        .into_iter()
        .flat_map(|l| ["--label".to_string(), l])
        .collect()
}

/// Starts a container that tests run in: as the agent's uid, on the host
/// network, with the sandbox mounted at its original paths.
async fn start_environment(
    s: &Session,
    id: u64,
    fixture: &str,
    image: &str,
    env: &BTreeMap<String, String>,
    exclusive: bool,
) -> Result<Container> {
    let name = format!("xtest-{}-{id}", s.run_id);
    // Test binaries are built for the host, so the image must be too.
    if docker(["image", "inspect", image]).await.is_err() {
        docker(["pull", image]).await.with_context(|| format!("pulling image `{image}`"))?;
    }
    let arch = docker(["image", "inspect", "-f", "{{.Architecture}}", image]).await?;
    let arch = crate::compat::docker_arch(arch.trim());
    if arch != std::env::consts::ARCH {
        bail!(
            "image `{image}` is built for {arch}, but this host and the tests are {}; \
             use an image (or `--platform`) for {}",
            std::env::consts::ARCH,
            std::env::consts::ARCH
        );
    }
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone()];
    args.extend(labels(s, fixture));
    args.extend(["--user".into(), format!("{uid}:{gid}"), "--network".into(), "host".into()]);
    for (k, v) in env {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    // The uid usually has no passwd entry in the image; tools that look up
    // the user name fall back to these.
    for var in ["USER", "LOGNAME"] {
        if let (Ok(v), false) = (std::env::var(var), env.contains_key(var)) {
            args.extend(["-e".into(), format!("{var}={v}")]);
        }
    }
    // The sandbox appears at the original paths, just like outside docker.
    for m in s.mirrors() {
        args.extend(["-v".into(), format!("{}:{}", s.host_path(&m).display(), m.display())]);
    }
    let tmp = s.run_dir.join("tmp");
    args.extend(["-v".into(), format!("{0}:{0}", tmp.display())]);
    args.extend(["--entrypoint".into(), "sleep".into(), image.into(), "infinity".into()]);
    docker(&args).await.context("starting container")?;

    let inspected = async {
        let cgroup = container_cgroup(&name).await;
        let envs = docker(["inspect", "-f", "{{range .Config.Env}}{{println .}}{{end}}", &name]).await?;
        let path = envs
            .lines()
            .find_map(|l| l.strip_prefix("PATH="))
            .unwrap_or("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .to_string();
        let probe = "getconf GNU_LIBC_VERSION 2>/dev/null || ldd --version 2>&1";
        let libc = match docker(["exec", &name, "sh", "-c", probe]).await {
            Ok(out) => crate::compat::Libc::parse(&out),
            // `ldd --version` exits 1 on musl, with its output on stderr.
            Err(e) => crate::compat::Libc::parse(&format!("{e:#}")),
        };
        anyhow::Ok(Container { name: name.clone(), image: image.to_string(), libc, cgroup, exclusive, path })
    }
    .await;
    if inspected.is_err() {
        let _ = docker(["rm", "-f", &name]).await;
    }
    inspected
}

/// Tag for an image built from `dockerfile` and `context` (as the tests see
/// them): a hash of their contents, so a changed file means a new build.
fn build_tag(s: &Session, dockerfile: &Path, context: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let df = s.host_path(dockerfile);
    let ctx = s.host_path(context);
    let mut h = Sha256::new();
    h.update(std::fs::read(&df).with_context(|| format!("reading {}", dockerfile.display()))?);
    let mut files = Vec::new();
    let mut dirs = vec![ctx.clone()];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let e = e?;
            if e.file_type()?.is_dir() {
                dirs.push(e.path());
            } else {
                files.push(e.path());
            }
        }
    }
    files.sort();
    for f in files {
        h.update(f.strip_prefix(&ctx).unwrap_or(&f).as_os_str().as_encoded_bytes());
        h.update(std::fs::read(&f).unwrap_or_default());
    }
    let name = dockerfile.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let repo: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    Ok(format!("xtest/{}:{}", if repo.is_empty() { "image" } else { &repo }, &hex::encode(h.finalize())[..16]))
}

/// Starts a container. Returns the lease, a debug hint, and the host port of
/// each published container port.
pub(crate) async fn docker_setup(
    s: &Session,
    id: u64,
    fixture: &str,
    role: Role,
    d: DockerSpec,
    exclusive: bool,
) -> Result<(Lease, Option<String>, BTreeMap<u16, u16>)> {
    if !s.docker {
        bail!("fixture `{fixture}` needs docker, which is not available on this host");
    }
    let image = ensure_image(s, &d.image).await?;

    if role == Role::Environment {
        let c = start_environment(s, id, fixture, &image, &d.env, exclusive).await?;
        if let Some(r) = &d.ready {
            // Ports aren't published for environments; only `cmd` applies.
            let r = Ready { tcp: None, ..r.clone() };
            if let Err(e) = wait_ready(&c.name, &r, &BTreeMap::new()).await {
                let _ = docker(["rm", "-f", &c.name]).await;
                return Err(e);
            }
        }
        let hint = format!("docker exec -it {} sh", c.name);
        let lease = Lease { container: Some(c.clone()), ..Lease::new(LeaseKind::Container(c.name)) };
        return Ok((lease, Some(hint), BTreeMap::new()));
    }

    let name = format!("xtest-{}-{id}", s.run_id);
    let mut args: Vec<String> = vec!["run".into(), "-d".into(), "--name".into(), name.clone()];
    args.extend(labels(s, fixture));
    for (k, v) in &d.env {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    let mut ports = d.ports.clone();
    ports.extend(d.ready.as_ref().and_then(|r| r.tcp));
    ports.sort();
    ports.dedup();
    for p in &ports {
        args.extend(["-p".into(), format!("127.0.0.1::{p}")]);
    }
    args.push(image);
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
        anyhow::Ok((env, mapped))
    }
    .await;
    match started {
        Ok((env, mapped)) => {
            let hint = format!("docker logs {name}");
            let memory = container_cgroup(&name).await.map(|cg| memory::FixtureMemory::watch(vec![cg]));
            Ok((Lease { env, memory, ..Lease::new(LeaseKind::Container(name)) }, Some(hint), mapped))
        }
        Err(e) => {
            let _ = docker(["rm", "-f", &name]).await;
            Err(e)
        }
    }
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
    Ok(Lease { env: parsed.env, ..Lease::new(LeaseKind::Command { teardown, cwd: c.cwd }) })
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

    #[test]
    fn fills_k3d_templates() {
        let values = BTreeMap::from([
            ("cluster".to_string(), "xt1-r".to_string()),
            ("server-ip".to_string(), "172.18.0.2".to_string()),
            ("server:0".to_string(), "172.18.0.2".to_string()),
        ]);
        let vars = Vars { values, nodes_error: None };
        assert_eq!(fill("http://{server-ip}:30080/{cluster}", &vars).unwrap(), "http://172.18.0.2:30080/xt1-r");
        assert_eq!(fill("${HOME}/{x} {", &vars).unwrap(), "${HOME}/{x} {");
        // Literal braces that only look a bit like templates stay as they are.
        assert_eq!(fill("{server: x} {agent:} {agent:a}", &vars).unwrap(), "{server: x} {agent:} {agent:a}");
        assert!(fill("{agent:0}", &vars).unwrap_err().to_string().contains("no such node"));

        let vars = Vars { values: BTreeMap::new(), nodes_error: Some("lookup failed".into()) };
        assert!(fill("{server-ip}", &vars).unwrap_err().to_string().contains("lookup failed"));
        assert_eq!(fill("no templates", &vars).unwrap(), "no templates");
    }
}
