//! The logic behind `smq`, the command-line client: turning arguments into a
//! request, sending it over the socket and printing the answer for a person
//! (or as the raw JSON, for a script).
//!
//! Kept apart from `src/bin/smq.rs` so it can be tested with a fake
//! connection; the real one is [`send_over_socket`].

use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::VERSION;
use crate::config::parse_duration;

/// The socket the daemon listens on unless told otherwise.
pub const DEFAULT_SOCKET: &str = "/run/simple-metrics/simple-metrics.sock";
/// Where `--local` looks: the socket of a daemon run by hand for development.
pub const LOCAL_SOCKET: &str = "/tmp/simple-metrics.sock";
/// The environment variable that overrides [`DEFAULT_SOCKET`].
pub const SOCKET_ENV: &str = "SIMPLE_METRICS_SOCKET";

/// Exit code for success.
pub const EXIT_OK: u8 = 0;
/// Exit code for a request that could not be made or was refused.
pub const EXIT_FAILED: u8 = 1;
/// Exit code for a command line that can't be acted on.
pub const EXIT_USAGE: u8 = 2;

/// How long to wait for the daemon before giving up.
const TIMEOUT: Duration = Duration::from_secs(30);

const USAGE: &str = "\
Usage: smq [OPTIONS] <COMMAND>

Sends a query to a running simple-metrics and prints the answer.

Commands:
  info                    What the daemon holds: version, interval, records
  metrics                 The metrics it records, with units
  latest                  The most recent record
  read [READ OPTIONS]     Recorded history
  raw <JSON>              Send this request as it is (for anything else)

Read options:
  --from <TIME>           Only records from this time on (inclusive)
  --to <TIME>             Only records up to this time (inclusive)
  --metric <NAME>         Only this metric; repeat for several
  --step <TIME>           Summarise into buckets this wide (the mean of each)
  --points <N>            The same, choosing the width to give at most N buckets
  --extremes              With --step/--points: also give the min and max
  A TIME is `now`, how long ago (`90s`, `5m`, `2h`, `7d`; units ms, s, m, h,
  d), or milliseconds since the Unix epoch (a bare number).

Options:
  --socket <PATH>         The socket to use
                          [default: $SIMPLE_METRICS_SOCKET, else
                          /run/simple-metrics/simple-metrics.sock]
  --local                 Use /tmp/simple-metrics.sock (see run_local.sh)
  --json                  Print the response as JSON, exactly as received
  -h, --help              Print this help
  -V, --version           Print the version

Examples:
  smq info
  smq --local read --metric cpu_percent --from 1h --points 60 --extremes
";

/// A request to make, worked out from the command line.
#[derive(Debug, PartialEq)]
enum Command {
    Info,
    Metrics,
    Latest,
    Read(Box<Read>),
    Raw(String),
}

#[derive(Debug, Default, PartialEq)]
struct Read {
    from: Option<Time>,
    to: Option<Time>,
    metrics: Vec<String>,
    step: Option<Duration>,
    points: Option<u64>,
    extremes: bool,
}

/// A point in time given on the command line.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Time {
    Ago(Duration),
    At(u64),
}

#[derive(Debug, PartialEq)]
enum Parsed {
    Help,
    Version,
    Run {
        socket: Option<PathBuf>,
        json: bool,
        command: Command,
    },
}

fn parse_time(text: &str) -> Result<Time, String> {
    if text == "now" {
        return Ok(Time::Ago(Duration::ZERO));
    }
    if text.bytes().all(|b| b.is_ascii_digit()) && !text.is_empty() {
        return text
            .parse()
            .map(Time::At)
            .map_err(|_| format!("'{text}' is too large a time"));
    }
    parse_duration(text)
        .map(Time::Ago)
        .map_err(|why| format!("'{text}': {why}; use now, 5m, 2h, or epoch milliseconds"))
}

impl Time {
    fn resolve(self, now_ms: u64) -> u64 {
        match self {
            Time::At(ms) => ms,
            Time::Ago(ago) => {
                now_ms.saturating_sub(u64::try_from(ago.as_millis()).unwrap_or(u64::MAX))
            }
        }
    }
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut socket = None;
    let mut json = false;
    let mut args = args.iter();
    let mut command = None;
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--json" => json = true,
            "--local" => socket = Some(PathBuf::from(LOCAL_SOCKET)),
            "--socket" => socket = Some(PathBuf::from(value("--socket")?)),
            "info" | "metrics" | "latest" | "read" | "raw" => {
                command = Some(arg.clone());
                break;
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    let rest: Vec<String> = args.cloned().collect();
    let command = match command.as_deref() {
        None => return Err("no command given".to_owned()),
        Some("info") => no_arguments("info", &rest, Command::Info)?,
        Some("metrics") => no_arguments("metrics", &rest, Command::Metrics)?,
        Some("latest") => no_arguments("latest", &rest, Command::Latest)?,
        Some("raw") => match rest.as_slice() {
            [request] => Command::Raw(request.clone()),
            _ => return Err("raw takes exactly one argument: the JSON request".to_owned()),
        },
        Some(_) => Command::Read(Box::new(parse_read(&rest)?)),
    };
    Ok(Parsed::Run {
        socket,
        json,
        command,
    })
}

fn no_arguments(name: &str, rest: &[String], command: Command) -> Result<Command, String> {
    match rest.first() {
        None => Ok(command),
        Some(extra) => Err(format!("{name} takes no arguments (got '{extra}')")),
    }
}

fn parse_read(args: &[String]) -> Result<Read, String> {
    let mut read = Read::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--from" => read.from = Some(parse_time(&value("--from")?)?),
            "--to" => read.to = Some(parse_time(&value("--to")?)?),
            "--metric" => read.metrics.push(value("--metric")?),
            "--step" => {
                let text = value("--step")?;
                read.step = Some(parse_duration(&text).map_err(|why| format!("--step: {why}"))?);
            }
            "--points" => {
                let text = value("--points")?;
                read.points = Some(
                    text.parse()
                        .map_err(|_| format!("--points: '{text}' is not a whole number"))?,
                );
            }
            "--extremes" => read.extremes = true,
            other => return Err(format!("unknown read option '{other}'")),
        }
    }
    if read.step.is_some() && read.points.is_some() {
        return Err("--step and --points can't be combined".to_owned());
    }
    if read.extremes && read.step.is_none() && read.points.is_none() {
        return Err("--extremes needs --step or --points".to_owned());
    }
    Ok(read)
}

fn request_for(command: &Command, now_ms: u64) -> Result<String, String> {
    let request = match command {
        Command::Info => json!({"op": "info"}),
        Command::Metrics => json!({"op": "metrics"}),
        Command::Latest => json!({"op": "latest"}),
        Command::Raw(text) => {
            serde_json::from_str::<Value>(text).map_err(|e| format!("not valid JSON: {e}"))?
        }
        Command::Read(read) => {
            let mut request = Map::new();
            request.insert("op".to_owned(), json!("read"));
            if let Some(from) = read.from {
                request.insert("from".to_owned(), json!(from.resolve(now_ms)));
            }
            if let Some(to) = read.to {
                request.insert("to".to_owned(), json!(to.resolve(now_ms)));
            }
            if !read.metrics.is_empty() {
                request.insert("metrics".to_owned(), json!(read.metrics));
            }
            if let Some(step) = read.step {
                let ms = u64::try_from(step.as_millis()).unwrap_or(u64::MAX);
                request.insert("step_ms".to_owned(), json!(ms));
            }
            if let Some(points) = read.points {
                request.insert("max_points".to_owned(), json!(points));
            }
            if read.extremes {
                request.insert("extremes".to_owned(), json!(true));
            }
            Value::Object(request)
        }
    };
    Ok(request.to_string())
}

/// Sends `request` (one line) to the daemon listening on `socket` and returns
/// its one-line response.
///
/// # Errors
/// If the connection or the exchange fails.
pub fn send_over_socket(socket: &Path, request: &str) -> io::Result<String> {
    let stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut writer = &stream;
    writeln!(writer, "{request}")?;
    writer.flush()?;
    let mut response = String::new();
    if BufReader::new(&stream).read_line(&mut response)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the daemon closed the connection without answering",
        ));
    }
    Ok(response)
}

/// Runs `smq` with `args` (not including the program name).
///
/// `env_socket` is the value of [`SOCKET_ENV`], `now_ms` the current time, and
/// `send` makes the request (see [`send_over_socket`]); passing them in keeps
/// this testable. Returns the process exit code.
pub fn run<O, E, S>(
    args: &[String],
    env_socket: Option<&str>,
    now_ms: u64,
    send: S,
    out: &mut O,
    err: &mut E,
) -> u8
where
    O: Write,
    E: Write,
    S: FnOnce(&Path, &str) -> io::Result<String>,
{
    // A failed write (a closed pipe, say) isn't worth failing over.
    let (socket, json, command) = match parse(args) {
        Ok(Parsed::Help) => {
            write!(out, "{USAGE}").ok();
            return EXIT_OK;
        }
        Ok(Parsed::Version) => {
            writeln!(out, "smq {VERSION}").ok();
            return EXIT_OK;
        }
        Ok(Parsed::Run {
            socket,
            json,
            command,
        }) => (socket, json, command),
        Err(error) => {
            writeln!(err, "smq: {error}\n").ok();
            write!(err, "{USAGE}").ok();
            return EXIT_USAGE;
        }
    };
    let socket = socket.unwrap_or_else(|| {
        PathBuf::from(
            env_socket
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_SOCKET),
        )
    });
    let request = match request_for(&command, now_ms) {
        Ok(request) => request,
        Err(error) => {
            writeln!(err, "smq: {error}").ok();
            return EXIT_USAGE;
        }
    };
    let response = match send(&socket, &request) {
        Ok(response) => response,
        Err(error) => {
            writeln!(err, "smq: {}", describe_failure(&socket, &error)).ok();
            return EXIT_FAILED;
        }
    };
    let Ok(value) = serde_json::from_str::<Value>(&response) else {
        writeln!(err, "smq: the reply was not valid JSON").ok();
        return EXIT_FAILED;
    };
    if value["ok"] != json!(true) {
        if json {
            write!(out, "{}", response.trim_end()).ok();
            writeln!(out).ok();
        }
        let why = value["error"].as_str().unwrap_or("the request failed");
        writeln!(err, "smq: {why}").ok();
        return EXIT_FAILED;
    }
    if json {
        writeln!(out, "{}", response.trim_end()).ok();
    } else {
        writeln!(out, "{}", render(&command, &value).trim_end()).ok();
    }
    EXIT_OK
}

fn describe_failure(socket: &Path, error: &io::Error) -> String {
    let shown = socket.display();
    match error.kind() {
        io::ErrorKind::NotFound => format!(
            "no socket at {shown}. Is simple-metrics running? \
             (--local for one run by hand, or --socket <PATH>)"
        ),
        io::ErrorKind::PermissionDenied => format!(
            "not allowed to use {shown}. Only its owner and group may; \
             is this account in the group?"
        ),
        io::ErrorKind::ConnectionRefused => {
            format!("nothing is listening on {shown} (a stale socket?)")
        }
        _ => format!("{shown}: {error}"),
    }
}

// ---- turning the response into text --------------------------------------

fn render(command: &Command, response: &Value) -> String {
    match command {
        Command::Info => render_info(response),
        Command::Metrics => render_metrics(response),
        Command::Latest => render_latest(response),
        Command::Read(_) => render_read(response),
        // We don't know the shape of an arbitrary request's answer.
        Command::Raw(_) => response.to_string(),
    }
}

fn render_info(response: &Value) -> String {
    let mut text = String::new();
    for key in ["version", "interval_ms", "capacity", "len", "metrics"] {
        if let Some(value) = response.get(key) {
            writeln!(text, "{key:<12} {}", plain(value)).ok();
        }
    }
    text
}

fn render_metrics(response: &Value) -> String {
    let rows: Vec<Vec<String>> = response["metrics"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|m| {
                    ["name", "unit", "label"]
                        .map(|k| m[k].as_str().unwrap_or("").to_owned())
                        .to_vec()
                })
                .collect()
        })
        .unwrap_or_default();
    table(&["name", "unit", "label"], &rows)
}

fn render_latest(response: &Value) -> String {
    let Some(timestamp) = response["timestamp"].as_u64() else {
        return "nothing has been recorded yet".to_owned();
    };
    let rows: Vec<Vec<String>> = response["values"]
        .as_object()
        .map(|values| {
            values
                .iter()
                .map(|(name, v)| vec![name.clone(), number(v)])
                .collect()
        })
        .unwrap_or_default();
    format!(
        "at {}\n{}",
        iso(timestamp),
        table(&["metric", "value"], &rows)
    )
}

fn render_read(response: &Value) -> String {
    let empty = Vec::new();
    let timestamps = response["timestamps"].as_array().unwrap_or(&empty);
    let Some(series) = response["series"].as_object() else {
        return String::new();
    };
    // Each metric is one column, or three (mean, min, max) with extremes.
    let mut columns: Vec<(String, &Value)> = Vec::new();
    for (name, values) in series {
        columns.push((name.clone(), values));
        for (kind, label) in [("min", "min"), ("max", "max")] {
            if let Some(extra) = response.get(kind).and_then(|m| m.get(name)) {
                columns.push((format!("{name} ({label})"), extra));
            }
        }
    }
    let mut header = vec!["time"];
    header.extend(columns.iter().map(|(name, _)| name.as_str()));
    let rows: Vec<Vec<String>> = timestamps
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let mut row = vec![t.as_u64().map_or_else(|| "?".to_owned(), iso)];
            row.extend(columns.iter().map(|(_, values)| number(&values[i])));
            row
        })
        .collect();
    let mut text = table(&header, &rows);
    if let Some(step) = response["step_ms"].as_u64() {
        writeln!(text, "\n{} rows, {step} ms per bucket", rows.len()).ok();
    } else {
        writeln!(text, "\n{} rows", rows.len()).ok();
    }
    text
}

/// Left-aligned columns separated by two spaces.
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    let mut line = |cells: &mut dyn Iterator<Item = &str>| {
        let parts: Vec<String> = cells
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        writeln!(text, "{}", parts.join("  ").trim_end()).ok();
    };
    line(&mut header.iter().copied());
    for row in rows {
        line(&mut row.iter().map(String::as_str));
    }
    text
}

fn plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A metric value: up to three decimals with no trailing zeros, or `-` for a
/// gap.
fn number(value: &Value) -> String {
    match value.as_f64() {
        None => "-".to_owned(),
        Some(v) => {
            let text = format!("{v:.3}");
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        }
    }
}

/// Milliseconds since the epoch as `2026-09-26T13:54:07.250Z` (UTC).
fn iso(ms: u64) -> String {
    let (secs, millis) = (ms / 1000, ms % 1000);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (hour, minute, second) = (rest / 3600, rest % 3600 / 60, rest % 60);
    // Civil-from-days (Howard Hinnant's algorithm), for days since 1970-01-01.
    let z = i64::try_from(days).unwrap_or(i64::MAX / 2) + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_790_000_000_000;

    /// Runs `smq` with `args` against a daemon that answers `reply`. Returns
    /// (exit code, stdout, stderr, the request that was sent, the socket).
    fn smq(
        args: &[&str],
        env_socket: Option<&str>,
        reply: io::Result<&str>,
    ) -> (u8, String, String, Option<String>, Option<PathBuf>) {
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut sent = None;
        let mut used = None;
        let code = run(
            &args,
            env_socket,
            NOW,
            |socket, request| {
                sent = Some(request.to_owned());
                used = Some(socket.to_path_buf());
                reply.map(ToOwned::to_owned)
            },
            &mut out,
            &mut err,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            sent,
            used,
        )
    }

    fn request(args: &[&str]) -> Value {
        let (code, _, err, sent, _) = smq(args, None, Ok(r#"{"ok":true,"v":1}"#));
        assert_eq!(code, EXIT_OK, "{err}");
        serde_json::from_str(&sent.expect("a request should have been sent")).unwrap()
    }

    #[test]
    fn the_default_socket_is_the_run_one() {
        let (_, _, _, _, used) = smq(&["info"], None, Ok(r#"{"ok":true}"#));
        assert_eq!(used, Some(PathBuf::from(DEFAULT_SOCKET)));
    }

    #[test]
    fn local_uses_the_tmp_socket() {
        let (_, _, _, _, used) = smq(&["--local", "info"], None, Ok(r#"{"ok":true}"#));
        assert_eq!(used, Some(PathBuf::from("/tmp/simple-metrics.sock")));
    }

    #[test]
    fn the_environment_overrides_the_default_and_a_flag_overrides_the_environment() {
        let ok = Ok(r#"{"ok":true}"#);
        let (_, _, _, _, used) = smq(&["info"], Some("/x/env.sock"), ok);
        assert_eq!(used, Some(PathBuf::from("/x/env.sock")));
        let (_, _, _, _, used) = smq(
            &["--socket", "/x/flag.sock", "info"],
            Some("/x/env.sock"),
            Ok(r#"{"ok":true}"#),
        );
        assert_eq!(used, Some(PathBuf::from("/x/flag.sock")));
        let (_, _, _, _, used) = smq(&["info"], Some(""), Ok(r#"{"ok":true}"#));
        assert_eq!(
            used,
            Some(PathBuf::from(DEFAULT_SOCKET)),
            "empty means unset"
        );
    }

    #[test]
    fn simple_commands_send_their_op() {
        for op in ["info", "metrics", "latest"] {
            assert_eq!(request(&[op]), json!({"op": op}));
        }
    }

    #[test]
    fn read_with_nothing_else_asks_for_everything() {
        assert_eq!(request(&["read"]), json!({"op": "read"}));
    }

    #[test]
    fn read_options_become_request_fields() {
        let sent = request(&[
            "read",
            "--from",
            "1h",
            "--to",
            "now",
            "--metric",
            "cpu_percent",
            "--metric",
            "load1",
            "--points",
            "60",
            "--extremes",
        ]);
        assert_eq!(
            sent,
            json!({
                "op": "read",
                "from": NOW - 3_600_000,
                "to": NOW,
                "metrics": ["cpu_percent", "load1"],
                "max_points": 60,
                "extremes": true,
            })
        );
    }

    #[test]
    fn step_is_sent_in_milliseconds() {
        assert_eq!(request(&["read", "--step", "5m"])["step_ms"], 300_000);
    }

    #[test]
    fn a_bare_number_time_is_epoch_milliseconds() {
        let sent = request(&["read", "--from", "1790000000123", "--to", "1790000009999"]);
        assert_eq!(sent["from"], 1_790_000_000_123_u64);
        assert_eq!(sent["to"], 1_790_000_009_999_u64);
    }

    #[test]
    fn a_time_further_back_than_the_epoch_stops_at_zero() {
        assert_eq!(request(&["read", "--from", "99999d"])["from"], 0);
    }

    #[test]
    fn raw_sends_the_json_given() {
        assert_eq!(
            request(&["raw", r#"{"op":"read","metrics":["load1"]}"#]),
            json!({"op": "read", "metrics": ["load1"]})
        );
    }

    #[test]
    fn raw_rejects_json_that_is_not_json() {
        let (code, _, err, sent, _) = smq(&["raw", "{nope"], None, Ok("{}"));
        assert_eq!(code, EXIT_USAGE);
        assert!(err.contains("not valid JSON"), "{err}");
        assert!(sent.is_none(), "nothing should be sent");
    }

    #[test]
    fn bad_command_lines_are_usage_errors_and_send_nothing() {
        let cases: &[(&[&str], &str)] = &[
            (&[], "no command"),
            (&["bogus"], "unknown argument 'bogus'"),
            (&["--socket"], "--socket needs a value"),
            (&["info", "extra"], "takes no arguments"),
            (&["raw"], "exactly one argument"),
            (&["read", "--from"], "--from needs a value"),
            (&["read", "--from", "soon"], "'soon'"),
            (&["read", "--step", "0"], "--step"),
            (&["read", "--points", "many"], "--points"),
            (
                &["read", "--step", "5s", "--points", "9"],
                "can't be combined",
            ),
            (&["read", "--extremes"], "--extremes needs"),
            (&["read", "--wat"], "unknown read option"),
        ];
        for (args, expected) in cases {
            let (code, out, err, sent, _) = smq(args, None, Ok("{}"));
            assert_eq!(code, EXIT_USAGE, "{args:?}");
            assert!(out.is_empty(), "{args:?}");
            assert!(err.contains(expected), "{args:?}: {err}");
            assert!(sent.is_none(), "{args:?} should not send anything");
        }
    }

    #[test]
    fn help_and_version() {
        let (code, out, _, sent, _) = smq(&["--help"], None, Ok("{}"));
        assert_eq!(code, EXIT_OK);
        assert!(out.starts_with("Usage: smq"));
        assert!(sent.is_none());
        let (code, out, _, _, _) = smq(&["-V"], None, Ok("{}"));
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, format!("smq {VERSION}\n"));
    }

    #[test]
    fn json_prints_the_response_untouched() {
        let reply = r#"{"ok":true,"v":1,"z":[1,2]}"#;
        let (code, out, _, _, _) = smq(&["--json", "info"], None, Ok(reply));
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, format!("{reply}\n"));
    }

    #[test]
    fn a_refusal_is_reported_and_fails() {
        let reply = r#"{"ok":false,"v":1,"error":"unknown metric 'x'"}"#;
        let (code, out, err, _, _) = smq(&["read"], None, Ok(reply));
        assert_eq!(code, EXIT_FAILED);
        assert!(out.is_empty());
        assert!(err.contains("unknown metric 'x'"), "{err}");
    }

    #[test]
    fn connection_failures_say_what_to_try() {
        for (kind, expected) in [
            (io::ErrorKind::NotFound, "--local"),
            (io::ErrorKind::PermissionDenied, "group"),
            (io::ErrorKind::ConnectionRefused, "stale"),
        ] {
            let (code, out, err, _, _) = smq(&["info"], None, Err(kind.into()));
            assert_eq!(code, EXIT_FAILED);
            assert!(out.is_empty());
            assert!(err.contains(expected), "{kind:?}: {err}");
            assert!(err.contains(DEFAULT_SOCKET), "{err}");
        }
    }

    #[test]
    fn a_reply_that_is_not_json_fails() {
        let (code, _, err, _, _) = smq(&["info"], None, Ok("hello"));
        assert_eq!(code, EXIT_FAILED);
        assert!(err.contains("not valid JSON"), "{err}");
    }

    #[test]
    fn info_is_key_value_lines() {
        let reply = r#"{"ok":true,"v":1,"version":"0.2.0","interval_ms":5000,"capacity":10,"len":3,"metrics":15}"#;
        let (_, out, _, _, _) = smq(&["info"], None, Ok(reply));
        assert!(out.contains("version      0.2.0\n"), "{out}");
        assert!(out.contains("interval_ms  5000\n"), "{out}");
    }

    #[test]
    fn metrics_is_a_table() {
        let reply = r#"{"ok":true,"metrics":[{"name":"cpu_percent","label":"CPU","unit":"%"},{"name":"load1","label":"Load","unit":""}]}"#;
        let (_, out, _, _, _) = smq(&["metrics"], None, Ok(reply));
        assert_eq!(
            out,
            "name         unit  label\ncpu_percent  %     CPU\nload1              Load\n"
        );
    }

    #[test]
    fn latest_shows_the_time_and_values_with_gaps() {
        let reply =
            r#"{"ok":true,"timestamp":1790000000250,"values":{"a":1.5,"b":null,"c":20.0004}}"#;
        let (_, out, _, _, _) = smq(&["latest"], None, Ok(reply));
        assert!(out.starts_with("at 2026-09-21T14:13:20.250Z\n"), "{out}");
        assert!(out.contains("a       1.5\n"), "{out}");
        assert!(out.contains("b       -\n"), "{out}");
        assert!(out.contains("c       20\n"), "{out}");
    }

    #[test]
    fn latest_with_nothing_recorded() {
        let reply = r#"{"ok":true,"timestamp":null,"values":{}}"#;
        let (code, out, _, _, _) = smq(&["latest"], None, Ok(reply));
        assert_eq!(code, EXIT_OK);
        assert_eq!(out, "nothing has been recorded yet\n");
    }

    #[test]
    fn read_is_a_table_with_extremes_beside_each_metric() {
        let reply = r#"{"ok":true,"step_ms":5000,"timestamps":[1790000000000,1790000005000],
            "series":{"cpu":[1.0,null]},"min":{"cpu":[0.5,null]},"max":{"cpu":[2,null]}}"#;
        let (_, out, _, _, _) = smq(&["read", "--points", "2", "--extremes"], None, Ok(reply));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            "time                      cpu  cpu (min)  cpu (max)"
        );
        assert_eq!(lines[1], "2026-09-21T14:13:20.000Z  1    0.5        2");
        assert_eq!(lines[2], "2026-09-21T14:13:25.000Z  -    -          -");
        assert!(out.contains("2 rows, 5000 ms per bucket"), "{out}");
    }

    #[test]
    fn iso_handles_the_epoch_leap_days_and_year_ends() {
        assert_eq!(iso(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(
            iso(1_709_164_800_000 + 86_399_999),
            "2024-02-29T23:59:59.999Z"
        );
        assert_eq!(iso(1_735_689_599_000), "2024-12-31T23:59:59.000Z");
        assert_eq!(iso(4_102_444_800_000), "2100-01-01T00:00:00.000Z");
    }
}
