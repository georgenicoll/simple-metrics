//! The socket protocol: JSON lines.
//!
//! A client sends one JSON object per line and gets exactly one line of JSON
//! back for each. A connection can carry any number of requests.
//!
//! ```text
//! -> {"op":"info"}
//! <- {"ok":true,"v":1,"version":"0.1.0","interval_ms":5000,"capacity":120960,"len":42,"metrics":15}
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
//! | `read` | `timestamps` (oldest first) and `series`: `{name: [number or null, ...]}`, each the same length as `timestamps` |
//!
//! A value that couldn't be measured is `null`.

use std::fmt;
use std::io::{self, Write};

use serde_json::{Map, Value, json};

use crate::VERSION as CRATE_VERSION;
use crate::state::Shared;

/// The protocol version this server speaks.
pub const VERSION: u32 = 1;

/// The longest request line accepted, in bytes.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// What a client can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Facts about the collector and its store.
    Info,
    /// The list of metrics in every record.
    Metrics,
    /// The newest record.
    Latest,
    /// Every record held.
    Read,
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
        }
    }
}

impl std::error::Error for RequestError {}

/// Turns one request line into a [`Request`].
///
/// # Errors
/// If the line isn't a JSON object with a known `op` (and, if it has a `v`,
/// the supported one).
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
        "read" => Ok(Request::Read),
        other => Err(RequestError::UnknownOp(other.to_owned())),
    }
}

/// Writes the one-line response to `request`, and flushes.
///
/// # Errors
/// If writing fails, for example because the client went away.
pub fn respond<W: Write>(w: &mut W, request: Request, shared: &Shared) -> io::Result<()> {
    match request {
        Request::Info => respond_info(w, shared)?,
        Request::Metrics => respond_metrics(w, shared)?,
        Request::Latest => respond_latest(w, shared)?,
        Request::Read => respond_read(w, shared)?,
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
    let interval_ms = u64::try_from(shared.interval.as_millis()).unwrap_or(u64::MAX);
    serde_json::to_writer(
        w,
        &json!({
            "ok": true,
            "v": VERSION,
            "version": CRATE_VERSION,
            "interval_ms": interval_ms,
            "capacity": capacity,
            "len": len,
            "metrics": shared.schema.len(),
        }),
    )?;
    Ok(())
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

fn respond_read<W: Write>(w: &mut W, shared: &Shared) -> io::Result<()> {
    // Copy everything out and let go of the lock: writing to a slow client
    // must never hold up the sampler.
    let snapshot = shared.read().snapshot();

    // Written piece by piece, not built as one JSON value first: a full
    // store is over a million numbers, and a tree of them costs far more
    // memory than the store does.
    write!(w, "{{\"ok\":true,\"v\":{VERSION},\"timestamps\":[")?;
    for (i, timestamp) in snapshot.timestamps.iter().enumerate() {
        if i > 0 {
            w.write_all(b",")?;
        }
        write!(w, "{timestamp}")?;
    }
    w.write_all(b"],\"series\":{")?;
    for (i, (metric, column)) in shared
        .schema
        .metrics()
        .iter()
        .zip(&snapshot.columns)
        .enumerate()
    {
        if i > 0 {
            w.write_all(b",")?;
        }
        serde_json::to_writer(&mut *w, &metric.name)?;
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
    w.write_all(b"}}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::time::Duration;

    use super::*;
    use crate::metrics::Schema;
    use crate::store::Store;

    fn shared_with(rows: &[(u64, &[f64])]) -> Shared {
        let schema = Schema::new(&["eth0".to_owned()]);
        let store = Store::new(NonZeroUsize::new(4).unwrap(), schema.len()).unwrap();
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

    fn respond_to(request: Request, shared: &Shared) -> Value {
        let mut out = Vec::new();
        respond(&mut out, request, shared).unwrap();
        assert_eq!(out.last(), Some(&b'\n'));
        assert_eq!(newlines(&out), 1, "one line only");
        serde_json::from_slice(&out).unwrap()
    }

    #[test]
    fn parses_each_op() {
        assert_eq!(parse_request(r#"{"op":"info"}"#), Ok(Request::Info));
        assert_eq!(parse_request(r#"{"op":"metrics"}"#), Ok(Request::Metrics));
        assert_eq!(parse_request(r#"{"op":"latest"}"#), Ok(Request::Latest));
        assert_eq!(parse_request(r#"{"op":"read"}"#), Ok(Request::Read));
    }

    #[test]
    fn ignores_fields_it_does_not_know() {
        let request = r#"{"op":"read","from":1,"metrics":["x"],"extra":{"a":[1]}}"#;
        assert_eq!(parse_request(request), Ok(Request::Read));
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
    fn info_describes_the_collector_and_its_store() {
        let shared = shared_with(&[(1000, &row(0.0)), (2000, &row(1.0))]);
        let response = respond_to(Request::Info, &shared);
        assert_eq!(response["ok"], true);
        assert_eq!(response["v"], 1);
        assert_eq!(response["version"], CRATE_VERSION);
        assert_eq!(response["interval_ms"], 5000);
        assert_eq!(response["capacity"], 4);
        assert_eq!(response["len"], 2);
        assert_eq!(response["metrics"], 7);
    }

    #[test]
    fn metrics_lists_names_labels_and_units_in_record_order() {
        let response = respond_to(Request::Metrics, &shared_with(&[]));
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
        let response = respond_to(Request::Latest, &shared_with(&[]));
        assert_eq!(response["ok"], true);
        assert!(response["timestamp"].is_null());
        assert_eq!(response["values"], json!({}));
    }

    #[test]
    fn latest_returns_the_newest_record_by_metric_name() {
        let shared = shared_with(&[(1000, &row(0.0)), (2000, &row(10.0))]);
        let response = respond_to(Request::Latest, &shared);
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
        let response = respond_to(Request::Latest, &shared);
        assert!(response["values"]["cpu_temp_celsius"].is_null());
        assert_eq!(response["values"]["cpu_percent"], 1.0);
    }

    #[test]
    fn read_returns_every_record_as_aligned_columns() {
        let shared = shared_with(&[(1000, &row(0.0)), (2000, &row(10.0)), (3000, &row(20.0))]);
        let response = respond_to(Request::Read, &shared);
        assert_eq!(response["ok"], true);
        assert_eq!(response["timestamps"], json!([1000, 2000, 3000]));
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
        let response = respond_to(Request::Read, &shared_with(&[]));
        assert_eq!(response["timestamps"], json!([]));
        assert_eq!(response["series"]["cpu_percent"], json!([]));
    }

    #[test]
    fn read_writes_gaps_as_null_and_stays_valid_json() {
        let mut gappy = row(1.0);
        gappy[0] = f64::NAN;
        gappy[6] = f64::INFINITY;
        let shared = shared_with(&[(1000, &gappy)]);
        let response = respond_to(Request::Read, &shared);
        assert_eq!(response["series"]["cpu_percent"], json!([null]));
        assert_eq!(
            response["series"]["net_eth0_tx_bytes_per_sec"],
            json!([null])
        );
    }

    #[test]
    fn read_after_the_store_has_wrapped_is_still_oldest_first() {
        let rows: Vec<(u64, Vec<f64>)> = (1..=6_u32)
            .map(|i| (u64::from(i) * 1000, row(f64::from(i))))
            .collect();
        let borrowed: Vec<(u64, &[f64])> = rows.iter().map(|(t, r)| (*t, r.as_slice())).collect();
        let response = respond_to(Request::Read, &shared_with(&borrowed));
        assert_eq!(response["timestamps"], json!([3000, 4000, 5000, 6000]));
        assert_eq!(
            response["series"]["cpu_percent"],
            json!([3.0, 4.0, 5.0, 6.0])
        );
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
