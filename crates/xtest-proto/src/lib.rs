//! Wire protocol between the `cargo-xtest` controller and `xtest-agent`.
//!
//! Every message is a frame: a little-endian `u32` length followed by a
//! postcard-encoded body. The controller sends [`ClientMsg`]s, the agent
//! answers with [`AgentMsg`]s. Requests carry an id (`req`); replies and
//! process events refer back to it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped whenever the messages below change incompatibly.
pub const VERSION: u32 = 6;

/// Largest frame we accept. Blob uploads are chunked well below this.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// Size of the chunks blobs are uploaded in.
pub const BLOB_CHUNK: usize = 1024 * 1024;

/// SHA-256 of a file's contents.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", &self.to_hex()[..12])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientMsg {
    pub req: u64,
    pub body: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
    },
    /// Which of these blobs is the store missing? Answered with [`Reply::Missing`].
    HaveBlobs {
        hashes: Vec<Hash>,
    },
    /// One chunk of a blob. The blob is verified and committed on `last`.
    PutBlob {
        hash: Hash,
        offset: u64,
        data: Vec<u8>,
        last: bool,
    },
    /// Build the run's sandbox tree from blobs in the store.
    Materialize {
        /// Absolute directories that tests must see at their original path.
        mirrors: Vec<PathBuf>,
        files: Vec<FileEntry>,
    },
    /// Run a process. Produces [`AgentMsg::Output`] events and one [`AgentMsg::Exited`].
    Spawn(SpawnSpec),
    /// Kill a process started with `Spawn` (by its request id).
    Kill {
        id: u64,
    },
    /// Set up a fixture. Answered with [`Reply::Lease`].
    Setup {
        lease: u64,
        name: String,
        role: Role,
        spec: FixtureSpec,
        /// Used by a single test only (test scope). Lets the agent measure
        /// a container's whole peak memory as that test's peak.
        exclusive: bool,
    },
    /// Current memory situation. Answered with [`Reply::Memory`].
    Memory,
    /// Answered with [`Reply::TornDown`].
    Teardown {
        lease: u64,
    },
    /// End the session: kill processes, tear down every lease except `keep`,
    /// and remove the run directory (unless something is kept).
    Shutdown {
        keep: Vec<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// Absolute path as seen by the tests (inside a mirror).
    pub path: PathBuf,
    pub hash: Hash,
    pub mode: u32,
    /// Hardlink from the store instead of copying (large, never-written files).
    pub link: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSpec {
    pub exe: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub timeout: Option<Duration>,
    /// Fixture leases this process uses: their env vars are added, and an
    /// environment lease decides where the process runs.
    pub leases: Vec<u64>,
}

/// How a fixture is used by a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// The test runs inside it.
    Environment,
    /// It runs next to the test and exports env vars.
    Service,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FixtureSpec {
    Docker(DockerSpec),
    Command(CommandSpec),
    K3d(K3dSpec),
}

/// A Kubernetes cluster made with `k3d cluster create`. Tests get its
/// kubeconfig in `KUBECONFIG` and a matching `kubectl` on `PATH`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K3dSpec {
    pub servers: u32,
    pub agents: u32,
    /// k3s image, e.g. `rancher/k3s:v1.31.4-k3s1`.
    pub image: Option<String>,
    /// Extra arguments for `k3d cluster create`.
    pub args: Vec<String>,
    /// How long `k3d cluster create --wait` may take.
    pub timeout: Duration,
    /// Run on the host until it succeeds. May use the templates in [`K3D_TEMPLATES`].
    pub ready: Option<Ready>,
    /// Env vars exported to tests. May use the templates in [`K3D_TEMPLATES`].
    pub export: BTreeMap<String, String>,
    /// Docker network shared by every k3d fixture of the run with the same
    /// name, so their nodes can reach each other (e.g. through NodePorts).
    pub network: Option<String>,
    /// Image of the container tests run in when the cluster is their
    /// environment (`run-in`).
    pub runner: Option<ImageSource>,
}

/// Templates k3d fixtures can use in `ready` and `export`.
pub const K3D_TEMPLATES: &[&str] =
    &["{cluster}", "{context}", "{kubeconfig}", "{network}", "{server-ip}", "{server:N}", "{agent:N}"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DockerSpec {
    pub image: ImageSource,
    /// Env vars passed to the container.
    pub env: BTreeMap<String, String>,
    /// Container ports to publish (services only).
    pub ports: Vec<u16>,
    pub ready: Option<Ready>,
    /// Env vars exported to tests. Values may use `{host}` and `{port:N}`.
    pub export: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ImageSource {
    Pull(String),
    Build {
        /// Absolute paths as the tests see them (inside a mirror).
        dockerfile: PathBuf,
        context: PathBuf,
        /// Tag derived from a hash of the Dockerfile and context.
        tag: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ready {
    /// Wait until this container port accepts TCP connections.
    pub tcp: Option<u16>,
    /// Wait until this command exits 0 inside the container.
    pub cmd: Option<Vec<String>>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSpec {
    /// Shell command run in the workspace root. Prints JSON:
    /// `{ "handle": "...", "env": { ... } }`.
    pub setup: String,
    /// Shell command; `{handle}` is replaced by the handle from setup.
    pub teardown: Option<String>,
    /// Directory the commands run in (as seen by tests).
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentMsg {
    Reply { req: u64, result: Result<Reply, String> },
    Output { req: u64, stream: Stream, data: Vec<u8> },
    Exited {
        req: u64,
        status: Exit,
        duration: Duration,
        /// Peak memory of the process (and of its container, if it ran in one), in bytes.
        peak_memory: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    Hello(HostInfo),
    Missing { hashes: Vec<Hash> },
    Ok,
    Lease {
        env: BTreeMap<String, String>,
        /// Shown to the user when a lease is kept for debugging.
        debug_hint: Option<String>,
    },
    Memory(MemoryInfo),
    TornDown {
        /// Peak memory of the fixture, if the agent could measure it.
        peak: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub version: u32,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub run_dir: PathBuf,
    /// Whether processes can see the sandbox at the original paths.
    pub mirroring: bool,
    pub docker: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInfo {
    /// `MemAvailable` from `/proc/meminfo`, in bytes.
    pub available: u64,
    pub total: u64,
    /// Memory each running process (by `Spawn` request id) uses right now, in bytes.
    pub running: BTreeMap<u64, u64>,
    /// Memory of each fixture (by lease id) that the agent can measure.
    pub fixtures: BTreeMap<u64, FixtureUsage>,
}

/// Memory of a fixture's own containers (not the tests in them), without
/// reclaimable file cache.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct FixtureUsage {
    pub current: u64,
    pub peak: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    Timeout,
    SpawnFailed(String),
}

impl Exit {
    pub fn success(&self) -> bool {
        matches!(self, Exit::Code(0))
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Code(c) => write!(f, "exit code {c}"),
            Exit::Signal(s) => write!(f, "killed by signal {s}"),
            Exit::Timeout => write!(f, "timed out"),
            Exit::SpawnFailed(e) => write!(f, "failed to start: {e}"),
        }
    }
}

pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = postcard::to_stdvec(msg)?;
    anyhow::ensure!(body.len() <= MAX_FRAME, "frame too large ({} bytes)", body.len());
    w.write_all(&(body.len() as u32).to_le_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame. Returns `None` on a clean end of stream.
pub async fn read_frame<R, T>(r: &mut R) -> anyhow::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_le_bytes(len) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "frame too large ({len} bytes)");
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(postcard::from_bytes(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_roundtrip() {
        let msg = ClientMsg {
            req: 7,
            body: Request::Spawn(SpawnSpec {
                exe: "/bin/true".into(),
                args: vec!["--exact".into(), "a::b".into()],
                cwd: "/".into(),
                env: [("K".to_string(), "V".to_string())].into(),
                timeout: Some(Duration::from_secs(3)),
                leases: vec![1, 2],
            }),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).await.unwrap();
        write_frame(&mut buf, &msg).await.unwrap();
        let mut r = &buf[..];
        for _ in 0..2 {
            let back: ClientMsg = read_frame(&mut r).await.unwrap().unwrap();
            assert_eq!(back.req, 7);
            assert!(matches!(back.body, Request::Spawn(s) if s.args[1] == "a::b"));
        }
        assert!(read_frame::<_, ClientMsg>(&mut r).await.unwrap().is_none());
    }
}
