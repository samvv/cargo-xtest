//! Whether binaries built on the developer's machine can run somewhere else:
//! the glibc they need, and Docker's architecture names.

use std::fmt;
use std::path::Path;

/// A glibc version, e.g. 2.39.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Glibc(pub u32, pub u32);

impl Glibc {
    /// Parses `2.39`, `2.2.5`, or `getconf GNU_LIBC_VERSION` output (`glibc 2.39`).
    pub fn parse(s: &str) -> Option<Glibc> {
        let v = s.trim().trim_start_matches("glibc").trim();
        let mut parts = v.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()?;
        Some(Glibc(major, minor))
    }
}

impl fmt::Display for Glibc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.0, self.1)
    }
}

/// The glibc this process runs with, or `None` if it doesn't use glibc.
pub fn running_glibc() -> Option<Glibc> {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let v = unsafe { std::ffi::CStr::from_ptr(libc::gnu_get_libc_version()) };
        Glibc::parse(&v.to_string_lossy())
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        None
    }
}

/// The newest glibc an executable needs, from the `GLIBC_x.y` symbol
/// versions it references. `None` if it doesn't use glibc (static or musl).
pub fn required_glibc(path: &Path) -> std::io::Result<Option<Glibc>> {
    let bytes = std::fs::read(path)?;
    Ok(required_glibc_of(&bytes))
}

fn required_glibc_of(bytes: &[u8]) -> Option<Glibc> {
    // Symbol version names are NUL-terminated strings in the dynamic string table.
    bytes
        .split(|b| *b == 0)
        .filter_map(|s| s.strip_prefix(b"GLIBC_"))
        .filter_map(|v| Glibc::parse(std::str::from_utf8(v).ok()?))
        .max()
}

/// The C library of an image, as far as the agent can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Libc {
    Glibc(Glibc),
    Musl,
    Unknown,
}

impl Libc {
    /// From `getconf GNU_LIBC_VERSION` or `ldd --version` output.
    pub fn parse(out: &str) -> Libc {
        let first = out.lines().next().unwrap_or("");
        if out.to_lowercase().contains("musl") {
            Libc::Musl
        } else if let Some(g) = Glibc::parse(first).or_else(|| first.split_whitespace().last().and_then(Glibc::parse)) {
            Libc::Glibc(g)
        } else {
            Libc::Unknown
        }
    }
}

/// Docker's architecture name (`docker image inspect`) in Rust's naming
/// (`std::env::consts::ARCH`).
pub fn docker_arch(arch: &str) -> &str {
    match arch {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        "386" => "x86",
        "ppc64le" => "powerpc64",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(Glibc::parse("glibc 2.39"), Some(Glibc(2, 39)));
        assert_eq!(Glibc::parse("2.2.5"), Some(Glibc(2, 2)));
        assert_eq!(Glibc::parse("2.41.9000"), Some(Glibc(2, 41)));
        assert_eq!(Glibc::parse("PRIVATE"), None);
        assert!(Glibc(2, 4) < Glibc(2, 34));
    }

    #[test]
    fn recognizes_image_libcs() {
        assert_eq!(Libc::parse("glibc 2.36\n"), Libc::Glibc(Glibc(2, 36)));
        assert_eq!(Libc::parse("ldd (Debian GLIBC 2.36-9) 2.36\nCopyright"), Libc::Glibc(Glibc(2, 36)));
        assert_eq!(Libc::parse("musl libc (x86_64)\nVersion 1.2.5"), Libc::Musl);
        assert_eq!(Libc::parse("sh: getconf: not found"), Libc::Unknown);
    }

    #[test]
    fn finds_the_newest_symbol_version() {
        let bytes = b"\0libc.so.6\0GLIBC_2.2.5\0GLIBC_2.34\0GLIBC_PRIVATE\0GLIBC_2.17\0";
        assert_eq!(required_glibc_of(bytes), Some(Glibc(2, 34)));
        assert_eq!(required_glibc_of(b"\0static\0"), None);
        // This test binary itself, when built against glibc.
        if let Some(running) = running_glibc() {
            let needed = required_glibc(&std::env::current_exe().unwrap()).unwrap().unwrap();
            assert!(needed <= running);
        }
    }
}
