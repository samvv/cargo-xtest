//! The agent that runs on each test host. It is started by the controller
//! (over SSH, or locally) and speaks the `xtest-proto` protocol on stdio.

mod fixtures;
pub mod gc;
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
        Some("serve") => {
            let mut root = default_root();
            let mut rest = args[1..].iter();
            while let Some(a) = rest.next() {
                match a.as_str() {
                    "--root" => root = rest.next().expect("--root needs a value").into(),
                    "--stdio" => {}
                    other => die(&format!("unknown argument {other}")),
                }
            }
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            match rt.block_on(session::serve(root)) {
                Ok(()) => std::process::exit(0),
                Err(e) => die(&format!("{e:#}")),
            }
        }
        Some("--version") => {
            println!("xtest-agent {} (protocol {})", env!("CARGO_PKG_VERSION"), xtest_proto::VERSION);
            std::process::exit(0)
        }
        _ => die("usage: xtest-agent serve [--stdio] [--root DIR]"),
    }
}

fn die(msg: &str) -> ! {
    eprintln!("xtest-agent: {msg}");
    std::process::exit(2)
}
