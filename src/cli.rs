//! The command line: argument handling, kept apart from `main` so it can be
//! tested by passing in the arguments and reading back what was written.

use std::io::Write;

use crate::VERSION;

/// Process exit code for success.
pub const EXIT_OK: u8 = 0;
/// Process exit code for a command line the program can't act on.
pub const EXIT_USAGE: u8 = 2;

const USAGE: &str = "\
Usage: simple-metrics [OPTIONS]

Options:
  -h, --help     Print this help and exit
  -V, --version  Print the version and exit
";

/// Runs the program with `args` (not including the program name), writing
/// normal output to `out` and errors to `err`. Returns the process exit code.
pub fn run<O: Write, E: Write>(args: &[String], out: &mut O, err: &mut E) -> u8 {
    // A failed write (a closed pipe, say) isn't worth failing over.
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => {
            write!(out, "{USAGE}").ok();
            EXIT_OK
        }
        Some("-V" | "--version") => {
            writeln!(out, "simple-metrics {VERSION}").ok();
            EXIT_OK
        }
        Some(other) => {
            writeln!(err, "simple-metrics: unknown argument '{other}'\n").ok();
            write!(err, "{USAGE}").ok();
            EXIT_USAGE
        }
        None => {
            writeln!(err, "simple-metrics: there is nothing to run yet\n").ok();
            write!(err, "{USAGE}").ok();
            EXIT_USAGE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Returns (exit code, stdout, stderr).
    fn run_with(args: &[&str]) -> (u8, String, String) {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&args, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn version_prints_the_crate_version() {
        for flag in ["-V", "--version"] {
            let (code, out, err) = run_with(&[flag]);
            assert_eq!(code, EXIT_OK);
            assert_eq!(out, format!("simple-metrics {VERSION}\n"));
            assert!(err.is_empty());
        }
    }

    #[test]
    fn help_prints_usage_to_stdout() {
        for flag in ["-h", "--help"] {
            let (code, out, err) = run_with(&[flag]);
            assert_eq!(code, EXIT_OK);
            assert!(out.starts_with("Usage: simple-metrics"));
            assert!(err.is_empty());
        }
    }

    #[test]
    fn an_unknown_argument_is_a_usage_error() {
        let (code, out, err) = run_with(&["--bogus"]);
        assert_eq!(code, EXIT_USAGE);
        assert!(out.is_empty());
        assert!(err.contains("unknown argument '--bogus'"));
        assert!(err.contains("Usage:"));
    }

    #[test]
    fn no_arguments_is_a_usage_error_until_there_is_something_to_run() {
        let (code, out, err) = run_with(&[]);
        assert_eq!(code, EXIT_USAGE);
        assert!(out.is_empty());
        assert!(err.contains("Usage:"));
    }
}
