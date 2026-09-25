//! The command line: turning arguments into an action, kept apart from `main`
//! so it can be tested by passing in the arguments and reading back what was
//! written.

use std::io::Write;

use crate::VERSION;
use crate::config::{self, Config, Parsed, USAGE};

/// Process exit code for success.
pub const EXIT_OK: u8 = 0;
/// Process exit code for a command line the program can't act on.
pub const EXIT_USAGE: u8 = 2;

/// What the program should do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing more: exit with this code. (Any output has been written.)
    Exit(u8),
    /// Run the daemon with these settings.
    Serve(Config),
}

/// Interprets `args` (not including the program name), writing normal output
/// to `out` and errors to `err`.
pub fn run<O: Write, E: Write>(args: &[String], out: &mut O, err: &mut E) -> Outcome {
    // A failed write (a closed pipe, say) isn't worth failing over.
    match config::parse(args) {
        Ok(Parsed::Help) => {
            write!(out, "{USAGE}").ok();
            Outcome::Exit(EXIT_OK)
        }
        Ok(Parsed::Version) => {
            writeln!(out, "simple-metrics {VERSION}").ok();
            Outcome::Exit(EXIT_OK)
        }
        Ok(Parsed::Run(config)) => Outcome::Serve(config),
        Err(error) => {
            writeln!(err, "simple-metrics: {error}\n").ok();
            write!(err, "{USAGE}").ok();
            Outcome::Exit(EXIT_USAGE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns (outcome, stdout, stderr).
    fn run_with(args: &[&str]) -> (Outcome, String, String) {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let outcome = run(&args, &mut out, &mut err);
        (
            outcome,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn version_prints_the_crate_version() {
        for flag in ["-V", "--version"] {
            let (outcome, out, err) = run_with(&[flag]);
            assert_eq!(outcome, Outcome::Exit(EXIT_OK));
            assert_eq!(out, format!("simple-metrics {VERSION}\n"));
            assert!(err.is_empty());
        }
    }

    #[test]
    fn help_prints_usage_to_stdout() {
        for flag in ["-h", "--help"] {
            let (outcome, out, err) = run_with(&[flag]);
            assert_eq!(outcome, Outcome::Exit(EXIT_OK));
            assert!(out.starts_with("Usage: simple-metrics"));
            assert!(err.is_empty());
        }
    }

    #[test]
    fn an_unknown_argument_is_a_usage_error() {
        let (outcome, out, err) = run_with(&["--bogus"]);
        assert_eq!(outcome, Outcome::Exit(EXIT_USAGE));
        assert!(out.is_empty());
        assert!(err.contains("unknown argument '--bogus'"));
        assert!(err.contains("Usage:"));
    }

    #[test]
    fn a_bad_value_is_a_usage_error_that_names_the_option() {
        let (outcome, out, err) = run_with(&["--interval", "soon"]);
        assert_eq!(outcome, Outcome::Exit(EXIT_USAGE));
        assert!(out.is_empty());
        assert!(err.contains("--interval"), "{err}");
    }

    #[test]
    fn valid_settings_mean_run_the_daemon() {
        let (outcome, out, err) = run_with(&["--socket", "/tmp/x.sock"]);
        let Outcome::Serve(config) = outcome else {
            panic!("expected to serve, got {outcome:?}");
        };
        assert_eq!(config.socket.to_str(), Some("/tmp/x.sock"));
        assert!(out.is_empty() && err.is_empty());
    }

    #[test]
    fn no_arguments_at_all_runs_with_the_defaults() {
        let (outcome, _, _) = run_with(&[]);
        assert!(matches!(outcome, Outcome::Serve(_)));
    }
}
