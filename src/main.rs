//! The `simple-metrics` binary: see the library crate for the logic.

#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::{env, io};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    ExitCode::from(simple_metrics::cli::run(
        &args,
        &mut io::stdout(),
        &mut io::stderr(),
    ))
}
