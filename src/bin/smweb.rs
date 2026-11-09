//! The `smweb` binary: a small web server for the metrics chart. See
//! [`simple_metrics::web`] for the logic.

#![forbid(unsafe_code)]

use std::env;
use std::io::{self, Write};
use std::process::ExitCode;

use simple_metrics::web;
use simple_metrics::web_cli::{self, Outcome};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match web_cli::run(&args, &mut io::stdout(), &mut io::stderr()) {
        Outcome::Exit(code) => ExitCode::from(code),
        Outcome::Serve(config) => {
            // `run` only ever returns a startup error.
            let Err(error) = web::run(config);
            writeln!(io::stderr(), "smweb: {error}").ok();
            ExitCode::FAILURE
        }
    }
}
