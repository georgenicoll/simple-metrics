//! The `smq` binary: a command-line client for a running simple-metrics. See
//! [`simple_metrics::query`] for the logic.

#![forbid(unsafe_code)]

use std::env;
use std::io;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use simple_metrics::query::{self, SOCKET_ENV};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let env_socket = env::var(SOCKET_ENV).ok();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    ExitCode::from(query::run(
        &args,
        env_socket.as_deref(),
        now_ms,
        query::send_over_socket,
        &mut io::stdout(),
        &mut io::stderr(),
    ))
}
