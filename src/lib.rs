//! simple-metrics: a small in-memory metrics collector.
//!
//! The daemon samples a fixed set of variables at a regular interval, keeps
//! only the most recent records in memory (older ones are discarded as new
//! ones arrive) and serves them over a Unix socket. Nothing is persisted.
//!
//! The library holds the logic so that it can be tested without starting a
//! process; `main.rs` is a thin wrapper around it.

#![forbid(unsafe_code)]

pub mod cli;
pub mod store;

/// This crate's version, as set in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
