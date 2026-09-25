//! The `simple-metrics` binary: see the library crate for the logic.

#![forbid(unsafe_code)]

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use simple_metrics::cli::{self, Outcome};
use simple_metrics::daemon;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match cli::run(&args, &mut io::stdout(), &mut io::stderr()) {
        Outcome::Exit(code) => ExitCode::from(code),
        Outcome::Serve(config) => {
            // `run` only ever returns a startup error.
            let Err(error) = daemon::run(&config);
            writeln!(io::stderr(), "simple-metrics: {error}").ok();
            ExitCode::FAILURE
        }
    }
}
