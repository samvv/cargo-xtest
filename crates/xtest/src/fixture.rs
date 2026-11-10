//! Writing fixtures: the context a setup gets ([`Setup`]), the one a
//! teardown gets ([`Teardown`]), and the building blocks a setup can ask for.
//!
//! Everything a setup asks for through [`Setup`] (containers, clusters,
//! processes) is created and owned by cargo-xtest's agent. It is removed when
//! the fixture is torn down, also when a run crashes, and kept with
//! `--keep-fixtures`.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::net::UnixStream;
use xtest_proto::fx;

use crate::Result;

type ServeFn = Box<dyn FnOnce(Shutdown) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send>;

/// What a fixture's setup gets: asks cargo-xtest for containers, clusters
/// and processes, and sets env vars for the tests.
pub struct Setup {
    conn: UnixStream,
    pub(crate) serve: Option<ServeFn>,
    instance: String,
}

impl Setup {
    pub(crate) fn new(conn: UnixStream) -> Setup {
        let instance = std::env::var(fx::env::INSTANCE).unwrap_or_default();
        Setup { conn, serve: None, instance }
    }

    pub(crate) async fn call(&mut self, req: fx::Request) -> Result<fx::Reply> {
        xtest_proto::write_frame(&mut self.conn, &req).await.map_err(|e| e.to_string())?;
        match xtest_proto::read_frame(&mut self.conn).await.map_err(|e| e.to_string())? {
            Some(fx::Reply::Error(e)) => Err(e.into()),
            Some(r) => Ok(r),
            None => Err("cargo-xtest's agent closed the connection".into()),
        }
    }

    /// Starts a container next to the tests. Its ports are published on
    /// 127.0.0.1; see [`Container::addr`].
    pub async fn docker(&mut self, docker: Docker) -> Result<Container> {
        match self.call(fx::Request::Docker { spec: docker.spec, environment: false }).await? {
            fx::Reply::Container { name, ports } => Ok(Container { name, ports }),
            r => Err(unexpected(r)),
        }
    }

    /// Starts a container that the fixture's tests run in, with the
    /// workspace at its original paths. The fixture must be declared with
    /// `#[xtest::fixture(environment)]`.
    pub async fn docker_environment(&mut self, docker: Docker) -> Result<()> {
        match self.call(fx::Request::Docker { spec: docker.spec, environment: true }).await? {
            fx::Reply::Container { .. } => Ok(()),
            r => Err(unexpected(r)),
        }
    }

    /// Creates a k3d cluster. Tests get its kubeconfig in `KUBECONFIG` and a
    /// matching `kubectl` on `PATH`.
    pub async fn k3d(&mut self, k3d: K3d) -> Result<Cluster> {
        self.k3d_inner(k3d, false).await
    }

    /// Creates a k3d cluster whose runner container the fixture's tests run
    /// in (see [`K3d::runner`]). The fixture must be declared with
    /// `#[xtest::fixture(environment)]`.
    pub async fn k3d_environment(&mut self, k3d: K3d) -> Result<Cluster> {
        if k3d.spec.runner.is_none() {
            return Err("k3d_environment needs a runner image: K3d::new().runner(\"image\")".into());
        }
        self.k3d_inner(k3d, true).await
    }

    async fn k3d_inner(&mut self, k3d: K3d, environment: bool) -> Result<Cluster> {
        match self.call(fx::Request::K3d { spec: k3d.spec, environment }).await? {
            fx::Reply::Cluster { vars, bin } => Ok(Cluster { vars, bin }),
            r => Err(unexpected(r)),
        }
    }

    /// Starts a process that runs until the fixture is torn down.
    pub async fn spawn(&mut self, command: Command) -> Result<Process> {
        let cwd = match command.cwd {
            Some(d) => d,
            None => std::env::current_dir()?,
        };
        let req = fx::Request::Spawn { exe: command.exe, args: command.args, cwd, env: command.env };
        match self.call(req).await? {
            fx::Reply::Process { pid, log } => Ok(Process { pid, log }),
            r => Err(unexpected(r)),
        }
    }

    /// Sets an env var for the tests that use this fixture, e.g. for code
    /// that reads its configuration from the environment.
    pub async fn export(&mut self, key: impl Into<String>, value: impl Into<String>) -> Result<()> {
        self.call(fx::Request::Export { key: key.into(), value: value.into() }).await.map(drop)
    }

    /// Runs this command when the fixture is torn down, even if the run
    /// crashes (then during the next run's cleanup). Use it for things
    /// cargo-xtest can't know about, like a cloud resource. It runs on the
    /// host from `/`, so don't rely on the workspace's paths in it.
    pub async fn cleanup<I, S>(&mut self, argv: I) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let argv = argv.into_iter().map(Into::into).collect();
        self.call(fx::Request::Cleanup { argv }).await.map(drop)
    }

    /// Keeps running `serve` after setup, until the fixture is torn down;
    /// for Rust code that serves the tests, like a mock server. The
    /// [`Shutdown`] it gets completes when teardown starts; `serve` then has
    /// a few seconds to finish.
    pub fn serve<F, Fut>(&mut self, serve: F)
    where
        F: FnOnce(Shutdown) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.serve = Some(Box::new(move |shutdown| Box::pin(serve(shutdown))));
    }

    /// Unique per fixture instance, and only letters, digits and `-`: e.g.
    /// for naming a database per test.
    pub fn instance_id(&self) -> &str {
        &self.instance
    }
}

fn unexpected(r: fx::Reply) -> crate::Error {
    format!("unexpected reply from cargo-xtest's agent: {r:?}").into()
}

/// What a fixture's teardown gets.
pub struct Teardown {
    pub(crate) failed: bool,
    pub(crate) attach_dir: Option<PathBuf>,
}

impl Teardown {
    /// Whether a test that used the fixture failed.
    pub fn failed(&self) -> bool {
        self.failed
    }

    /// Attaches a file to the report: it is shown with the test's output
    /// (test scope) or at the end of the run.
    pub fn attach(&self, name: &str, data: impl AsRef<[u8]>) -> Result<()> {
        let Some(dir) = &self.attach_dir else { return Ok(()) };
        let name: String = name.chars().map(|c| if c == '/' || c == '\0' { '_' } else { c }).collect();
        std::fs::write(dir.join(name), data)?;
        Ok(())
    }
}

/// Completes when the fixture is being torn down. See [`Setup::serve`].
pub struct Shutdown(pub(crate) Pin<Box<dyn Future<Output = ()> + Send>>);

impl Future for Shutdown {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.0.as_mut().poll(cx)
    }
}

/// A path as the tests see it: relative paths are relative to the package
/// (`CARGO_MANIFEST_DIR`).
fn package_path(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_owned();
    }
    let base = std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from).or_else(|| std::env::current_dir().ok());
    base.unwrap_or_default().join(p)
}

/// When a container or cluster counts as ready.
#[derive(Debug, Clone)]
pub struct Ready(xtest_proto::Ready);

impl Ready {
    /// Ready once this container port accepts TCP connections (published
    /// automatically). Not for environments.
    pub fn tcp(port: u16) -> Ready {
        Ready(xtest_proto::Ready { tcp: Some(port), cmd: None, timeout: Duration::from_secs(30) })
    }

    /// Ready once this command exits 0: inside the container for
    /// [`Docker`], on the host (with `kubectl` on `PATH`) for [`K3d`].
    pub fn cmd<I, S>(argv: I) -> Ready
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let cmd = argv.into_iter().map(Into::into).collect();
        Ready(xtest_proto::Ready { tcp: None, cmd: Some(cmd), timeout: Duration::from_secs(30) })
    }

    /// Also wait for this command (after the TCP port, if any).
    pub fn and_cmd<I, S>(mut self, argv: I) -> Ready
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.0.cmd = Some(argv.into_iter().map(Into::into).collect());
        self
    }

    /// How long to wait (default: 30 seconds).
    pub fn timeout(mut self, timeout: Duration) -> Ready {
        self.0.timeout = timeout;
        self
    }
}

/// A Docker container to start; see [`Setup::docker`].
#[derive(Debug, Clone)]
pub struct Docker {
    spec: xtest_proto::DockerSpec,
}

impl Docker {
    /// A container from an image, pulled if the host doesn't have it.
    pub fn image(image: impl Into<String>) -> Docker {
        Docker::from_source(xtest_proto::ImageSource::Pull(image.into()))
    }

    /// A container from an image built from this Dockerfile (relative to the
    /// package), with its directory as the build context. The image is built
    /// once per host and content.
    pub fn build(dockerfile: impl AsRef<Path>) -> Docker {
        let dockerfile = package_path(dockerfile.as_ref());
        let context = dockerfile.parent().map(Path::to_owned).unwrap_or_default();
        Docker::from_source(xtest_proto::ImageSource::Build { dockerfile, context, tag: String::new() })
    }

    fn from_source(image: xtest_proto::ImageSource) -> Docker {
        Docker {
            spec: xtest_proto::DockerSpec { image, env: BTreeMap::new(), ports: vec![], ready: None, export: BTreeMap::new() },
        }
    }

    /// The build context, for [`Docker::build`] (default: the Dockerfile's directory).
    pub fn context(mut self, dir: impl AsRef<Path>) -> Docker {
        if let xtest_proto::ImageSource::Build { context, .. } = &mut self.spec.image {
            *context = package_path(dir.as_ref());
        }
        self
    }

    /// An env var for the container.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Docker {
        self.spec.env.insert(key.into(), value.into());
        self
    }

    /// Publishes a container port on 127.0.0.1 (not for environments).
    pub fn port(mut self, port: u16) -> Docker {
        self.spec.ports.push(port);
        self
    }

    pub fn ready(mut self, ready: Ready) -> Docker {
        self.spec.ready = Some(ready.0);
        self
    }
}

/// A running container; see [`Setup::docker`].
#[derive(Debug, Clone)]
pub struct Container {
    name: String,
    ports: BTreeMap<u16, u16>,
}

impl Container {
    /// The container's name, e.g. for `docker exec`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The host port a published container port is reachable on.
    ///
    /// # Panics
    ///
    /// If `port` wasn't published with [`Docker::port`] or [`Ready::tcp`].
    pub fn host_port(&self, port: u16) -> u16 {
        match self.ports.get(&port) {
            Some(p) => *p,
            None => panic!("container port {port} isn't published; add `.port({port})` to the Docker builder"),
        }
    }

    /// The address a published container port is reachable on.
    pub fn addr(&self, port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.host_port(port)))
    }
}

/// A k3d cluster to create; see [`Setup::k3d`].
#[derive(Debug, Clone)]
pub struct K3d {
    spec: xtest_proto::K3dSpec,
}

impl Default for K3d {
    fn default() -> Self {
        K3d::new()
    }
}

impl K3d {
    /// One server node, no agents.
    pub fn new() -> K3d {
        K3d {
            spec: xtest_proto::K3dSpec {
                servers: 1,
                agents: 0,
                image: None,
                args: vec![],
                timeout: Duration::from_secs(300),
                ready: None,
                export: BTreeMap::new(),
                network: None,
                runner: None,
            },
        }
    }

    pub fn servers(mut self, n: u32) -> K3d {
        self.spec.servers = n.max(1);
        self
    }

    pub fn agents(mut self, n: u32) -> K3d {
        self.spec.agents = n;
        self
    }

    /// The k3s image, e.g. `rancher/k3s:v1.31.4-k3s1`.
    pub fn image(mut self, image: impl Into<String>) -> K3d {
        self.spec.image = Some(image.into());
        self
    }

    /// An extra argument for `k3d cluster create`.
    pub fn arg(mut self, arg: impl Into<String>) -> K3d {
        self.spec.args.push(arg.into());
        self
    }

    /// How long creating the cluster may take (default: 5 minutes).
    pub fn timeout(mut self, timeout: Duration) -> K3d {
        self.spec.timeout = timeout;
        self
    }

    /// Joins a Docker network shared with every other cluster of the run
    /// that names the same one, so their nodes can reach each other.
    pub fn network(mut self, name: impl Into<String>) -> K3d {
        self.spec.network = Some(name.into());
        self
    }

    /// The image of the container tests run in, for [`Setup::k3d_environment`].
    pub fn runner(mut self, image: impl Into<String>) -> K3d {
        self.spec.runner = Some(xtest_proto::ImageSource::Pull(image.into()));
        self
    }

    /// Wait for this command on the host (with `kubectl` on `PATH`) before
    /// the cluster counts as ready.
    pub fn ready(mut self, ready: Ready) -> K3d {
        self.spec.ready = Some(xtest_proto::Ready { tcp: None, ..ready.0 });
        self
    }
}

/// A k3d cluster; see [`Setup::k3d`].
#[derive(Debug, Clone)]
pub struct Cluster {
    vars: BTreeMap<String, String>,
    bin: PathBuf,
}

impl Cluster {
    fn var(&self, key: &str) -> &str {
        self.vars.get(key).map(String::as_str).unwrap_or_default()
    }

    /// The cluster's k3d name.
    pub fn name(&self) -> &str {
        self.var("cluster")
    }

    /// The cluster's kubeconfig context.
    pub fn context(&self) -> &str {
        self.var("context")
    }

    pub fn kubeconfig(&self) -> PathBuf {
        PathBuf::from(self.var("kubeconfig"))
    }

    /// The Docker network its nodes are on.
    pub fn network(&self) -> &str {
        self.var("network")
    }

    /// A `kubectl` matching the cluster.
    pub fn kubectl(&self) -> PathBuf {
        self.bin.join("kubectl")
    }

    /// IPv4 address of server node `n` on the cluster's network.
    pub fn server_ip(&self, n: usize) -> Option<&str> {
        self.vars.get(&format!("server:{n}")).map(String::as_str)
    }

    /// IPv4 address of agent node `n` on the cluster's network.
    pub fn agent_ip(&self, n: usize) -> Option<&str> {
        self.vars.get(&format!("agent:{n}")).map(String::as_str)
    }
}

/// A process to start; see [`Setup::spawn`].
#[derive(Debug, Clone)]
pub struct Command {
    exe: PathBuf,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

impl Command {
    pub fn new(exe: impl AsRef<Path>) -> Command {
        Command { exe: exe.as_ref().to_owned(), args: vec![], env: BTreeMap::new(), cwd: None }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Command {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Command {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Where it runs (default: where the setup runs, the package directory).
    pub fn current_dir(mut self, dir: impl AsRef<Path>) -> Command {
        self.cwd = Some(dir.as_ref().to_owned());
        self
    }
}

/// A process started with [`Setup::spawn`].
#[derive(Debug, Clone)]
pub struct Process {
    pid: u32,
    log: PathBuf,
}

impl Process {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The file its stdout and stderr go to.
    pub fn log(&self) -> &Path {
        &self.log
    }
}

/// Waits until `addr` accepts TCP connections.
pub async fn wait_for_tcp(addr: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::net::TcpStream::connect(addr).await {
            Ok(_) => return Ok(()),
            Err(e) if tokio::time::Instant::now() >= deadline => {
                return Err(format!("{addr} not reachable after {timeout:?}: {e}").into());
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}
