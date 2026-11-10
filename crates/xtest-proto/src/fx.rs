//! Protocol between a Rust fixture's setup process and the agent.
//!
//! The agent starts the fixture's hidden setup test with `XTEST_SOCKET` set
//! to a Unix socket. The fixture connects once and sends [`Request`]s, one at
//! a time, in the same frames as the main protocol ([`write_frame`](crate::write_frame)).
//! Everything it asks for is created and owned by the agent, so the agent can
//! remove it even if the fixture process dies. Setup ends with
//! [`Request::Ready`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{DockerSpec, K3dSpec};

/// Env vars the agent sets for fixture and test processes.
pub mod env {
    /// Set to `1` for every process cargo-xtest starts.
    pub const XTEST: &str = "XTEST";
    /// `setup` or `teardown`, for a fixture's hidden tests.
    pub const MODE: &str = "XTEST_MODE";
    /// The socket a fixture's setup talks to the agent on.
    pub const SOCKET: &str = "XTEST_SOCKET";
    /// JSON file: `{ "<fixture id>": <value>, ... }` for the fixtures a
    /// process uses (a test, or a fixture's dependencies).
    pub const FIXTURES: &str = "XTEST_FIXTURES";
    /// Unique per fixture instance, e.g. for naming databases.
    pub const INSTANCE: &str = "XTEST_INSTANCE";
    /// For teardown: JSON file with the fixture's value.
    pub const VALUE: &str = "XTEST_VALUE";
    /// For teardown: `1` if a test that used the fixture failed.
    pub const FAILED: &str = "XTEST_FAILED";
    /// For teardown: directory for files the teardown attaches.
    pub const ATTACH_DIR: &str = "XTEST_ATTACH_DIR";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// Start a container. `export` in the spec is ignored. A container that
    /// is an `environment` is where the fixture's tests run; it publishes no
    /// ports. Answered with [`Reply::Container`].
    Docker { spec: DockerSpec, environment: bool },
    /// Create a k3d cluster; as an `environment`, tests run in its runner
    /// container. `export` in the spec is ignored. Answered with
    /// [`Reply::Cluster`].
    K3d { spec: K3dSpec, environment: bool },
    /// Start a process that runs until the fixture is torn down. Answered
    /// with [`Reply::Process`].
    Spawn { exe: PathBuf, args: Vec<String>, cwd: PathBuf, env: BTreeMap<String, String> },
    /// Set an env var for the tests that use the fixture.
    Export { key: String, value: String },
    /// Run this command when the fixture is torn down, even if the run
    /// crashed (then by the next run's cleanup).
    Cleanup { argv: Vec<String> },
    /// Setup is done. `value` is the fixture's value as JSON. When
    /// `serving`, the process keeps running until teardown; otherwise it
    /// exits right after this.
    Ready { value: String, serving: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    Ok,
    Container {
        name: String,
        /// Container port to host port, on 127.0.0.1.
        ports: BTreeMap<u16, u16>,
    },
    Cluster {
        /// The values k3d templates have: `cluster`, `context`,
        /// `kubeconfig`, `network`, `server-ip`, `server:N`, `agent:N`.
        vars: BTreeMap<String, String>,
        /// Directory with a `kubectl` matching the cluster.
        bin: PathBuf,
    },
    Process {
        pid: u32,
        /// File the process's stdout and stderr go to.
        log: PathBuf,
    },
    Error(String),
}
