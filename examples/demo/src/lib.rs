pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 2), 4);
    }

    #[test]
    fn reads_fixture_via_manifest_dir() {
        // A compile-time absolute path: only works if the host mirrors it.
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hello.txt");
        assert_eq!(std::fs::read_to_string(p).unwrap().trim(), "hello");
    }

    #[test]
    fn has_private_tmpdir() {
        let tmp = std::env::temp_dir();
        assert!(tmp.to_string_lossy().contains("cargo-xtest"), "TMPDIR = {}", tmp.display());
        std::fs::write(tmp.join("scratch"), "x").unwrap();
    }

    #[test]
    #[ignore = "only runs with --include-ignored"]
    fn slow_one() {}
}
