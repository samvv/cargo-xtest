//! The agent that runs on each test host. It is started by the controller
//! (over SSH, or locally) and speaks the `xtest-proto` protocol on stdio.

pub mod compat;
mod fixtures;
pub mod gc;
pub mod memory;
mod rust_fixture;
pub mod sandbox;
mod session;
pub mod store;

use std::path::PathBuf;

/// Default location of the agent's data: `$XDG_CACHE_HOME/cargo-xtest`.
pub fn default_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("cargo-xtest");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".cache/cargo-xtest")
}

/// Entry point of the `xtest-agent` binary.
pub fn main() -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("sandbox-exec") => sandbox::main(&args[1..]),
        Some(cmd @ ("serve" | "gc")) => {
            let mut root = default_root();
            let mut store_size = gc::DEFAULT_STORE_SIZE;
            let mut everything = false;
            let mut rest = args[1..].iter();
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--root" => root = rest.next().unwrap_or_else(|| die("--root needs a value")).into(),
                    "--store-size" => {
                        let v = rest.next().unwrap_or_else(|| die("--store-size needs a value"));
                        store_size = v.parse().unwrap_or_else(|_| die("--store-size takes a number of bytes"));
                    }
                    "--stdio" if cmd == "serve" => {}
                    "--all" if cmd == "gc" => everything = true,
                    other => die(&format!("unknown argument {other}")),
                }
            }
            if cmd == "gc" {
                std::process::exit(match gc_main(&root, store_size, everything) {
                    Ok(()) => 0,
                    Err(e) => die(&format!("{e:#}")),
                });
            }
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            match rt.block_on(session::serve(root, store_size)) {
                Ok(()) => std::process::exit(0),
                Err(e) => die(&format!("{e:#}")),
            }
        }
        Some("--version") => {
            println!("xtest-agent {} (protocol {})", env!("CARGO_PKG_VERSION"), xtest_proto::VERSION);
            std::process::exit(0)
        }
        _ => die("usage: xtest-agent serve [--stdio] [--root DIR] [--store-size BYTES]\n       xtest-agent gc [--root DIR] [--store-size BYTES] [--all]"),
    }
}

/// `xtest-agent gc`: collects garbage now and prints what was removed.
/// With `all`, removes everything that no live run uses, kept runs included.
fn gc_main(root: &std::path::Path, store_size: u64, all: bool) -> anyhow::Result<()> {
    if !root.exists() {
        println!("nothing to clean: {} does not exist", root.display());
        return Ok(());
    }
    let root = root.canonicalize()?;
    let docker = std::process::Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let policy = if all { gc::Policy::everything() } else { gc::Policy::startup(store_size) };
    println!("{}", gc::collect(&root, docker, &policy));
    Ok(())
}

fn die(msg: &str) -> ! {
    eprintln!("xtest-agent: {msg}");
    std::process::exit(2)
}
