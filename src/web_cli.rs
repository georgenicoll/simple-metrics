//! The `smweb` command line: turning arguments into a [`Config`], kept apart
//! from `main` so it can be tested by passing in the arguments and reading
//! back what was written.

use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;

use crate::VERSION;
use crate::config::{DEFAULT_SOCKET, DEFAULT_SOCKET_MODE};
use crate::web::{Config, Site};

/// Process exit code for success.
pub const EXIT_OK: u8 = 0;
/// Process exit code for a command line the program can't act on.
pub const EXIT_USAGE: u8 = 2;

/// Where to serve HTTP if neither `--http` nor `--unix` is given: this
/// machine only, since there is no login.
pub const DEFAULT_HTTP: &str = "127.0.0.1:8080";

/// The usage text.
pub const USAGE: &str = "\
Usage: smweb [OPTIONS]

A small read-only web server for the metrics chart. It asks a running
simple-metrics for the data, and serves the page, its script and styles
(all built in) and the chart's data. There is no login: listen only where
whoever can connect may see the metrics.

Options:
  --socket <PATH>      The simple-metrics socket to read from
                       [default: /run/simple-metrics/simple-metrics.sock]
  --http <ADDR:PORT>   Serve HTTP on this address, e.g. 0.0.0.0:80 or
                       192.168.1.10:8080 [default if no --unix:
                       127.0.0.1:8080]
  --unix <PATH>        Also (or instead) serve HTTP on this Unix socket, for
                       a reverse proxy on the same machine
  --unix-mode <MODE>   The Unix socket's permissions, in octal [default: 660]
  --base-path <PATH>   Where a reverse proxy mounts this server, so that the
                       links in its pages work, e.g. /metrics [default: none]
  --title <TEXT>       The heading of the page [default: Metrics]
  -h, --help           Print this help
  -V, --version        Print the version

Pages: / (the chart), /fragment (the chart without a page around it, to
embed), /data, /assets/*, /healthz.
";

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Print the usage.
    Help,
    /// Print the version.
    Version,
    /// Serve with these settings.
    Run(Config),
}

/// What the program should do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing more: exit with this code. (Any output has been written.)
    Exit(u8),
    /// Run the server with these settings.
    Serve(Config),
}

fn valid_base_path(path: &str) -> bool {
    path.is_empty()
        || (path.starts_with('/')
            && !path.ends_with('/')
            && !path.contains("//")
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.')))
}

/// Interprets `args` (not including the program name).
///
/// # Errors
/// A message naming what's wrong with the arguments.
pub fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut collector_socket = PathBuf::from(DEFAULT_SOCKET);
    let mut http = None;
    let mut unix = None;
    let mut unix_mode = DEFAULT_SOCKET_MODE;
    let mut base_path = String::new();
    let mut title = "Metrics".to_owned();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--socket" => collector_socket = PathBuf::from(value("--socket")?),
            "--http" => {
                let text = value("--http")?;
                http = Some(text.parse::<SocketAddr>().map_err(|_| {
                    format!("--http: '{text}' isn't an address and port, like 0.0.0.0:80")
                })?);
            }
            "--unix" => unix = Some(PathBuf::from(value("--unix")?)),
            "--unix-mode" => {
                let text = value("--unix-mode")?;
                unix_mode = u32::from_str_radix(&text, 8)
                    .ok()
                    .filter(|mode| *mode <= 0o777)
                    .ok_or_else(|| {
                        format!("--unix-mode: '{text}' isn't an octal mode (000-777)")
                    })?;
            }
            "--base-path" => {
                base_path = value("--base-path")?;
                if !valid_base_path(&base_path) {
                    return Err(format!(
                        "--base-path: '{base_path}' must be empty or start with / and not end with one"
                    ));
                }
            }
            "--title" => title = value("--title")?,
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if http.is_none() && unix.is_none() {
        http = DEFAULT_HTTP.parse().ok();
    }
    Ok(Parsed::Run(Config {
        collector_socket,
        http,
        unix,
        unix_mode,
        site: Site { base_path, title },
    }))
}

/// Interprets `args` (not including the program name), writing normal output
/// to `out` and errors to `err`.
pub fn run<O: Write, E: Write>(args: &[String], out: &mut O, err: &mut E) -> Outcome {
    // A failed write (a closed pipe, say) isn't worth failing over.
    match parse(args) {
        Ok(Parsed::Help) => {
            write!(out, "{USAGE}").ok();
            Outcome::Exit(EXIT_OK)
        }
        Ok(Parsed::Version) => {
            writeln!(out, "smweb {VERSION}").ok();
            Outcome::Exit(EXIT_OK)
        }
        Ok(Parsed::Run(config)) => Outcome::Serve(config),
        Err(error) => {
            writeln!(err, "smweb: {error}\n").ok();
            write!(err, "{USAGE}").ok();
            Outcome::Exit(EXIT_USAGE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(args: &[&str]) -> Config {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        match parse(&args) {
            Ok(Parsed::Run(config)) => config,
            other => panic!("expected a config, got {other:?}"),
        }
    }

    fn error(args: &[&str]) -> String {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        parse(&args).unwrap_err()
    }

    #[test]
    fn the_defaults_serve_this_machine_only() {
        let config = config(&[]);
        assert_eq!(config.http, Some("127.0.0.1:8080".parse().unwrap()));
        assert_eq!(config.unix, None);
        assert_eq!(config.collector_socket.to_str(), Some(DEFAULT_SOCKET));
        assert_eq!(config.site.base_path, "");
        assert_eq!(config.site.title, "Metrics");
    }

    #[test]
    fn a_unix_socket_alone_means_no_tcp() {
        let config = config(&["--unix", "/run/x.sock"]);
        assert_eq!(config.http, None);
        assert_eq!(config.unix.unwrap().to_str(), Some("/run/x.sock"));
        assert_eq!(config.unix_mode, 0o660);
    }

    #[test]
    fn everything_can_be_set() {
        let config = config(&[
            "--socket",
            "/tmp/m.sock",
            "--http",
            "0.0.0.0:80",
            "--unix",
            "/tmp/w.sock",
            "--unix-mode",
            "600",
            "--base-path",
            "/metrics",
            "--title",
            "my server",
        ]);
        assert_eq!(config.collector_socket.to_str(), Some("/tmp/m.sock"));
        assert_eq!(config.http, Some("0.0.0.0:80".parse().unwrap()));
        assert_eq!(config.unix_mode, 0o600);
        assert_eq!(config.site.base_path, "/metrics");
        assert_eq!(config.site.title, "my server");
    }

    #[test]
    fn bad_values_name_the_option() {
        assert!(error(&["--http", "localhost"]).contains("--http"));
        assert!(error(&["--http", "80"]).contains("--http"));
        assert!(error(&["--unix-mode", "999"]).contains("--unix-mode"));
        assert!(error(&["--unix-mode", "1000"]).contains("--unix-mode"));
        assert!(error(&["--base-path", "metrics"]).contains("--base-path"));
        assert!(error(&["--base-path", "/metrics/"]).contains("--base-path"));
        assert!(error(&["--base-path", "/a\"b"]).contains("--base-path"));
        assert!(error(&["--title"]).contains("needs a value"));
        assert!(error(&["--bogus"]).contains("unknown argument '--bogus'"));
    }

    #[test]
    fn help_and_version_print_and_exit() {
        let args = |a: &str| vec![a.to_owned()];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(
            run(&args("--help"), &mut out, &mut err),
            Outcome::Exit(EXIT_OK)
        );
        assert!(String::from_utf8(out).unwrap().starts_with("Usage: smweb"));
        let mut out = Vec::new();
        assert_eq!(run(&args("-V"), &mut out, &mut err), Outcome::Exit(EXIT_OK));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("smweb {VERSION}\n")
        );
        assert_eq!(err, Vec::<u8>::new());
    }

    #[test]
    fn a_usage_error_goes_to_stderr_with_the_usage() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let outcome = run(&["--bogus".to_owned()], &mut out, &mut err);
        assert_eq!(outcome, Outcome::Exit(EXIT_USAGE));
        assert_eq!(out, Vec::<u8>::new());
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("smweb: unknown argument") && err.contains("Usage:"));
    }
}
