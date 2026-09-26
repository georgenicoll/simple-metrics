//! Runs the real `smq` against the real daemon on a real socket, to check
//! the pieces fit: the request it builds is one the daemon accepts, and the
//! exit codes reach the process.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Daemon {
    fn start(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("smq-e2e-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root");
        let child = Command::new(env!("CARGO_BIN_EXE_simple-metrics"))
            .arg("--socket")
            .arg(dir.join("m.sock"))
            .args(["--interval", "100ms", "--retention", "10s", "--root"])
            .arg(root)
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = Self { child, dir };
        // Wait for at least a few records, so a read has something to show.
        let start = Instant::now();
        loop {
            let out = daemon.smq(&["info"]);
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success()
                && !text.contains("len          0")
                && !text.contains("len          1\n")
            {
                return daemon;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "daemon never had data"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn smq(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_smq"))
            .arg("--socket")
            .arg(self.dir.join("m.sock"))
            .args(args)
            .env_remove("SIMPLE_METRICS_SOCKET")
            .output()
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        fs::remove_dir_all(&self.dir).ok();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[test]
fn every_command_works_against_the_real_daemon() {
    let daemon = Daemon::start("commands");

    let info = daemon.smq(&["info"]);
    assert!(info.status.success());
    assert!(text(&info.stdout).contains("interval_ms  100"));

    let metrics = daemon.smq(&["metrics"]);
    assert!(text(&metrics.stdout).contains("cpu_percent"));

    let latest = daemon.smq(&["latest"]);
    assert!(latest.status.success());
    assert!(
        text(&latest.stdout).starts_with("at 20"),
        "{}",
        text(&latest.stdout)
    );

    let read = daemon.smq(&["read", "--metric", "load1", "--from", "1h"]);
    assert!(read.status.success(), "{}", text(&read.stderr));
    let out = text(&read.stdout);
    assert!(out.starts_with("time"), "{out}");
    assert!(out.contains("load1"), "{out}");
    assert!(
        !out.contains("cpu_percent"),
        "only the metric asked for: {out}"
    );

    let summarised = daemon.smq(&["read", "--metric", "load1", "--points", "5", "--extremes"]);
    assert!(summarised.status.success(), "{}", text(&summarised.stderr));
    assert!(text(&summarised.stdout).contains("load1 (max)"));
}

#[test]
fn json_output_is_the_daemons_reply() {
    let daemon = Daemon::start("json");
    let out = daemon.smq(&["--json", "info"]);
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["ok"], true);
    assert_eq!(value["metrics"], 15);
}

#[test]
fn the_daemons_refusals_become_a_failed_exit() {
    let daemon = Daemon::start("refusal");
    let out = daemon.smq(&["read", "--metric", "no_such_metric"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("no_such_metric"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_missing_socket_fails_with_a_hint() {
    let out = Command::new(env!("CARGO_BIN_EXE_smq"))
        .args(["--socket", "/nonexistent/x.sock", "info"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("--local"));
}

#[test]
fn the_socket_can_come_from_the_environment() {
    let daemon = Daemon::start("env");
    let out = Command::new(env!("CARGO_BIN_EXE_smq"))
        .env("SIMPLE_METRICS_SOCKET", daemon.dir.join("m.sock"))
        .arg("info")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
}
