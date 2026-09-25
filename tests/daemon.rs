//! End to end: runs the real binary against the fixture files, on a real Unix
//! socket, and talks to it the way the web app will.

// clippy.toml exempts #[test] functions from these lints, but not the helpers
// they call, so this test crate opts out as a whole.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(15);

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root")
}

/// Polls until `done` is true, failing the test if that takes too long.
fn wait_until(mut done: impl FnMut() -> bool, what: &str) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < WAIT, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

/// A private scratch directory, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "simple-metrics-e2e-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn socket(&self) -> PathBuf {
        self.0.join("metrics.sock")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
    }
}

/// A running daemon, stopped when dropped.
struct Daemon {
    child: Child,
    scratch: Scratch,
}

impl Daemon {
    /// Starts a daemon sampling fast (every 100 ms) from the fixture files,
    /// with `extra` arguments after (so they can override those settings).
    fn start(extra: &[&str]) -> Self {
        Self::start_with_root(&fixture_root(), extra)
    }

    fn start_with_root(root: &Path, extra: &[&str]) -> Self {
        let scratch = Scratch::new();
        let child = Command::new(env!("CARGO_BIN_EXE_simple-metrics"))
            .arg("--socket")
            .arg(scratch.socket())
            .args(["--interval", "100ms", "--retention", "10s"])
            .arg("--root")
            .arg(root)
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the binary should start");
        let daemon = Self { child, scratch };
        wait_until(
            || UnixStream::connect(daemon.socket()).is_ok(),
            "the socket",
        );
        daemon
    }

    fn socket(&self) -> PathBuf {
        self.scratch.socket()
    }

    /// Waits until at least `count` records are held.
    fn wait_for_records(&self, count: u64) {
        wait_until(
            || self.ask(&json!({"op": "info"}))["len"].as_u64().unwrap() >= count,
            "records",
        );
    }

    /// Sends one request on a fresh connection and returns the reply.
    fn ask(&self, request: &Value) -> Value {
        Client::connect(&self.socket()).ask(&request.to_string())
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// One connection to the socket.
struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).expect("should connect");
        stream.set_read_timeout(Some(WAIT)).unwrap();
        Self {
            reader: BufReader::new(stream.try_clone().unwrap()),
            writer: stream,
        }
    }

    fn send(&mut self, line: &str) {
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad reply {line:?}: {e}"))
    }

    fn ask(&mut self, line: &str) -> Value {
        self.send(line);
        self.receive()
    }
}

/// Runs the binary to completion (for cases where it should refuse to start).
fn run_to_exit(args: &[&std::ffi::OsStr]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_simple-metrics"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn info_describes_the_collector() {
    let daemon = Daemon::start(&[]);
    let info = daemon.ask(&json!({"op": "info"}));
    assert_eq!(info["ok"], true);
    assert_eq!(info["v"], 1);
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(info["interval_ms"], 100);
    assert_eq!(info["capacity"], 100); // 10 s at 100 ms
    assert_eq!(info["metrics"], 15); // 5 fixed + 2 for each of 5 interfaces
}

#[test]
fn metrics_lists_the_default_metrics() {
    let daemon = Daemon::start(&[]);
    let reply = daemon.ask(&json!({"op": "metrics"}));
    let names: Vec<&str> = reply["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 15);
    assert_eq!(
        &names[..5],
        [
            "cpu_percent",
            "load1",
            "mem_used_bytes",
            "swap_used_bytes",
            "cpu_temp_celsius"
        ]
    );
    for interface in ["eth0", "wlan0", "wlan1", "br-ap", "wg0"] {
        assert!(
            names.contains(&format!("net_{interface}_rx_bytes_per_sec").as_str()),
            "{interface}"
        );
        assert!(
            names.contains(&format!("net_{interface}_tx_bytes_per_sec").as_str()),
            "{interface}"
        );
    }
}

#[test]
fn the_interface_setting_changes_the_metrics() {
    let daemon = Daemon::start(&["--interface", "eth0", "--interface", "wg0"]);
    let info = daemon.ask(&json!({"op": "info"}));
    assert_eq!(info["metrics"], 9);
    let names = daemon.ask(&json!({"op": "metrics"}));
    assert!(names.to_string().contains("net_wg0_tx_bytes_per_sec"));
    assert!(!names.to_string().contains("wlan1"));
}

#[test]
fn latest_reports_the_values_in_the_fixture_files() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(2);
    let latest = daemon.ask(&json!({"op": "latest"}));
    let values = &latest["values"];
    assert_eq!(values["load1"], 0.07);
    assert_eq!(values["cpu_temp_celsius"], 48.686);
    // Values are floats in the JSON, so compare them as floats.
    assert_eq!(values["mem_used_bytes"].as_f64(), Some(414_113_792.0)); // (3886840 - 3482432) KiB
    assert_eq!(values["swap_used_bytes"].as_f64(), Some(0.0));
    // The fixture files never change, so nothing moved between samples:
    // network rates are zero...
    assert_eq!(values["net_eth0_rx_bytes_per_sec"], 0.0);
    assert_eq!(values["net_wg0_tx_bytes_per_sec"], 0.0);
    // ...and CPU time didn't pass, so there is no CPU percentage to give.
    assert!(values["cpu_percent"].is_null());
    assert!(latest["timestamp"].as_u64().unwrap() > 1_704_067_200_000);
}

#[test]
fn read_returns_aligned_columns_oldest_first_with_a_gap_at_the_start() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(3);
    let reply = daemon.ask(&json!({"op": "read"}));
    let timestamps = reply["timestamps"].as_array().unwrap();
    assert!(timestamps.len() >= 3);
    let times: Vec<u64> = timestamps.iter().map(|t| t.as_u64().unwrap()).collect();
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "oldest first, strictly increasing"
    );

    let series = reply["series"].as_object().unwrap();
    assert_eq!(series.len(), 15);
    for (name, column) in series {
        assert_eq!(column.as_array().unwrap().len(), timestamps.len(), "{name}");
    }
    // The first record has no previous reading to take a rate from.
    let eth0 = series["net_eth0_rx_bytes_per_sec"].as_array().unwrap();
    assert!(eth0[0].is_null());
    assert_eq!(eth0[1], 0.0);
}

#[test]
fn only_the_configured_retention_is_kept() {
    // 500 ms at 100 ms is room for 5 records.
    let daemon = Daemon::start(&["--retention", "500ms"]);
    assert_eq!(daemon.ask(&json!({"op": "info"}))["capacity"], 5);
    daemon.wait_for_records(5);

    let first: Vec<u64> = timestamps(&daemon);
    assert_eq!(first.len(), 5);
    // Keep sampling: it stays at five and the window moves on.
    thread::sleep(Duration::from_millis(600));
    let later: Vec<u64> = timestamps(&daemon);
    assert_eq!(later.len(), 5);
    assert!(later[0] > first[0], "the oldest records were discarded");
    assert_eq!(daemon.ask(&json!({"op": "info"}))["len"], 5);
}

fn timestamps(daemon: &Daemon) -> Vec<u64> {
    daemon.ask(&json!({"op": "read"}))["timestamps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_u64().unwrap())
        .collect()
}

#[test]
fn one_connection_can_make_many_requests() {
    let daemon = Daemon::start(&[]);
    let mut client = Client::connect(&daemon.socket());
    for _ in 0..3 {
        assert_eq!(client.ask(r#"{"op":"info"}"#)["ok"], true);
        assert_eq!(client.ask(r#"{"op":"metrics"}"#)["ok"], true);
    }
}

#[test]
fn bad_requests_get_an_error_and_the_connection_survives() {
    let daemon = Daemon::start(&[]);
    let mut client = Client::connect(&daemon.socket());
    for (request, expected) in [
        ("this is not json", "not valid JSON"),
        ("[1,2]", "must be a JSON object"),
        (r#"{"nothing":1}"#, "no \"op\""),
        (r#"{"op":"delete_everything"}"#, "unknown op"),
        (r#"{"op":"info","v":99}"#, "unsupported protocol version 99"),
    ] {
        let reply = client.ask(request);
        assert_eq!(reply["ok"], false, "{request}");
        assert!(
            reply["error"].as_str().unwrap().contains(expected),
            "{request}: {reply}"
        );
    }
    assert_eq!(client.ask(r#"{"op":"info"}"#)["ok"], true);
}

#[test]
fn a_request_that_never_ends_is_refused_and_does_not_grow_memory_without_limit() {
    let daemon = Daemon::start(&[]);
    let mut stream = UnixStream::connect(daemon.socket()).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    // 200 KiB with no newline, well past the 64 KiB limit. The server answers
    // and hangs up part-way, so the later writes may fail: that's fine.
    let chunk = vec![b'x'; 8192];
    for _ in 0..25 {
        if stream.write_all(&chunk).is_err() {
            break;
        }
    }
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["ok"], false);
    assert!(reply["error"].as_str().unwrap().contains("too long"));
}

#[test]
fn many_clients_at_once_all_get_answers() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(3);
    let socket = daemon.socket();
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let socket = socket.clone();
            thread::spawn(move || {
                let mut client = Client::connect(&socket);
                for _ in 0..20 {
                    assert_eq!(client.ask(r#"{"op":"read"}"#)["ok"], true);
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
}

#[test]
fn too_many_connections_are_turned_away_politely() {
    let daemon = Daemon::start(&[]);
    // Fill every slot, checking each is really being served...
    let mut held: Vec<Client> = (0..16)
        .map(|_| {
            let mut client = Client::connect(&daemon.socket());
            assert_eq!(client.ask(r#"{"op":"info"}"#)["ok"], true);
            client
        })
        .collect();
    // ...so the next is refused, with a reason.
    let mut extra = Client::connect(&daemon.socket());
    let reply = extra.receive();
    assert_eq!(reply["ok"], false);
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("too many connections")
    );
    // Freeing one makes room again.
    held.pop();
    let start = Instant::now();
    loop {
        let mut client = Client::connect(&daemon.socket());
        let reply = client.ask(r#"{"op":"info"}"#);
        if reply["ok"] == true {
            break;
        }
        assert!(start.elapsed() < WAIT, "a freed slot was never reused");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_socket_is_not_open_to_everyone_by_default() {
    let daemon = Daemon::start(&[]);
    let mode = fs::metadata(daemon.socket()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o660);
}

#[test]
fn the_socket_mode_can_be_changed() {
    let daemon = Daemon::start(&["--socket-mode", "600"]);
    let mode = fs::metadata(daemon.socket()).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_missing_proc_tree_gives_gaps_not_a_crash() {
    let daemon = Daemon::start_with_root(Path::new("/this/does/not/exist"), &[]);
    daemon.wait_for_records(2);
    let latest = daemon.ask(&json!({"op": "latest"}));
    let values = latest["values"].as_object().unwrap();
    assert_eq!(values.len(), 15);
    assert!(values.values().all(Value::is_null), "{values:?}");
}

#[test]
fn a_leftover_socket_file_from_a_crashed_run_is_replaced() {
    let scratch = Scratch::new();
    drop(UnixListener::bind(scratch.socket()).unwrap()); // file stays, nobody listening
    let child = Command::new(env!("CARGO_BIN_EXE_simple-metrics"))
        .arg("--socket")
        .arg(scratch.socket())
        .arg("--root")
        .arg(fixture_root())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let daemon = Daemon { child, scratch };
    wait_until(
        || UnixStream::connect(daemon.socket()).is_ok(),
        "the socket",
    );
    assert_eq!(daemon.ask(&json!({"op": "info"}))["ok"], true);
}

#[test]
fn it_refuses_to_replace_a_file_that_is_not_a_socket() {
    let scratch = Scratch::new();
    let precious = scratch.0.join("precious.txt");
    fs::write(&precious, "keep me").unwrap();
    let output = run_to_exit(&["--socket".as_ref(), precious.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("is not a socket"), "{stderr}");
    assert_eq!(fs::read_to_string(&precious).unwrap(), "keep me");
}

#[test]
fn a_second_copy_will_not_take_over_a_live_socket() {
    let daemon = Daemon::start(&[]);
    let output = run_to_exit(&["--socket".as_ref(), daemon.socket().as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("already listening"), "{stderr}");
    // The first is untouched.
    assert_eq!(daemon.ask(&json!({"op": "info"}))["ok"], true);
}

#[test]
fn a_socket_in_a_missing_directory_is_a_clear_error() {
    let output = run_to_exit(&["--socket".as_ref(), "/this/does/not/exist/s.sock".as_ref()]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("cannot set up the socket"), "{stderr}");
}

#[test]
fn a_client_that_hangs_up_mid_conversation_does_not_disturb_the_daemon() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(3);
    for _ in 0..20 {
        let mut stream = UnixStream::connect(daemon.socket()).unwrap();
        stream.write_all(br#"{"op":"read"}"#).unwrap();
        stream.write_all(b"\n").unwrap();
        // Read a little of the response, then vanish.
        let mut byte = [0_u8; 16];
        stream.read_exact(&mut byte).ok();
        drop(stream);
    }
    assert_eq!(daemon.ask(&json!({"op": "info"}))["ok"], true);
}

// ---- ranges, subsets and downsampling ---------------------------------------

/// Every record's timestamp, asking for one cheap metric.
fn all_timestamps(daemon: &Daemon) -> Vec<u64> {
    let reply = daemon.ask(&json!({"op": "read", "metrics": ["load1"]}));
    reply["timestamps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_u64().unwrap())
        .collect()
}

#[test]
fn a_subset_of_metrics_returns_only_those() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(2);
    let reply = daemon.ask(&json!({"op": "read", "metrics": ["load1", "cpu_temp_celsius"]}));
    assert_eq!(reply["ok"], true);
    let series = reply["series"].as_object().unwrap();
    assert_eq!(series.len(), 2);
    assert_eq!(series["load1"][0], 0.07);
    assert_eq!(series["cpu_temp_celsius"][0], 48.686);
}

#[test]
fn an_unknown_metric_is_refused_by_name() {
    let daemon = Daemon::start(&[]);
    let reply = daemon.ask(&json!({"op": "read", "metrics": ["load1", "no_such_metric"]}));
    assert_eq!(reply["ok"], false);
    assert!(reply["error"].as_str().unwrap().contains("no_such_metric"));
}

#[test]
fn a_time_range_returns_only_records_inside_it() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(6);
    let all = all_timestamps(&daemon);
    let (from, to) = (all[2], all[4]);

    let reply = daemon.ask(&json!({"op": "read", "metrics": ["load1"], "from": from, "to": to}));
    let got: Vec<u64> = reply["timestamps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_u64().unwrap())
        .collect();
    // Both ends are included, and nothing outside them.
    assert_eq!(got, all[2..=4]);

    let later = daemon.ask(&json!({"op": "read", "from": u64::MAX - 1}));
    assert_eq!(later["ok"], true);
    assert_eq!(later["timestamps"], json!([]));
}

#[test]
fn max_points_limits_the_number_of_points_and_reports_the_step() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(20);
    let reply = daemon.ask(&json!({"op": "read", "metrics": ["load1"], "max_points": 4}));
    assert_eq!(reply["ok"], true, "{reply}");
    let step = reply["step_ms"].as_u64().unwrap();
    assert_eq!(step % 100, 0, "a whole number of the 100 ms samples");
    let points = reply["timestamps"].as_array().unwrap();
    assert!((1..=4).contains(&points.len()), "{} points", points.len());
    assert_eq!(
        reply["series"]["load1"].as_array().unwrap().len(),
        points.len()
    );
    for t in points {
        assert_eq!(
            t.as_u64().unwrap() % step,
            0,
            "buckets are aligned to the step"
        );
    }
}

#[test]
fn a_step_gives_one_value_per_bucket_and_extremes_bracket_the_mean() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(10);
    let reply = daemon.ask(&json!({
        "op": "read", "metrics": ["net_eth0_rx_bytes_per_sec"], "step_ms": 300, "extremes": true
    }));
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(reply["step_ms"], 300);
    let name = "net_eth0_rx_bytes_per_sec";
    let (avg, min, max) = (
        &reply["series"][name],
        &reply["min"][name],
        &reply["max"][name],
    );
    let buckets = reply["timestamps"].as_array().unwrap().len();
    assert!(buckets >= 2);
    for column in [avg, min, max] {
        assert_eq!(column.as_array().unwrap().len(), buckets);
    }
    for i in 0..buckets {
        // The fixture's counters never move, so every measured rate is zero
        // (the very first record has none, so its bucket may be empty).
        if let (Some(a), Some(lo), Some(hi)) = (avg[i].as_f64(), min[i].as_f64(), max[i].as_f64()) {
            assert!(lo <= a && a <= hi, "bucket {i}: {lo} <= {a} <= {hi}");
            assert!(a.abs() < f64::EPSILON, "{a}");
        }
    }
}

#[test]
fn a_downsampled_range_uses_the_same_bucket_edges_as_the_whole() {
    let daemon = Daemon::start(&[]);
    daemon.wait_for_records(10);
    let whole = daemon.ask(&json!({"op": "read", "metrics": ["load1"], "step_ms": 500}));
    let edges: Vec<u64> = whole["timestamps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_u64().unwrap())
        .collect();
    // Ask again from a time in the middle: its buckets are ones of the first.
    let from = edges[edges.len() / 2] + 137;
    let part =
        daemon.ask(&json!({"op": "read", "metrics": ["load1"], "step_ms": 500, "from": from}));
    for t in part["timestamps"].as_array().unwrap() {
        assert!(
            edges.contains(&t.as_u64().unwrap()),
            "{t} is not one of {edges:?}"
        );
    }
}

#[test]
fn bad_read_options_are_refused_and_the_connection_survives() {
    let daemon = Daemon::start(&[]);
    let mut client = Client::connect(&daemon.socket());
    for (request, expected) in [
        (r#"{"op":"read","from":5,"to":1}"#, "must not be after"),
        (r#"{"op":"read","from":-1}"#, "whole number"),
        (r#"{"op":"read","metrics":[]}"#, "must not be empty"),
        (r#"{"op":"read","step_ms":0}"#, "at least 1"),
        (r#"{"op":"read","max_points":1}"#, "between 2 and"),
        (r#"{"op":"read","step_ms":10,"max_points":10}"#, "together"),
        (r#"{"op":"read","extremes":true}"#, "needs"),
    ] {
        let reply = client.ask(request);
        assert_eq!(reply["ok"], false, "{request}");
        assert!(
            reply["error"].as_str().unwrap().contains(expected),
            "{request}: {reply}"
        );
    }
    assert_eq!(client.ask(r#"{"op":"info"}"#)["ok"], true);
}
