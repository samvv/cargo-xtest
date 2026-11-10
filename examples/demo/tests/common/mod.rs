// Fixtures written in Rust, shared by every test file that says `mod common;`.
// A run-scoped fixture here is set up once per run and host, however many
// test binaries include this file.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};

use serde::{Deserialize, Serialize};
use xtest::fixture::{Command, Docker, Ready, Setup, Teardown};

/// A Redis server for the whole run.
#[derive(Serialize, Deserialize)]
pub struct Redis {
    pub addr: SocketAddr,
}

#[xtest::fixture(scope = "run", memory = "64M")]
pub async fn redis(cx: &mut Setup) -> xtest::Result<Redis> {
    let c = cx
        .docker(Docker::image("redis:7-alpine").port(6379).ready(Ready::tcp(6379).and_cmd(["redis-cli", "ping"])))
        .await?;
    // For code that reads its configuration from the environment.
    cx.export("REDIS_ADDR", c.addr(6379).to_string()).await?;
    Ok(Redis { addr: c.addr(6379) })
}

/// Sends one command, returns the reply's first line.
pub fn redis_cmd(addr: SocketAddr, args: &[&str]) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    let mut req = format!("*{}\r\n", args.len());
    for a in args {
        req += &format!("${}\r\n{a}\r\n", a.len());
    }
    s.write_all(req.as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    line.trim_end().to_string()
}

/// A key prefix of its own for each test, on the shared server.
#[derive(Serialize, Deserialize)]
pub struct Keyspace {
    pub addr: SocketAddr,
    pub prefix: String,
}

impl Keyspace {
    pub fn key(&self, k: &str) -> String {
        format!("{}:{k}", self.prefix)
    }
}

#[xtest::fixture]
pub async fn keyspace(cx: &mut Setup, redis: &Redis) -> xtest::Result<Keyspace> {
    let prefix = format!("test-{}", cx.instance_id());
    redis_cmd(redis.addr, &["SET", &format!("{prefix}:created"), "1"]);
    Ok(Keyspace { addr: redis.addr, prefix })
}

/// Removes the test's keys. If the test failed, attaches them first.
#[xtest::teardown(keyspace)]
pub fn drop_keyspace(cx: &mut Teardown, ks: Keyspace) -> xtest::Result<()> {
    let keys = redis_cmd(ks.addr, &["EVAL", "return #redis.call('keys', ARGV[1])", "0", &format!("{}:*", ks.prefix)]);
    if cx.failed() {
        cx.attach("keys.txt", format!("{} keys under {}: {keys}\n", ks.prefix, ks.addr))?;
    }
    let script = "for _,k in ipairs(redis.call('keys', ARGV[1])) do redis.call('del', k) end";
    redis_cmd(ks.addr, &["EVAL", script, "0", &format!("{}:*", ks.prefix)]);
    Ok(())
}

/// An echo server written in Rust, running for as long as the test binary
/// needs it.
#[derive(Serialize, Deserialize)]
pub struct Echo {
    pub addr: SocketAddr,
}

#[xtest::fixture(scope = "binary")]
pub async fn echo(cx: &mut Setup) -> xtest::Result<Echo> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    cx.serve(move |shutdown| async move {
        tokio::select! {
            _ = shutdown => {}
            _ = async {
                loop {
                    let Ok((mut conn, _)) = listener.accept().await else { break };
                    tokio::spawn(async move {
                        let (mut r, mut w) = conn.split();
                        let _ = tokio::io::copy(&mut r, &mut w).await;
                    });
                }
            } => {}
        }
        Ok(())
    });
    Ok(Echo { addr })
}

pub fn echo_roundtrip(addr: SocketAddr, msg: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(msg.as_bytes()).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

/// A process cargo-xtest starts and stops, like the `greeter` command
/// fixture in .config/xtest.toml, plus a cleanup command.
#[derive(Serialize, Deserialize)]
pub struct Greeter {
    pub state: std::path::PathBuf,
}

#[xtest::fixture]
pub async fn greeter(cx: &mut Setup) -> xtest::Result<Greeter> {
    let state = std::env::temp_dir().join("greeting");
    // Outside the run's directories, so only the cleanup command removes it.
    let marker = format!("/tmp/xtest-demo-greeter-{}", cx.instance_id());
    let script = format!("touch {marker}; echo hi > {}; exec sleep 1000", state.display());
    cx.spawn(Command::new("sh").args(["-c", &script])).await?;
    for _ in 0..100 {
        if state.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Runs at teardown, or after a crash during the next run's cleanup.
    cx.cleanup(["rm", "-f", &marker]).await?;
    cx.export("GREETER_STATE", state.display().to_string()).await?;
    Ok(Greeter { state })
}

/// A container with borg installed that tests run inside.
#[derive(Serialize, Deserialize)]
pub struct Borg;

#[xtest::fixture(environment, scope = "binary")]
pub async fn borg(cx: &mut Setup) -> xtest::Result<Borg> {
    cx.docker_environment(Docker::build("tests/docker/borg.Dockerfile")).await?;
    Ok(Borg)
}
