//! Runs the real binary, to check what `cli.rs`'s unit tests can't: that
//! `main` is wired up and the exit codes reach the process.

// clippy.toml exempts #[test] functions from the expect/unwrap lints, but not
// the helpers they call, so this test crate opts out as a whole.
#![allow(clippy::expect_used)]

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_simple-metrics"))
        .args(args)
        .output()
        .expect("the binary should start")
}

#[test]
fn version_flag_prints_the_version_and_exits_zero() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        stdout,
        format!("simple-metrics {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn an_unknown_argument_exits_with_the_usage_code() {
    let output = run(&["--bogus"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
