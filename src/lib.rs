//! simple-metrics: a small in-memory metrics collector.
//!
//! The daemon samples a fixed set of variables at a regular interval, keeps
//! only the most recent records in memory (older ones are discarded as new
//! ones arrive) and serves them over a Unix socket. Nothing is persisted.
//!
//! The library holds the logic so that it can be tested without starting a
//! process; `main.rs` is a thin wrapper around it.
//!
//! - [`config`] and [`cli`]: the command line.
//! - [`proc`]: reading and parsing `/proc` and `/sys`.
//! - [`sampler`]: turning successive readings into rows of values.
//! - [`store`]: the bounded in-memory store of those rows.
//! - [`protocol`] and [`server`]: the JSON-lines socket API.
//! - [`daemon`]: putting it together.
//! - [`query`]: the `smq` command-line client.

#![forbid(unsafe_code)]

use std::fmt;
use std::io::{self, Write};

pub mod cli;
pub mod config;
pub mod daemon;
pub mod metrics;
pub mod proc;
pub mod protocol;
pub mod query;
pub mod sampler;
pub mod server;
pub mod state;
pub mod store;

/// This crate's version, as set in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Writes a line to standard error, where systemd sends it to the journal.
/// Never fails: a log line isn't worth an error of its own.
pub(crate) fn log(message: fmt::Arguments<'_>) {
    writeln!(io::stderr(), "{message}").ok();
}
