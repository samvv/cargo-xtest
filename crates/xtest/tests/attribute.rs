// Tests written with #[xtest::test], run by plain libtest.

mod private {
    fn secret() -> u32 {
        42
    }

    mod tests {
        use super::*;

        #[xtest::test]
        fn sees_private_items() {
            assert_eq!(secret(), 42);
        }
    }
}

#[xtest::test]
fn passes() {}

#[xtest::test]
fn returns_ok() -> Result<(), String> {
    Ok(())
}

#[xtest::test]
#[should_panic(expected = "boom")]
fn should_panic_still_works() {
    panic!("boom");
}

#[xtest::test]
#[ignore = "checks that #[ignore] is kept"]
fn ignore_still_works() {
    panic!("ignored tests don't run");
}

#[xtest::test]
fn early_return_and_question_mark() -> Result<(), std::num::ParseIntError> {
    let n: u32 = "7".parse()?;
    if n == 7 {
        return Ok(());
    }
    unreachable!()
}

/// Lists this binary's tests, as cargo-xtest does during discovery, and checks
/// that each #[xtest::test] keeps its own name (and its #[ignore]).
#[test]
fn listed_under_their_own_names() {
    let exe = std::env::current_exe().unwrap();
    let list = |extra: &[&str]| {
        let out = std::process::Command::new(&exe)
            .args(["--list", "--format", "terse"])
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success());
        let mut names: Vec<String> =
            String::from_utf8(out.stdout).unwrap().lines().filter_map(|l| l.strip_suffix(": test")).map(String::from).collect();
        names.sort();
        names
    };
    assert_eq!(
        list(&[]),
        [
            "early_return_and_question_mark",
            "ignore_still_works",
            "listed_under_their_own_names",
            "passes",
            "private::tests::sees_private_items",
            "returns_ok",
            "should_panic_still_works",
        ]
    );
    assert_eq!(list(&["--ignored"]), ["ignore_still_works"]);
}
