//! Runs the real `smweb` against the real daemon, over real sockets, the way
//! a browser (or a reverse proxy) will use it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Servers {
    daemon: Child,
    web: Child,
    dir: PathBuf,
    port: u16,
}

impl Servers {
    fn web_socket(&self) -> PathBuf {
        self.dir.join("web.sock")
    }

    /// Sends a request and returns the whole response as text.
    fn get_tcp(&self, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }
}

impl Drop for Servers {
    fn drop(&mut self) {
        self.web.kill().ok();
        self.daemon.kill().ok();
        self.web.wait().ok();
        self.daemon.wait().ok();
        fs::remove_dir_all(&self.dir).ok();
    }
}

fn start() -> Servers {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "smweb-e2e-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&dir).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/root");
    let daemon = Command::new(env!("CARGO_BIN_EXE_simple-metrics"))
        .arg("--socket")
        .arg(dir.join("m.sock"))
        .args(["--interval", "100ms", "--retention", "10s", "--root"])
        .arg(root)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // A free port: bind to 0, note it, let go.
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let web = Command::new(env!("CARGO_BIN_EXE_smweb"))
        .arg("--socket")
        .arg(dir.join("m.sock"))
        .args([
            "--http",
            &format!("127.0.0.1:{port}"),
            "--title",
            "test box",
        ])
        .arg("--unix")
        .arg(dir.join("web.sock"))
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let servers = Servers {
        daemon,
        web,
        dir,
        port,
    };
    // Wait until the page lists metrics, which needs both servers up and the
    // daemon to have sampled.
    let start = Instant::now();
    loop {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            stream.write_all(b"GET /fragment HTTP/1.1\r\n\r\n").ok();
            let mut reply = String::new();
            stream.read_to_string(&mut reply).ok();
            if reply.contains("cpu_percent") {
                return servers;
            }
        }
        assert!(start.elapsed() < Duration::from_secs(15), "never came up");
        thread::sleep(Duration::from_millis(50));
    }
}

fn body(reply: &str) -> &str {
    reply.split_once("\r\n\r\n").unwrap().1
}

#[test]
fn serves_the_page_the_data_and_the_assets() {
    let servers = start();

    let page = servers.get_tcp("GET / HTTP/1.1\r\nHost: x\r\n\r\n");
    assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
    assert!(page.contains("<h1>test box</h1>"));
    assert!(page.contains("<option value=\"cpu_percent\">"));

    let data = servers.get_tcp("GET /data?metric=cpu_percent&range=1h HTTP/1.1\r\n\r\n");
    assert!(data.starts_with("HTTP/1.1 200 OK\r\n"), "{data}");
    let value: Value = serde_json::from_str(body(&data)).unwrap();
    assert_eq!(value["metric"]["name"], "cpu_percent");
    assert_ne!(value["timestamps"], json!([]));
    assert_eq!(
        value["avg"].as_array().unwrap().len(),
        value["timestamps"].as_array().unwrap().len()
    );

    let js = servers.get_tcp("GET /assets/uPlot.iife.min.js HTTP/1.1\r\n\r\n");
    assert!(js.contains("Content-Type: text/javascript"));

    let missing = servers.get_tcp("GET /data?metric=nope HTTP/1.1\r\n\r\n");
    assert!(missing.starts_with("HTTP/1.1 404 "), "{missing}");

    let post = servers.get_tcp("POST / HTTP/1.1\r\n\r\n");
    assert!(post.starts_with("HTTP/1.1 405 "), "{post}");
}

#[test]
fn also_serves_on_a_unix_socket() {
    let servers = start();
    let mut stream = UnixStream::connect(servers.web_socket()).unwrap();
    stream.write_all(b"GET /healthz HTTP/1.1\r\n\r\n").unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
    assert!(reply.ends_with("ok\n"));
}

#[test]
fn says_so_when_the_collector_is_down() {
    let mut servers = start();
    // Stop the collector (the Servers' Drop still cleans up the rest).
    servers.daemon.kill().unwrap();
    servers.daemon.wait().unwrap();
    let port = servers.port;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    let mut page = String::new();
    stream.read_to_string(&mut page).unwrap();
    assert!(page.contains("No metrics available"), "{page}");
}
