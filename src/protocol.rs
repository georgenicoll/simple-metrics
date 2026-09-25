//! The socket protocol: JSON lines.
//!
//! A client sends one JSON object per line and gets exactly one line of JSON
//! back for each. A connection can carry any number of requests.
//!
//! ```text
//! -> {"op":"info"}
//! <- {"ok":true,"v":1,"version":"0.2.0","interval_ms":5000,"capacity":120960,"len":42,"metrics":15}
//! ```
//!
//! Every response has `"ok"` and `"v"` (the protocol version). A failure is
//! `{"ok":false,"v":1,"error":"..."}`. A request may include `"v"` too, to
//! insist on a version: the server refuses one it doesn't speak. Fields it
//! doesn't know are ignored, so a later version can add to a request.
//!
//! | `op` | Response fields |
//! |---|---|
//! | `info` | `version`, `interval_ms`, `capacity`, `len`, `metrics` (a count) |
//! | `metrics` | `metrics`: a list of `{name, label, unit}` in record order |
//! | `latest` | `timestamp` (ms since the Unix epoch, or `null` if nothing is recorded yet) and `values`: `{name: number or null}` |
//! | `read` | see below |
//!
//! ## `read`
//!
//! With no other fields, `read` returns every record held: `timestamps`
//! (oldest first) and `series`: `{name: [number or null, ...]}`, each list the
//! same length as `timestamps`. A value that couldn't be measured is `null`.
//! Optional fields narrow and summarise it:
//!
//! | Field | Meaning |
//! |---|---|
//! | `from`, `to` | Only records from `from` to `to`, both inclusive, in milliseconds since the Unix epoch. Either may be left out. |
//! | `metrics` | Only these metrics, by name (see `metrics`). `series` then has just these. |
//! | `step_ms` | Group the records into buckets this many milliseconds wide and return the mean of each metric per bucket. |
//! | `max_points` | The same, letting the server pick the bucket width so there are at most this many buckets (2 to 200,000). |
//! | `extremes` | With `step_ms` or `max_points`: also return the smallest and largest value per bucket, as `min` and `max`, laid out like `series`. |
//!
//! A downsampled response also has `step_ms` (the bucket width used), and
//! `timestamps` are the buckets' start times. Buckets are aligned to multiples
//! of the width since the Unix epoch, so a later query has the same edges. Every
//! bucket from the first record's to the last's is present, and one with no
//! usable values is `null`.

use std::fmt;
use std::io::{self, Write};

use serde_json::{Map, Value, json};

use crate::VERSION as CRATE_VERSION;
use crate::metrics::Schema;
use crate::state::Shared;
use crate::store::{Downsampled, MAX_BUCKETS, Snapshot, step_for_max_points};

/// The protocol version this server speaks.
pub const VERSION: u32 = 1;

/// The longest request line accepted, in bytes.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// The most buckets a `max_points` request may ask for. (The same as the
/// most buckets any query may produce.)
pub const MAX_POINTS: u64 = MAX_BUCKETS;

/// The most metric names one request may list.
pub const MAX_METRIC_NAMES: usize = 256;

/// What a client can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Facts about the collector and its store.
    Info,
    /// The list of metrics in every record.
    Metrics,
    /// The newest record.
    Latest,
    /// Records, optionally narrowed and summarised.
    Read(ReadParams),
}

/// How a `read` narrows and summarises the records. The default, with
/// everything left out, asks for every record and every metric.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadParams {
    /// The earliest record wanted, in milliseconds since the Unix epoch.
    pub from: Option<u64>,
    /// The latest record wanted.
    pub to: Option<u64>,
    /// The metrics wanted, by name; `None` for all of them.
    pub metrics: Option<Vec<String>>,
    /// How to group the records into buckets; `None` for no grouping.
    pub downsample: Option<Downsample>,
    /// Whether to return each bucket's smallest and largest value too.
    pub extremes: bool,
}

/// How a `read` chooses its buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Downsample {
    /// Buckets this many milliseconds wide.
    StepMs(u64),
    /// As wide as needed to give at most this many buckets.
    MaxPoints(u64),
}

/// Why a request line couldn't be turned into a [`Request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// Not valid JSON.
    NotJson,
    /// Valid JSON, but not an object.
    NotAnObject,
    /// No `op` field.
    MissingOp,
    /// `op` wasn't a string.
    OpNotAString,
    /// An `op` this server doesn't have.
    UnknownOp(String),
    /// `v` wasn't a whole number.
    BadVersion,
    /// `v` asked for a version other than [`VERSION`].
    UnsupportedVersion(u64),
    /// A field of a `read` had an unusable value.
    InvalidField {
        /// The field's name.
        field: &'static str,
        /// What is wrong with it.
        reason: &'static str,
    },
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotJson => write!(f, "request is not valid JSON"),
            Self::NotAnObject => write!(f, "request must be a JSON object"),
            Self::MissingOp => write!(f, "request has no \"op\""),
            Self::OpNotAString => write!(f, "\"op\" must be a string"),
            Self::UnknownOp(op) => {
                let shown: String = op.chars().take(40).collect();
                write!(
                    f,
                    "unknown op \"{shown}\" (try info, metrics, latest or read)"
                )
            }
            Self::BadVersion => write!(f, "\"v\" must be a whole number"),
            Self::UnsupportedVersion(v) => {
                write!(
                    f,
                    "unsupported protocol version {v} (this server speaks {VERSION})"
                )
            }
            Self::InvalidField { field, reason } => write!(f, "\"{field}\" {reason}"),
        }
    }
}

impl std::error::Error for RequestError {}

/// Turns one request line into a [`Request`].
///
/// # Errors
/// If the line isn't a JSON object with a known `op` (and, if it has a `v`,
/// the supported one), or a `read`'s fields aren't usable.
pub fn parse_request(line: &str) -> Result<Request, RequestError> {
    let value: Value = serde_json::from_str(line).map_err(|_| RequestError::NotJson)?;
    let object = value.as_object().ok_or(RequestError::NotAnObject)?;
    if let Some(version) = object.get("v") {
        let version = version.as_u64().ok_or(RequestError::BadVersion)?;
        if version != u64::from(VERSION) {
            return Err(RequestError::UnsupportedVersion(version));
        }
    }
    let op = object
        .get("op")
        .ok_or(RequestError::MissingOp)?
        .as_str()
        .ok_or(RequestError::OpNotAString)?;
    match op {
        "info" => Ok(Request::Info),
        "metrics" => Ok(Request::Metrics),
        "latest" => Ok(Request::Latest),
        "read" => Ok(Request::Read(parse_read(object)?)),
        other => Err(RequestError::UnknownOp(other.to_owned())),
    }
}

fn invalid(field: &'static str, reason: &'static str) -> RequestError {
    RequestError::InvalidField { field, reason }
}

/// A whole number field that may be left out (or `null`).
fn optional_u64(
    object: &Map<String, Value>,
    field: &'static str,
    reason: &'static str,
) -> Result<Option<u64>, RequestError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(field, reason)),
    }
}

fn parse_read(object: &Map<String, Value>) -> Result<ReadParams, RequestError> {
    const MILLIS: &str = "must be a whole number of milliseconds since the Unix epoch";
    let from = optional_u64(object, "from", MILLIS)?;
    let to = optional_u64(object, "to", MILLIS)?;
    if let (Some(from), Some(to)) = (from, to) {
        if from > to {
            return Err(invalid("from", "must not be after \"to\""));
        }
    }

    let metrics = match object.get("metrics") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => {
            if items.is_empty() {
                return Err(invalid("metrics", "must not be empty"));
            }
            if items.len() > MAX_METRIC_NAMES {
                return Err(invalid("metrics", "has too many names"));
            }
            let names: Option<Vec<String>> = items
                .iter()
                .map(|item| item.as_str().map(ToOwned::to_owned))
                .collect();
            Some(names.ok_or_else(|| invalid("metrics", "must be a list of metric names"))?)
        }
        Some(_) => return Err(invalid("metrics", "must be a list of metric names")),
    };

    let step_ms = optional_u64(object, "step_ms", "must be a whole number of milliseconds")?;
    let max_points = optional_u64(object, "max_points", "must be a whole number")?;
    let downsample = match (step_ms, max_points) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "max_points",
                "cannot be used together with \"step_ms\"",
            ));
        }
        (Some(0), None) => return Err(invalid("step_ms", "must be at least 1")),
        (Some(step), None) => Some(Downsample::StepMs(step)),
        (None, Some(points)) if !(2..=MAX_POINTS).contains(&points) => {
            return Err(invalid("max_points", "must be between 2 and 200000"));
        }
        (None, Some(points)) => Some(Downsample::MaxPoints(points)),
        (None, None) => None,
    };

    let extremes = match object.get("extremes") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(invalid("extremes", "must be true or false")),
    };
    if extremes && downsample.is_none() {
        return Err(invalid("extremes", "needs \"step_ms\" or \"max_points\""));
    }

    Ok(ReadParams {
        from,
        to,
        metrics,
        downsample,
        extremes,
    })
}

/// Writes the one-line response to `request`, and flushes.
///
/// # Errors
/// If writing fails, for example because the client went away.
pub fn respond<W: Write>(w: &mut W, request: &Request, shared: &Shared) -> io::Result<()> {
    match request {
        Request::Info => respond_info(w, shared)?,
        Request::Metrics => respond_metrics(w, shared)?,
        Request::Latest => respond_latest(w, shared)?,
        Request::Read(params) => match answer_read(shared, params) {
            Ok(answer) => write_answer(w, &answer)?,
            Err(message) => return respond_error(w, &message),
        },
    }
    w.write_all(b"\n")?;
    w.flush()
}

/// Writes a one-line failure response, and flushes.
///
/// # Errors
/// If writing fails, for example because the client went away.
pub fn respond_error<W: Write>(w: &mut W, message: &str) -> io::Result<()> {
    write!(w, "{{\"ok\":false,\"v\":{VERSION},\"error\":")?;
    serde_json::to_writer(&mut *w, message)?;
    w.write_all(b"}\n")?;
    w.flush()
}

fn respond_info<W: Write>(w: &mut W, shared: &Shared) -> io::Result<()> {
    let (capacity, len) = {
        let store = shared.read();
        (store.capacity(), store.len())
    };
    serde_json::to_writer(
        w,
        &json!({
            "ok": true,
            "v": VERSION,
            "version": CRATE_VERSION,
            "interval_ms": interval_ms(shared),
            "capacity": capacity,
            "len": len,
            "metrics": shared.schema.len(),
        }),
    )?;
    Ok(())
}

fn interval_ms(shared: &Shared) -> u64 {
    u64::try_from(shared.interval.as_millis()).unwrap_or(u64::MAX)
}

fn respond_metrics<W: Write>(w: &mut W, shared: &Shared) -> io::Result<()> {
    let metrics: Vec<Value> = shared
        .schema
        .metrics()
        .iter()
        .map(|m| json!({"name": m.name, "label": m.label, "unit": m.unit}))
        .collect();
    serde_json::to_writer(w, &json!({"ok": true, "v": VERSION, "metrics": metrics}))?;
    Ok(())
}

fn respond_latest<W: Write>(w: &mut W, shared: &Shared) -> io::Result<()> {
    // Copy the record out and let go of the lock before writing anything.
    let latest = shared.read().latest().map(|(t, row)| (t, row.to_vec()));
    let (timestamp, values) = match latest {
        Some((timestamp, row)) => {
            let values: Map<String, Value> = shared
                .schema
                .metrics()
                .iter()
                .zip(row)
                // A NaN (a value that couldn't be measured) becomes null.
                .map(|(metric, value)| (metric.name.clone(), Value::from(value)))
                .collect();
            (Value::from(timestamp), values)
        }
        None => (Value::Null, Map::new()),
    };
    serde_json::to_writer(
        w,
        &json!({"ok": true, "v": VERSION, "timestamp": timestamp, "values": values}),
    )?;
    Ok(())
}

/// What a `read` found, copied out of the store so that the lock is let go
/// before any of it is written to a possibly slow client.
enum Answer {
    Raw {
        names: Vec<String>,
        snapshot: Snapshot,
    },
    Downsampled {
        names: Vec<String>,
        result: Downsampled,
    },
}

/// The names and schema positions of the metrics a `read` asks for: all of
/// them, or those listed (each once, in the order first listed).
fn resolve_metrics(
    schema: &Schema,
    wanted: Option<&[String]>,
) -> Result<(Vec<String>, Vec<usize>), String> {
    let all = schema.metrics();
    let Some(wanted) = wanted else {
        return Ok((
            all.iter().map(|m| m.name.clone()).collect(),
            (0..all.len()).collect(),
        ));
    };
    let (mut names, mut columns) = (Vec::new(), Vec::new());
    for name in wanted {
        let Some(index) = all.iter().position(|m| &m.name == name) else {
            let shown: String = name.chars().take(60).collect();
            return Err(format!(
                "unknown metric \"{shown}\" (ask {{\"op\":\"metrics\"}} for the list)"
            ));
        };
        if !columns.contains(&index) {
            names.push(name.clone());
            columns.push(index);
        }
    }
    Ok((names, columns))
}

/// Works out the answer to a `read`, or why it can't be given. The store is
/// locked only for as long as that takes: a copy for a plain read, or one pass
/// over the records in range for a downsampled one (a few milliseconds at most,
/// even for a full week).
fn answer_read(shared: &Shared, params: &ReadParams) -> Result<Answer, String> {
    let (names, columns) = resolve_metrics(&shared.schema, params.metrics.as_deref())?;
    let (from, to) = (params.from, params.to);
    let Some(downsample) = params.downsample else {
        let snapshot = shared.read().snapshot_range(from, to, &columns);
        return Ok(Answer::Raw { names, snapshot });
    };

    let result = {
        let store = shared.read();
        let step = match downsample {
            Downsample::StepMs(step) => step,
            Downsample::MaxPoints(points) => store
                .bounds(from, to)
                .map_or(interval_ms(shared).max(1), |(first, last)| {
                    step_for_max_points(first, last, points, interval_ms(shared))
                }),
        };
        store.downsample(from, to, &columns, step, params.extremes)
    }
    .map_err(|error| error.to_string())?;
    Ok(Answer::Downsampled { names, result })
}

fn write_answer<W: Write>(w: &mut W, answer: &Answer) -> io::Result<()> {
    // Written piece by piece, not built as one JSON value first: a full
    // store is over a million numbers, and a tree of them costs far more
    // memory than the store does.
    write!(w, "{{\"ok\":true,\"v\":{VERSION},")?;
    match answer {
        Answer::Raw { names, snapshot } => {
            write_timestamps(w, &snapshot.timestamps)?;
            w.write_all(b",\"series\":")?;
            write_series(w, names, &snapshot.columns)?;
        }
        Answer::Downsampled { names, result } => {
            write!(w, "\"step_ms\":{},", result.step_ms)?;
            write_timestamps(w, &result.timestamps)?;
            w.write_all(b",\"series\":")?;
            write_series(w, names, &result.avg)?;
            if let Some(min) = &result.min {
                w.write_all(b",\"min\":")?;
                write_series(w, names, min)?;
            }
            if let Some(max) = &result.max {
                w.write_all(b",\"max\":")?;
                write_series(w, names, max)?;
            }
        }
    }
    w.write_all(b"}")
}

fn write_timestamps<W: Write>(w: &mut W, timestamps: &[u64]) -> io::Result<()> {
    w.write_all(b"\"timestamps\":[")?;
    for (i, timestamp) in timestamps.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        write!(w, "{timestamp}")?;
    }
    w.write_all(b"]")
}

/// Writes `{"name":[values...],...}`.
fn write_series<W: Write>(w: &mut W, names: &[String], columns: &[Vec<f64>]) -> io::Result<()> {
    w.write_all(b"{")?;
    for (i, (name, column)) in names.iter().zip(columns).enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        serde_json::to_writer(&mut *w, name)?;
        w.write_all(b":[")?;
        for (j, value) in column.iter().enumerate() {
            if j > 0 {
                w.write_all(b",")?;
            }
            // serde_json writes a NaN as null, which is what we want.
            serde_json::to_writer(&mut *w, value)?;
        }
        w.write_all(b"]")?;
    }
    w.write_all(b"}")
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use super::*;
    use crate::store::Store;

    fn shared_with(rows: &[(u64, &[f64])]) -> Shared {
        let schema = Schema::new(&["eth0".to_owned()]);
        let store = Store::new(NonZeroUsize::new(64).unwrap(), schema.len()).unwrap();
        let shared = Shared::new(schema, Duration::from_secs(5), store);
        for (timestamp, row) in rows {
            shared.write().push(*timestamp, row).unwrap();
        }
        shared
    }

    /// A full row for the schema above: 5 fixed metrics and eth0's two rates.
    fn row(seed: f64) -> Vec<f64> {
        (0..7).map(|i| seed + f64::from(i)).collect()
    }

    fn newlines(bytes: &[u8]) -> usize {
        std::str::from_utf8(bytes).unwrap().matches('\n').count()
    }

    fn respond_to(request: &Request, shared: &Shared) -> Value {
        let mut out = Vec::new();
        respond(&mut out, request, shared).unwrap();
        assert_eq!(out.last(), Some(&b'\n'));
        assert_eq!(newlines(&out), 1, "one line only");
        serde_json::from_slice(&out).unwrap()
    }

    /// Parses a request line that is expected to be a valid `read`.
    fn read(line: &str) -> Request {
        parse_request(&format!(r#"{{"op":"read",{line}}}"#)).unwrap()
    }

    fn read_error(line: &str) -> String {
        parse_request(&format!(r#"{{"op":"read",{line}}}"#))
            .unwrap_err()
            .to_string()
    }

    /// `n` records, 5 s apart from 5000 ms, whose values are all `i` for the
    /// `i`th record (1-based).
    fn steady(n: u32) -> Shared {
        let rows: Vec<(u64, Vec<f64>)> = (1..=n)
            .map(|i| (u64::from(i) * 5000, vec![f64::from(i); 7]))
            .collect();
        let borrowed: Vec<(u64, &[f64])> = rows.iter().map(|(t, r)| (*t, r.as_slice())).collect();
        shared_with(&borrowed)
    }

    fn everything() -> Request {
        Request::Read(ReadParams::default())
    }

    // ---- parsing ------------------------------------------------------------

    #[test]
    fn parses_each_op() {
        assert_eq!(parse_request(r#"{"op":"info"}"#), Ok(Request::Info));
        assert_eq!(parse_request(r#"{"op":"metrics"}"#), Ok(Request::Metrics));
        assert_eq!(parse_request(r#"{"op":"latest"}"#), Ok(Request::Latest));
        assert_eq!(parse_request(r#"{"op":"read"}"#), Ok(everything()));
    }

    #[test]
    fn ignores_fields_it_does_not_know() {
        let request = r#"{"op":"read","colour":1,"extra":{"a":[1]}}"#;
        assert_eq!(parse_request(request), Ok(everything()));
    }

    #[test]
    fn accepts_the_version_it_speaks_and_refuses_others() {
        assert_eq!(parse_request(r#"{"v":1,"op":"info"}"#), Ok(Request::Info));
        assert_eq!(
            parse_request(r#"{"v":2,"op":"info"}"#),
            Err(RequestError::UnsupportedVersion(2))
        );
        for bad in [r#""1""#, "1.5", "-1", "null", "true"] {
            let request = format!(r#"{{"v":{bad},"op":"info"}}"#);
            assert_eq!(
                parse_request(&request),
                Err(RequestError::BadVersion),
                "{bad}"
            );
        }
    }

    #[test]
    fn refuses_what_is_not_a_request() {
        assert_eq!(parse_request("not json"), Err(RequestError::NotJson));
        assert_eq!(parse_request(""), Err(RequestError::NotJson));
        assert_eq!(parse_request(r#"{"op":"info""#), Err(RequestError::NotJson));
        for text in ["[]", "1", r#""info""#, "null"] {
            assert_eq!(
                parse_request(text),
                Err(RequestError::NotAnObject),
                "{text}"
            );
        }
        assert_eq!(parse_request("{}"), Err(RequestError::MissingOp));
        assert_eq!(
            parse_request(r#"{"op":7}"#),
            Err(RequestError::OpNotAString)
        );
        assert_eq!(
            parse_request(r#"{"op":"drop tables"}"#),
            Err(RequestError::UnknownOp("drop tables".to_owned()))
        );
    }

    #[test]
    fn an_unknown_op_message_is_short_however_long_the_op() {
        let request = format!(r#"{{"op":"{}"}}"#, "x".repeat(10_000));
        let message = parse_request(&request).unwrap_err().to_string();
        assert!(message.len() < 200, "{}", message.len());
    }

    #[test]
    fn a_read_can_carry_every_option() {
        let request = read(
            r#""from":1000,"to":9000,"metrics":["cpu_percent","load1"],"max_points":50,"extremes":true"#,
        );
        assert_eq!(
            request,
            Request::Read(ReadParams {
                from: Some(1000),
                to: Some(9000),
                metrics: Some(vec!["cpu_percent".to_owned(), "load1".to_owned()]),
                downsample: Some(Downsample::MaxPoints(50)),
                extremes: true,
            })
        );
        let by_step = read(r#""step_ms":60000"#);
        assert_eq!(
            by_step,
            Request::Read(ReadParams {
                downsample: Some(Downsample::StepMs(60_000)),
                ..ReadParams::default()
            })
        );
    }

    #[test]
    fn a_null_field_is_the_same_as_leaving_it_out() {
        let request = read(
            r#""from":null,"to":null,"metrics":null,"step_ms":null,"max_points":null,"extremes":null"#,
        );
        assert_eq!(request, everything());
    }

    #[test]
    fn times_must_be_whole_non_negative_numbers() {
        for field in ["from", "to"] {
            for bad in ["-1", "1.5", "1.0", r#""1000""#, "true", "[1]", "{}"] {
                let message = read_error(&format!(r#""{field}":{bad}"#));
                assert!(
                    message.contains(field) && message.contains("whole number"),
                    "{field} {bad}: {message}"
                );
            }
        }
    }

    #[test]
    fn from_may_not_be_after_to() {
        assert!(read_error(r#""from":2000,"to":1000"#).contains("must not be after"));
        assert_eq!(
            read(r#""from":1000,"to":1000"#),
            Request::Read(ReadParams {
                from: Some(1000),
                to: Some(1000),
                ..ReadParams::default()
            })
        );
    }

    #[test]
    fn the_metrics_field_must_be_a_non_empty_list_of_names() {
        for bad in [
            "[]",
            r#""cpu_percent""#,
            "[1]",
            r#"["a",2]"#,
            "{}",
            "true",
            "[null]",
        ] {
            let message = read_error(&format!(r#""metrics":{bad}"#));
            assert!(message.contains("metrics"), "{bad}: {message}");
        }
        let too_many = format!(
            r#""metrics":[{}]"#,
            vec![r#""x""#; MAX_METRIC_NAMES + 1].join(",")
        );
        assert!(read_error(&too_many).contains("too many"));
    }

    #[test]
    fn step_and_max_points_are_checked() {
        assert!(read_error(r#""step_ms":0"#).contains("at least 1"));
        assert!(read_error(r#""step_ms":-5"#).contains("step_ms"));
        assert!(read_error(r#""step_ms":"5s""#).contains("step_ms"));
        for bad in ["0", "1", "200001", "-1", "2.5", r#""9""#] {
            let message = read_error(&format!(r#""max_points":{bad}"#));
            assert!(message.contains("max_points"), "{bad}: {message}");
        }
        assert_eq!(MAX_POINTS, 200_000, "the message above says 200000");
        assert_eq!(
            read(r#""max_points":200000"#),
            Request::Read(ReadParams {
                downsample: Some(Downsample::MaxPoints(200_000)),
                ..ReadParams::default()
            })
        );
        assert!(matches!(read(r#""max_points":2"#), Request::Read(_)));
    }

    #[test]
    fn step_and_max_points_cannot_both_be_given() {
        assert!(read_error(r#""step_ms":1000,"max_points":10"#).contains("together"));
    }

    #[test]
    fn extremes_need_a_bucket_size_and_a_true_or_false() {
        assert!(read_error(r#""extremes":true"#).contains("needs"));
        assert!(read_error(r#""extremes":"yes","step_ms":1000"#).contains("true or false"));
        assert!(read_error(r#""extremes":1,"step_ms":1000"#).contains("true or false"));
        // False needs no bucket size.
        assert_eq!(read(r#""extremes":false"#), everything());
    }

    // ---- info, metrics and latest ----------------------------------------------

    #[test]
    fn info_describes_the_collector_and_its_store() {
        let shared = shared_with(&[(1000, &row(0.0)), (2000, &row(1.0))]);
        let response = respond_to(&Request::Info, &shared);
        assert_eq!(response["ok"], true);
        assert_eq!(response["v"], 1);
        assert_eq!(response["version"], CRATE_VERSION);
        assert_eq!(response["interval_ms"], 5000);
        assert_eq!(response["capacity"], 64);
        assert_eq!(response["len"], 2);
        assert_eq!(response["metrics"], 7);
    }

    #[test]
    fn metrics_lists_names_labels_and_units_in_record_order() {
        let response = respond_to(&Request::Metrics, &shared_with(&[]));
        let metrics = response["metrics"].as_array().unwrap();
        assert_eq!(metrics.len(), 7);
        assert_eq!(
            metrics[0],
            json!({"name":"cpu_percent","label":"CPU","unit":"%"})
        );
        assert_eq!(metrics[5]["name"], "net_eth0_rx_bytes_per_sec");
        assert_eq!(metrics[5]["unit"], "bytes/s");
    }

    #[test]
    fn latest_with_nothing_recorded_has_a_null_timestamp() {
        let response = respond_to(&Request::Latest, &shared_with(&[]));
        assert_eq!(response["ok"], true);
        assert!(response["timestamp"].is_null());
        assert_eq!(response["values"], json!({}));
    }

    #[test]
    fn latest_returns_the_newest_record_by_metric_name() {
        let shared = shared_with(&[(1000, &row(0.0)), (2000, &row(10.0))]);
        let response = respond_to(&Request::Latest, &shared);
        assert_eq!(response["timestamp"], 2000);
        assert_eq!(response["values"]["cpu_percent"], 10.0);
        assert_eq!(response["values"]["load1"], 11.0);
        assert_eq!(response["values"]["net_eth0_tx_bytes_per_sec"], 16.0);
    }

    #[test]
    fn a_value_that_could_not_be_measured_is_null() {
        let mut gappy = row(1.0);
        gappy[4] = f64::NAN;
        let shared = shared_with(&[(1000, &gappy)]);
        let response = respond_to(&Request::Latest, &shared);
        assert!(response["values"]["cpu_temp_celsius"].is_null());
        assert_eq!(response["values"]["cpu_percent"], 1.0);
    }

    // ---- a plain read ------------------------------------------------------------

    #[test]
    fn read_returns_every_record_as_aligned_columns() {
        let shared = shared_with(&[(1000, &row(0.0)), (2000, &row(10.0)), (3000, &row(20.0))]);
        let response = respond_to(&everything(), &shared);
        assert_eq!(response["ok"], true);
        assert_eq!(response["timestamps"], json!([1000, 2000, 3000]));
        assert!(response.get("step_ms").is_none(), "not downsampled");
        let series = response["series"].as_object().unwrap();
        assert_eq!(series.len(), 7);
        assert_eq!(series["cpu_percent"], json!([0.0, 10.0, 20.0]));
        assert_eq!(
            series["net_eth0_tx_bytes_per_sec"],
            json!([6.0, 16.0, 26.0])
        );
        for column in series.values() {
            assert_eq!(column.as_array().unwrap().len(), 3);
        }
    }

    #[test]
    fn read_of_an_empty_store_has_empty_columns() {
        let response = respond_to(&everything(), &shared_with(&[]));
        assert_eq!(response["timestamps"], json!([]));
        assert_eq!(response["series"]["cpu_percent"], json!([]));
    }

    #[test]
    fn read_writes_gaps_as_null_and_stays_valid_json() {
        let mut gappy = row(1.0);
        gappy[0] = f64::NAN;
        gappy[6] = f64::INFINITY;
        let shared = shared_with(&[(1000, &gappy)]);
        let response = respond_to(&everything(), &shared);
        assert_eq!(response["series"]["cpu_percent"], json!([null]));
        assert_eq!(
            response["series"]["net_eth0_tx_bytes_per_sec"],
            json!([null])
        );
    }

    #[test]
    fn read_after_the_store_has_wrapped_is_still_oldest_first() {
        let schema = Schema::new(&["eth0".to_owned()]);
        let store = Store::new(NonZeroUsize::new(4).unwrap(), schema.len()).unwrap();
        let shared = Shared::new(schema, Duration::from_secs(5), store);
        for i in 1..=6_u32 {
            shared
                .write()
                .push(u64::from(i) * 1000, &row(f64::from(i)))
                .unwrap();
        }
        let response = respond_to(&everything(), &shared);
        assert_eq!(response["timestamps"], json!([3000, 4000, 5000, 6000]));
        assert_eq!(
            response["series"]["cpu_percent"],
            json!([3.0, 4.0, 5.0, 6.0])
        );
    }

    // ---- ranges and subsets ------------------------------------------------------

    #[test]
    fn a_time_range_returns_only_those_records_inclusive() {
        let shared = steady(10); // 5000, 10000, ... 50000
        let response = respond_to(&read(r#""from":10000,"to":25000"#), &shared);
        assert_eq!(response["timestamps"], json!([10000, 15000, 20000, 25000]));
        assert_eq!(response["series"]["load1"], json!([2.0, 3.0, 4.0, 5.0]));
    }

    #[test]
    fn a_range_with_no_records_is_empty_not_an_error() {
        let response = respond_to(&read(r#""from":900000"#), &steady(10));
        assert_eq!(response["ok"], true);
        assert_eq!(response["timestamps"], json!([]));
        assert_eq!(response["series"]["load1"], json!([]));
    }

    #[test]
    fn a_subset_returns_only_the_named_metrics_in_the_order_given() {
        let shared = shared_with(&[(1000, &row(0.0)), (2000, &row(10.0))]);
        let response = respond_to(&read(r#""metrics":["load1","cpu_percent"]"#), &shared);
        let series = response["series"].as_object().unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series["load1"], json!([1.0, 11.0]));
        assert_eq!(series["cpu_percent"], json!([0.0, 10.0]));
    }

    #[test]
    fn a_metric_named_twice_appears_once() {
        let shared = shared_with(&[(1000, &row(0.0))]);
        let response = respond_to(&read(r#""metrics":["load1","load1"]"#), &shared);
        assert_eq!(response["series"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn an_unknown_metric_is_an_error_that_names_it() {
        let shared = shared_with(&[(1000, &row(0.0))]);
        let response = respond_to(&read(r#""metrics":["load1","no_such_metric"]"#), &shared);
        assert_eq!(response["ok"], false);
        let message = response["error"].as_str().unwrap();
        assert!(
            message.contains("no_such_metric") && message.contains("metrics"),
            "{message}"
        );
    }

    #[test]
    fn a_very_long_unknown_metric_name_is_cut_short_in_the_error() {
        let name = "m".repeat(10_000);
        let response = respond_to(
            &read(&format!(r#""metrics":["{name}"]"#)),
            &shared_with(&[]),
        );
        assert!(response["error"].as_str().unwrap().len() < 200);
    }

    // ---- downsampled reads ---------------------------------------------------------

    #[test]
    fn step_ms_returns_the_mean_per_bucket() {
        let shared = steady(10); // load1 = 1..=10 at 5000, 10000, ... 50000
        let response = respond_to(&read(r#""metrics":["load1"],"step_ms":20000"#), &shared);
        assert_eq!(response["ok"], true);
        assert_eq!(response["step_ms"], 20000);
        // Buckets start at 0, 20000, 40000: records 5,10,15 | 20..35 | 40,45,50.
        assert_eq!(response["timestamps"], json!([0, 20000, 40000]));
        assert_eq!(response["series"]["load1"], json!([2.0, 5.5, 9.0]));
        assert!(response.get("min").is_none() && response.get("max").is_none());
    }

    #[test]
    fn extremes_add_min_and_max_laid_out_like_series() {
        let shared = steady(10);
        let request = read(r#""metrics":["load1"],"step_ms":20000,"extremes":true"#);
        let response = respond_to(&request, &shared);
        assert_eq!(response["min"]["load1"], json!([1.0, 4.0, 8.0]));
        assert_eq!(response["max"]["load1"], json!([3.0, 7.0, 10.0]));
    }

    #[test]
    fn max_points_picks_a_step_and_reports_it() {
        let shared = steady(40); // 200 s of data
        let response = respond_to(&read(r#""max_points":10"#), &shared);
        let step = response["step_ms"].as_u64().unwrap();
        assert_eq!(step % 5000, 0, "a whole number of samples");
        let buckets = response["timestamps"].as_array().unwrap().len();
        assert!((2..=10).contains(&buckets), "{buckets} buckets");
        for column in response["series"].as_object().unwrap().values() {
            assert_eq!(column.as_array().unwrap().len(), buckets);
        }
    }

    #[test]
    fn max_points_at_least_the_record_count_keeps_every_record() {
        let response = respond_to(
            &read(r#""max_points":1000,"metrics":["load1"]"#),
            &steady(6),
        );
        assert_eq!(response["step_ms"], 5000);
        assert_eq!(
            response["series"]["load1"],
            json!([1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
        );
    }

    #[test]
    fn a_downsampled_range_only_covers_that_range() {
        let request = read(r#""from":20000,"to":35000,"step_ms":60000"#);
        let response = respond_to(&request, &steady(10));
        assert_eq!(response["timestamps"], json!([0]));
        assert_eq!(response["series"]["load1"], json!([5.5])); // records 4..=7
    }

    #[test]
    fn downsampling_a_range_with_nothing_in_it_is_empty() {
        let request = read(r#""from":900000,"max_points":10,"extremes":true"#);
        let response = respond_to(&request, &steady(4));
        assert_eq!(response["ok"], true);
        assert_eq!(response["timestamps"], json!([]));
        assert_eq!(response["min"]["load1"], json!([]));
    }

    #[test]
    fn a_bucket_with_no_usable_values_is_null() {
        let mut gappy = row(1.0);
        gappy[0] = f64::NAN;
        let shared = shared_with(&[(1000, &gappy), (2000, &gappy)]);
        let response = respond_to(&read(r#""step_ms":10000,"extremes":true"#), &shared);
        assert_eq!(response["series"]["cpu_percent"], json!([null]));
        assert_eq!(response["min"]["cpu_percent"], json!([null]));
        assert_eq!(response["series"]["load1"], json!([2.0]));
    }

    #[test]
    fn too_fine_a_step_over_too_long_a_range_is_refused_with_advice() {
        let shared = shared_with(&[(0, &row(0.0)), (1_000_000_000_000, &row(1.0))]);
        let response = respond_to(&read(r#""step_ms":1"#), &shared);
        assert_eq!(response["ok"], false);
        assert!(response["error"].as_str().unwrap().contains("max_points"));
        // The same range is fine with max_points, which picks a wide enough step.
        let response = respond_to(&read(r#""max_points":100"#), &shared);
        assert_eq!(response["ok"], true);
        assert!(response["timestamps"].as_array().unwrap().len() <= 100);
    }

    #[test]
    fn an_error_response_is_one_line_of_valid_json_however_odd_the_message() {
        let mut out = Vec::new();
        respond_error(&mut out, "bad \"thing\"\nwith a newline").unwrap();
        assert_eq!(newlines(&out), 1);
        let response: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(response["ok"], false);
        assert_eq!(response["v"], 1);
        assert_eq!(response["error"], "bad \"thing\"\nwith a newline");
    }
}
