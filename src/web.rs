//! `smweb`: a small, read-only web server for the metrics chart.
//!
//! It serves one page, the chart of any metric over a few time ranges, and
//! asks the collector (over its Unix socket) for what the chart needs. The
//! page, its script and styles, and the chart library are all built into the
//! binary, so there is nothing to install beside it.
//!
//! | Path | What |
//! |---|---|
//! | `/` | The chart as a complete page (with the `--title` as a heading) |
//! | `/fragment` | Just the chart's markup, for another web app to embed |
//! | `/data?metric=NAME&range=1h` | The chart's data as JSON |
//! | `/assets/NAME` | The script, styles, chart library and icon (a chimp holding a clapperboard) |
//! | `/healthz` | `ok` |
//!
//! Only `GET` and `HEAD` are served, and nothing is ever written: there is
//! no login, so listen only where whoever can connect may see the metrics
//! (a Unix socket with restricted permissions, or a trusted network).
//!
//! The server speaks just enough HTTP/1.1 for a browser or a reverse proxy:
//! one request per connection, no body, no keep-alive, no TLS.

use std::convert::Infallible;
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::VERSION;

/// The most connections served at once. Each has a thread.
pub const MAX_CONNECTIONS: usize = 32;

/// How long a connection may sit silent, or a client be slow to take a
/// response, before it is dropped.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// How long to wait for the collector before giving up on a request.
const COLLECTOR_TIMEOUT: Duration = Duration::from_secs(5);

/// The most the collector may reply with. (A downsampled reply is a few tens
/// of kilobytes.)
const MAX_REPLY_BYTES: u64 = 8 * 1024 * 1024;

/// The most a request's first line and headers may come to, in bytes.
const MAX_HEAD_BYTES: u64 = 16 * 1024;

/// The most header lines read; a browser sends a dozen or so.
const MAX_HEAD_LINES: usize = 100;

/// The time ranges the page offers, by name, in seconds.
pub const RANGES: [(&str, u64); 4] = [
    ("1h", 3600),
    ("6h", 6 * 3600),
    ("24h", 24 * 3600),
    ("7d", 7 * 24 * 3600),
];

/// A chart has nowhere near this many pixels, so more points would only cost
/// time and bandwidth: the collector averages down to (at most) this many.
const POINTS: u64 = 600;

const UNUSABLE: &str = "The metrics collector isn't giving usable data.";

// --- The collector -----------------------------------------------------------

/// Why the collector couldn't answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectorError {
    /// Nothing is listening, or it didn't answer in time.
    Unreachable,
    /// It answered with something that isn't a JSON object.
    BadReply,
    /// It answered `"ok": false`.
    Refused,
}

impl fmt::Display for CollectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unreachable => "the metrics collector isn't reachable",
            Self::BadReply => "the metrics collector gave an unusable reply",
            Self::Refused => "the metrics collector refused the request",
        })
    }
}

/// Something that answers the collector's JSON-lines requests: the real
/// socket, or a fake in tests.
pub trait Collector: Send + Sync {
    /// Sends one request and returns the reply, which is always a successful
    /// one (`"ok": true`).
    ///
    /// # Errors
    /// If the collector can't be reached, or doesn't give a usable answer.
    fn request(&self, request: &Value) -> Result<Value, CollectorError>;
}

/// The collector, reached over its Unix socket. Each request uses a fresh
/// connection: a page is a request or two every few seconds, and nothing is
/// kept open that could go stale when the collector restarts.
#[derive(Debug)]
pub struct SocketCollector {
    path: PathBuf,
}

impl SocketCollector {
    /// A collector listening on the socket at `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn exchange(&self, request: &Value) -> io::Result<Vec<u8>> {
        let stream = UnixStream::connect(&self.path)?;
        stream.set_read_timeout(Some(COLLECTOR_TIMEOUT))?;
        stream.set_write_timeout(Some(COLLECTOR_TIMEOUT))?;
        let mut line = serde_json::to_vec(request).map_err(io::Error::other)?;
        line.push(b'\n');
        (&stream).write_all(&line)?;
        let mut reply = Vec::new();
        BufReader::new(&stream)
            .take(MAX_REPLY_BYTES + 1)
            .read_until(b'\n', &mut reply)?;
        if u64::try_from(reply.len()).unwrap_or(u64::MAX) > MAX_REPLY_BYTES {
            return Err(io::Error::other("reply too large"));
        }
        Ok(reply)
    }
}

impl Collector for SocketCollector {
    fn request(&self, request: &Value) -> Result<Value, CollectorError> {
        let reply = self.exchange(request).map_err(|error| {
            crate::log(format_args!("collector request failed: {error}"));
            CollectorError::Unreachable
        })?;
        let Ok(reply) = serde_json::from_slice::<Value>(&reply) else {
            crate::log(format_args!("collector sent something that isn't JSON"));
            return Err(CollectorError::BadReply);
        };
        if reply.get("ok") != Some(&Value::Bool(true)) {
            crate::log(format_args!("collector refused a request: {reply}"));
            return Err(CollectorError::Refused);
        }
        Ok(reply)
    }
}

// --- The pages ---------------------------------------------------------------

/// How the pages are dressed: set from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// Where this server is mounted, for the links in its pages: empty when
    /// it has a host to itself, or something like `/metrics` when a reverse
    /// proxy serves it under that path. No trailing slash.
    pub base_path: String,
    /// The heading of the complete page.
    pub title: String,
}

struct Asset {
    name: &'static str,
    content_type: &'static str,
    body: &'static [u8],
}

const ASSETS: [Asset; 6] = [
    Asset {
        name: "metrics.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/metrics.js"),
    },
    Asset {
        name: "metrics.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../web/metrics.css"),
    },
    Asset {
        name: "page.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../web/page.css"),
    },
    Asset {
        name: "uPlot.iife.min.js",
        content_type: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/uPlot.iife.min.js"),
    },
    Asset {
        name: "favicon.svg",
        content_type: "image/svg+xml",
        body: include_bytes!("../web/favicon.svg"),
    },
    Asset {
        name: "uPlot.min.css",
        content_type: "text/css; charset=utf-8",
        body: include_bytes!("../web/uPlot.min.css"),
    },
];

/// Escapes `text` for use in HTML, in text or in a quoted attribute.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// The metrics from the collector's `metrics` reply as (name, label, unit),
/// leaving out any entry that isn't shaped as expected.
fn listing(reply: &Value) -> Vec<(&str, &str, &str)> {
    let Some(metrics) = reply.get("metrics").and_then(Value::as_array) else {
        return Vec::new();
    };
    metrics
        .iter()
        .filter_map(|m| {
            Some((
                m.get("name")?.as_str()?,
                m.get("label")?.as_str()?,
                m.get("unit").and_then(Value::as_str).unwrap_or(""),
            ))
        })
        .collect()
}

/// The chart and its pickers, ready to be placed in a page. The pickers are
/// filled in here, from the collector's list of metrics, so the page needs no
/// request to show them.
fn render_fragment(site: &Site, metrics: &Result<Value, CollectorError>) -> String {
    let base = escape(&site.base_path);
    let mut html = String::new();
    // Written to a String, which can't fail.
    write!(
        html,
        "<link rel=\"stylesheet\" href=\"{base}/assets/uPlot.min.css?v={VERSION}\">\n\
         <link rel=\"stylesheet\" href=\"{base}/assets/metrics.css?v={VERSION}\">\n"
    )
    .ok();
    match metrics {
        Err(error) => {
            write!(
                html,
                "<article>\n  <header>No metrics available</header>\n  \
                 <p>{}. It keeps this machine's history of CPU, memory, temperature and \
                 network use; check that the simple-metrics service is running.</p>\n\
                 </article>\n",
                escape(&capitalise(&error.to_string()))
            )
            .ok();
        }
        Ok(reply) => {
            let all = listing(reply);
            html.push_str(
                "<div class=\"metric-controls\">\n  <div class=\"field-row\">\n    \
                 <label for=\"metric-select\">Metric</label>\n    \
                 <select id=\"metric-select\" class=\"fit-content\">\n",
            );
            let (network, system): (Vec<_>, Vec<_>) = all
                .into_iter()
                .partition(|(name, _, _)| name.starts_with("net_"));
            for (heading, group) in [("System", system), ("Network", network)] {
                if group.is_empty() {
                    continue;
                }
                writeln!(html, "      <optgroup label=\"{heading}\">").ok();
                for (name, label, unit) in group {
                    let unit = if unit.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", escape(unit))
                    };
                    writeln!(
                        html,
                        "        <option value=\"{}\">{}{unit}</option>",
                        escape(name),
                        escape(label)
                    )
                    .ok();
                }
                html.push_str("      </optgroup>\n");
            }
            html.push_str(
                "    </select>\n  </div>\n  <div class=\"field-row\">\n    \
                 <label for=\"range-select\">Last</label>\n    \
                 <select id=\"range-select\" class=\"fit-content\">\n",
            );
            for (name, _) in RANGES {
                let selected = if name == "1h" { " selected" } else { "" };
                writeln!(
                    html,
                    "      <option value=\"{name}\"{selected}>{name}</option>"
                )
                .ok();
            }
            write!(
                html,
                "    </select>\n  </div>\n</div>\n\n\
                 <div id=\"chart\" class=\"chart\" role=\"img\" \
                 aria-label=\"Chart of the selected metric over time\" \
                 data-url=\"{base}/data\"></div>\n\
                 <p id=\"chart-status\" class=\"chart-status\" aria-live=\"polite\">Loading...</p>\n\
                 <script src=\"{base}/assets/uPlot.iife.min.js?v={VERSION}\"></script>\n\
                 <script src=\"{base}/assets/metrics.js?v={VERSION}\" defer></script>\n"
            )
            .ok();
        }
    }
    html
}

fn render_page(site: &Site, metrics: &Result<Value, CollectorError>) -> String {
    let base = escape(&site.base_path);
    let title = escape(&site.title);
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\">\n  \
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  \
         <title>{title}</title>\n  \
         <link rel=\"icon\" type=\"image/svg+xml\" href=\"{base}/assets/favicon.svg?v={VERSION}\">\n  \
         <link rel=\"stylesheet\" href=\"{base}/assets/page.css?v={VERSION}\">\n</head>\n<body>\n\
         <main>\n<h1>{title}</h1>\n{}</main>\n</body>\n</html>\n",
        render_fragment(site, metrics)
    )
}

// --- Routing -----------------------------------------------------------------

/// What to send back for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// The `Content-Type`.
    pub content_type: &'static str,
    /// The `Cache-Control`.
    pub cache_control: &'static str,
    /// The body.
    pub body: Vec<u8>,
}

impl Response {
    fn new(status: u16, content_type: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            content_type,
            cache_control: "no-store",
            body: body.into(),
        }
    }

    fn html(body: String) -> Self {
        Self::new(200, "text/html; charset=utf-8", body)
    }

    fn text(status: u16, body: &str) -> Self {
        Self::new(status, "text/plain; charset=utf-8", format!("{body}\n"))
    }

    fn json(status: u16, value: &Value) -> Self {
        Self::new(
            status,
            "application/json",
            serde_json::to_vec(value).unwrap_or_default(),
        )
    }

    fn json_error(status: u16, message: &str) -> Self {
        Self::json(status, &json!({ "error": message }))
    }
}

/// The value of query parameter `name`. Nothing is percent-decoded: the
/// values this server accepts never need it, and anything else is refused.
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

/// Whether `name` could be a metric's: the characters interface names come
/// with. Checked before a name goes anywhere near the collector.
fn valid_metric_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn data(collector: &dyn Collector, query: &str, now_ms: u64) -> Response {
    let metric = query_param(query, "metric").unwrap_or("");
    if !valid_metric_name(metric) {
        return Response::json_error(400, "no such metric");
    }
    let range = query_param(query, "range").unwrap_or("1h");
    let Some((_, seconds)) = RANGES.iter().find(|(name, _)| *name == range) else {
        let names: Vec<&str> = RANGES.iter().map(|(name, _)| *name).collect();
        return Response::json_error(400, &format!("range must be one of {}", names.join(", ")));
    };
    let unavailable = |error: CollectorError| {
        Response::json_error(503, &format!("{}.", capitalise(&error.to_string())))
    };
    let all = match collector.request(&json!({ "op": "metrics" })) {
        Ok(reply) => reply,
        Err(error) => return unavailable(error),
    };
    let Some(known) = all
        .get("metrics")
        .and_then(Value::as_array)
        .and_then(|list| list.iter().find(|m| m.get("name") == Some(&json!(metric))))
    else {
        return Response::json_error(404, "no such metric");
    };
    let reply = match collector.request(&json!({
        "op": "read",
        "metrics": [metric],
        "from": now_ms.saturating_sub(seconds * 1000),
        "max_points": POINTS,
        "extremes": true,
    })) {
        Ok(reply) => reply,
        Err(error) => return unavailable(error),
    };
    // A reply that isn't shaped as expected, e.g. from an older collector.
    let column = |key: &str| reply.get(key)?.get(metric).filter(|v| v.is_array());
    let (Some(avg), Some(min), Some(max), Some(timestamps), Some(step_ms)) = (
        column("series"),
        column("min"),
        column("max"),
        reply.get("timestamps").filter(|v| v.is_array()),
        reply.get("step_ms").filter(|v| v.is_u64()),
    ) else {
        crate::log(format_args!("unexpected reply from the collector: {reply}"));
        return Response::json_error(503, UNUSABLE);
    };
    Response::json(
        200,
        &json!({
            "metric": known,
            "range": range,
            "now": now_ms,
            "step_ms": step_ms,
            "timestamps": timestamps,
            "avg": avg,
            "min": min,
            "max": max,
        }),
    )
}

fn asset(name: &str) -> Response {
    match ASSETS.iter().find(|asset| asset.name == name) {
        Some(asset) => Response {
            status: 200,
            content_type: asset.content_type,
            // The pages ask for ?v=<version>, so a new release is a new URL.
            cache_control: "public, max-age=86400",
            body: asset.body.to_vec(),
        },
        None => Response::text(404, "not found"),
    }
}

/// Works out the response to `GET` (or `HEAD`) of `target`, a path with an
/// optional query. `now_ms` is the time, in milliseconds since the epoch.
#[must_use]
pub fn route(
    site: &Site,
    collector: &dyn Collector,
    method: &str,
    target: &str,
    now_ms: u64,
) -> Response {
    if method != "GET" && method != "HEAD" {
        return Response::text(405, "method not allowed");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" => Response::html(render_page(
            site,
            &collector.request(&json!({ "op": "metrics" })),
        )),
        "/fragment" => Response::html(render_fragment(
            site,
            &collector.request(&json!({ "op": "metrics" })),
        )),
        "/data" => data(collector, query, now_ms),
        // Browsers ask for this by default, whatever the page says.
        "/favicon.ico" => asset("favicon.svg"),
        "/healthz" => Response::text(200, "ok"),
        _ => match path.strip_prefix("/assets/") {
            Some(name) => asset(name),
            None => Response::text(404, "not found"),
        },
    }
}

// --- HTTP --------------------------------------------------------------------

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn write_response<W: Write>(
    writer: &mut W,
    response: &Response,
    head_only: bool,
) -> io::Result<()> {
    let allow = if response.status == 405 {
        "Allow: GET, HEAD\r\n"
    } else {
        ""
    };
    write!(
        writer,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: {}\r\n\
         X-Content-Type-Options: nosniff\r\n{allow}Connection: close\r\n\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len(),
        response.cache_control,
    )?;
    if !head_only {
        writer.write_all(&response.body)?;
    }
    writer.flush()
}

/// Reads one request (its first line and headers; there is never a body
/// that matters to a `GET`) from `reader` and writes the response to
/// `writer`. Closes after that: one request per connection.
///
/// # Errors
/// If reading or writing fails.
pub fn handle_connection<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    site: &Site,
    collector: &dyn Collector,
    now_ms: u64,
) -> io::Result<()> {
    let mut budget = MAX_HEAD_BYTES;
    let mut request_line = Vec::new();
    for lines in 0..=MAX_HEAD_LINES {
        let mut line = Vec::new();
        let read = reader
            .by_ref()
            .take(budget + 1)
            .read_until(b'\n', &mut line)?;
        let read_bytes = u64::try_from(read).unwrap_or(u64::MAX);
        if read_bytes > budget || lines == MAX_HEAD_LINES {
            let response = Response::text(431, "request headers too large");
            return write_response(&mut writer, &response, false);
        }
        budget -= read_bytes;
        if read == 0 {
            // The client went away without asking for anything.
            return Ok(());
        }
        if lines == 0 {
            request_line = line;
        } else if line == b"\r\n" || line == b"\n" {
            break;
        }
    }
    let bad = || Response::text(400, "bad request");
    let parts: Vec<&str> = match std::str::from_utf8(&request_line) {
        Ok(text) => text.split_ascii_whitespace().collect(),
        Err(_) => return write_response(&mut writer, &bad(), false),
    };
    let response = match parts.as_slice() {
        [method, target, version] if version.starts_with("HTTP/1.") && target.starts_with('/') => {
            let response = route(site, collector, method, target, now_ms);
            return write_response(&mut writer, &response, *method == "HEAD");
        }
        _ => bad(),
    };
    write_response(&mut writer, &response, false)
}

// --- Serving -----------------------------------------------------------------

/// What `smweb` listens on and how its pages are dressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The collector's Unix socket.
    pub collector_socket: PathBuf,
    /// A TCP address to serve HTTP on.
    pub http: Option<SocketAddr>,
    /// A Unix socket to serve HTTP on (for a reverse proxy on this machine).
    pub unix: Option<PathBuf>,
    /// The permissions of that Unix socket.
    pub unix_mode: u32,
    /// How the pages look.
    pub site: Site,
}

/// Why `smweb` couldn't start.
#[derive(Debug)]
pub struct WebError(String);

impl fmt::Display for WebError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WebError {}

struct Context {
    site: Site,
    collector: Box<dyn Collector>,
    active: AtomicUsize,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// A connection that can be given the same read and write time limit.
trait Connection: Send + 'static
where
    for<'a> &'a Self: Read + Write,
{
    fn limit_time(&self, limit: Duration) -> io::Result<()>;
}

impl Connection for TcpStream {
    fn limit_time(&self, limit: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(limit))?;
        self.set_write_timeout(Some(limit))
    }
}

impl Connection for UnixStream {
    fn limit_time(&self, limit: Duration) -> io::Result<()> {
        self.set_read_timeout(Some(limit))?;
        self.set_write_timeout(Some(limit))
    }
}

/// One of the [`MAX_CONNECTIONS`]; freed when dropped.
struct Slot(Arc<Context>);

impl Slot {
    fn acquire(context: &Arc<Context>) -> Option<Self> {
        if context.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            context.active.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(context)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn serve_connection<S: Connection>(stream: &S, context: &Context) -> io::Result<()>
where
    for<'a> &'a S: Read + Write,
{
    stream.limit_time(IO_TIMEOUT)?;
    handle_connection(
        BufReader::new(stream),
        stream,
        &context.site,
        &*context.collector,
        now_ms(),
    )
}

/// Accepts connections from `incoming` until it ends (which a listener's
/// never does), answering each on its own thread.
fn accept_all<S, I>(incoming: I, context: &Arc<Context>)
where
    S: Connection,
    for<'a> &'a S: Read + Write,
    I: Iterator<Item = io::Result<S>>,
{
    for accepted in incoming {
        let stream = match accepted {
            Ok(stream) => stream,
            Err(error) => {
                // Usually running out of file descriptors: back off, don't spin.
                crate::log(format_args!("accept failed: {error}"));
                thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        let Some(slot) = Slot::acquire(context) else {
            let busy = Response::text(503, "too many connections");
            write_response(&mut &stream, &busy, false).ok();
            continue;
        };
        let context = Arc::clone(context);
        let spawned = thread::Builder::new()
            .name("connection".to_owned())
            .spawn(move || {
                let _slot = slot; // released when this connection ends
                if let Err(error) = serve_connection(&stream, &context) {
                    // A client that goes away mid-response is routine.
                    if !matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe
                            | io::ErrorKind::ConnectionReset
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                    ) {
                        crate::log(format_args!("connection failed: {error}"));
                    }
                }
            });
        if let Err(error) = spawned {
            crate::log(format_args!("could not start a connection thread: {error}"));
        }
    }
}

/// Makes way for a fresh socket at `path`: removes a stale one left by an
/// earlier run, but never a live one, and never something that isn't a socket.
fn prepare_socket_path(path: &Path) -> Result<(), WebError> {
    let fail = |what: &str| WebError(format!("{}: {what}", path.display()));
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                return Err(fail("something is already listening there"));
            }
            fs::remove_file(path).map_err(|e| fail(&e.to_string()))
        }
        Ok(_) => Err(fail("exists and isn't a socket")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(fail(&error.to_string())),
    }
}

/// Starts serving. Once started this never returns; it only returns an
/// error, when it can't start.
///
/// # Errors
/// If a listener can't be set up or a thread can't be started.
pub fn run(config: Config) -> Result<Infallible, WebError> {
    let tcp = match config.http {
        Some(address) => {
            Some(TcpListener::bind(address).map_err(|e| WebError(format!("{address}: {e}")))?)
        }
        None => None,
    };
    let unix = match &config.unix {
        Some(path) => {
            prepare_socket_path(path)?;
            let listener = UnixListener::bind(path)
                .map_err(|e| WebError(format!("{}: {e}", path.display())))?;
            fs::set_permissions(path, fs::Permissions::from_mode(config.unix_mode))
                .map_err(|e| WebError(format!("{}: {e}", path.display())))?;
            Some(listener)
        }
        None => None,
    };
    let context = Arc::new(Context {
        site: config.site,
        collector: Box::new(SocketCollector::new(config.collector_socket.clone())),
        active: AtomicUsize::new(0),
    });
    let mut threads = Vec::new();
    let spawn = |name: &str, work: Box<dyn FnOnce() + Send>| {
        thread::Builder::new()
            .name(name.to_owned())
            .spawn(work)
            .map_err(|e| WebError(format!("could not start a thread: {e}")))
    };
    if let Some(listener) = tcp {
        let context = Arc::clone(&context);
        threads.push(spawn(
            "http",
            Box::new(move || accept_all(listener.incoming(), &context)),
        )?);
    }
    if let Some(listener) = unix {
        let context = Arc::clone(&context);
        threads.push(spawn(
            "unix",
            Box::new(move || accept_all(listener.incoming(), &context)),
        )?);
    }
    crate::log(format_args!(
        "smweb {VERSION} serving metrics from {}{}{}",
        config.collector_socket.display(),
        config
            .http
            .map_or_else(String::new, |a| format!(", on http://{a}")),
        config
            .unix
            .as_ref()
            .map_or_else(String::new, |p| format!(", on {}", p.display())),
    ));
    for thread in threads {
        thread.join().ok();
    }
    Err(WebError("the listeners stopped".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::Mutex;

    /// A collector that answers from a script and remembers what it was asked.
    struct Fake {
        replies: Mutex<Vec<Result<Value, CollectorError>>>,
        asked: Mutex<Vec<Value>>,
    }

    impl Fake {
        fn new(replies: Vec<Result<Value, CollectorError>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    impl Collector for Fake {
        fn request(&self, request: &Value) -> Result<Value, CollectorError> {
            self.asked.lock().unwrap().push(request.clone());
            self.replies.lock().unwrap().remove(0)
        }
    }

    fn listing_reply() -> Value {
        json!({"ok": true, "v": 1, "metrics": [
            {"name": "cpu_percent", "label": "CPU use", "unit": "%"},
            {"name": "load1", "label": "Load <1 min>", "unit": ""},
            {"name": "net_eth0_rx_bytes_per_sec", "label": "eth0 received", "unit": "bytes/s"},
        ]})
    }

    fn read_reply() -> Value {
        json!({"ok": true, "v": 1, "step_ms": 60000, "timestamps": [1000, 2000],
            "series": {"cpu_percent": [1.0, null]},
            "min": {"cpu_percent": [0.5, null]},
            "max": {"cpu_percent": [2.0, null]}})
    }

    /// A plausible time, later than the longest range.
    const NOW: u64 = 1_790_000_000_000;

    fn site() -> Site {
        Site {
            base_path: String::new(),
            title: "my server <metrics>".to_owned(),
        }
    }

    fn get(fake: &Fake, target: &str) -> Response {
        route(&site(), fake, "GET", target, NOW)
    }

    fn body(response: &Response) -> String {
        String::from_utf8(response.body.clone()).unwrap()
    }

    #[test]
    fn the_page_lists_the_metrics_grouped_with_everything_escaped() {
        let fake = Fake::new(vec![Ok(listing_reply())]);
        let response = get(&fake, "/");
        assert_eq!(response.status, 200);
        let html = body(&response);
        assert!(
            html.contains("<h1>my server &lt;metrics&gt;</h1>"),
            "{html}"
        );
        assert!(html.contains("<optgroup label=\"System\">"));
        assert!(html.contains("<optgroup label=\"Network\">"));
        assert!(html.contains("<option value=\"cpu_percent\">CPU use (%)</option>"));
        assert!(html.contains("<option value=\"load1\">Load &lt;1 min&gt;</option>"));
        assert!(html.contains("<option value=\"1h\" selected>1h</option>"));
        assert!(html.contains("data-url=\"/data\""));
        assert!(html.contains("href=\"/assets/page.css?v="));
        assert!(
            html.contains(
                "<link rel=\"icon\" type=\"image/svg+xml\" href=\"/assets/favicon.svg?v="
            ),
            "{html}"
        );
    }

    #[test]
    fn the_icon_is_an_svg_served_where_the_page_says_and_where_browsers_look() {
        let fake = Fake::new(vec![]);
        for path in ["/assets/favicon.svg", "/favicon.ico"] {
            let response = get(&fake, path);
            assert_eq!(response.status, 200, "{path}");
            assert_eq!(response.content_type, "image/svg+xml", "{path}");
            assert!(body(&response).starts_with("<svg "), "{path}");
        }
    }

    #[test]
    fn the_fragment_has_no_page_around_it() {
        let fake = Fake::new(vec![Ok(listing_reply())]);
        let html = body(&get(&fake, "/fragment"));
        assert!(!html.contains("<html"));
        assert!(!html.contains("page.css"));
        assert!(html.contains("id=\"chart\""));
    }

    #[test]
    fn links_in_the_pages_follow_the_base_path() {
        let fake = Fake::new(vec![Ok(listing_reply())]);
        let site = Site {
            base_path: "/metrics".to_owned(),
            title: String::new(),
        };
        let html = body(&route(&site, &fake, "GET", "/fragment", 0));
        assert!(html.contains("data-url=\"/metrics/data\""));
        assert!(html.contains("src=\"/metrics/assets/metrics.js?v="));
    }

    #[test]
    fn an_unreachable_collector_shows_a_message_not_the_controls() {
        let fake = Fake::new(vec![Err(CollectorError::Unreachable)]);
        let response = get(&fake, "/");
        assert_eq!(response.status, 200);
        let html = body(&response);
        assert!(html.contains("No metrics available"));
        assert!(html.contains("The metrics collector isn&#39;t reachable."));
        assert!(!html.contains("metric-select"));
    }

    #[test]
    fn data_asks_for_the_range_downsampled_with_extremes() {
        let fake = Fake::new(vec![Ok(listing_reply()), Ok(read_reply())]);
        let response = get(&fake, "/data?metric=cpu_percent&range=6h");
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
        assert_eq!(response.cache_control, "no-store");
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["metric"]["label"], "CPU use");
        assert_eq!(value["range"], "6h");
        assert_eq!(value["now"], NOW);
        assert_eq!(value["step_ms"], 60000);
        assert_eq!(value["avg"], json!([1.0, null]));
        assert_eq!(value["min"], json!([0.5, null]));
        assert_eq!(value["max"], json!([2.0, null]));
        let asked = fake.asked.lock().unwrap();
        assert_eq!(asked[1]["op"], "read");
        assert_eq!(asked[1]["metrics"], json!(["cpu_percent"]));
        assert_eq!(asked[1]["from"], NOW - 6 * 3600 * 1000);
        assert_eq!(asked[1]["max_points"], 600);
        assert_eq!(asked[1]["extremes"], true);
    }

    #[test]
    fn data_refuses_bad_input_without_asking_the_collector() {
        let fake = Fake::new(vec![]);
        for target in [
            "/data",
            "/data?metric=",
            "/data?metric=a%20b",
            "/data?metric=a/b",
            "/data?metric=cpu_percent&range=1y",
        ] {
            assert_eq!(get(&fake, target).status, 400, "{target}");
        }
        assert!(fake.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn data_for_an_unknown_metric_is_a_404() {
        let fake = Fake::new(vec![Ok(listing_reply())]);
        assert_eq!(get(&fake, "/data?metric=nope").status, 404);
    }

    #[test]
    fn data_reports_an_unavailable_collector_as_503() {
        let fake = Fake::new(vec![Err(CollectorError::Refused)]);
        let response = get(&fake, "/data?metric=cpu_percent");
        assert_eq!(response.status, 503);
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(value["error"], "The metrics collector refused the request.");
    }

    #[test]
    fn data_reports_a_reply_of_the_wrong_shape_as_503() {
        let fake = Fake::new(vec![
            Ok(listing_reply()),
            Ok(json!({"ok": true, "timestamps": [], "series": {}})),
        ]);
        let response = get(&fake, "/data?metric=cpu_percent");
        assert_eq!(response.status, 503);
        assert!(body(&response).contains("usable data"));
    }

    #[test]
    fn assets_are_served_with_their_type_and_cacheable() {
        let fake = Fake::new(vec![]);
        let js = get(&fake, "/assets/metrics.js?v=1");
        assert_eq!(js.status, 200);
        assert_eq!(js.content_type, "text/javascript; charset=utf-8");
        assert!(js.cache_control.contains("max-age"));
        assert!(body(&js).contains("uPlot"));
        assert_eq!(
            get(&fake, "/assets/uPlot.min.css").content_type,
            "text/css; charset=utf-8"
        );
        assert_eq!(get(&fake, "/assets/nope.js").status, 404);
        assert_eq!(get(&fake, "/assets/../Cargo.toml").status, 404);
    }

    #[test]
    fn only_get_and_head_are_served() {
        let fake = Fake::new(vec![]);
        for method in ["POST", "PUT", "DELETE"] {
            assert_eq!(route(&site(), &fake, method, "/", 0).status, 405);
        }
        assert_eq!(get(&fake, "/nowhere").status, 404);
        assert_eq!(body(&get(&fake, "/healthz")), "ok\n");
    }

    fn exchange(request: &[u8], fake: &Fake) -> String {
        let mut out = Vec::new();
        handle_connection(Cursor::new(request.to_vec()), &mut out, &site(), fake, 0).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_request_gets_a_complete_http_response() {
        let fake = Fake::new(vec![]);
        let reply = exchange(
            b"GET /healthz HTTP/1.1\r\nHost: x\r\nAccept: */*\r\n\r\n",
            &fake,
        );
        assert!(reply.starts_with("HTTP/1.1 200 OK\r\n"), "{reply}");
        assert!(reply.contains("Content-Length: 3\r\n"));
        assert!(reply.contains("X-Content-Type-Options: nosniff\r\n"));
        assert!(reply.contains("Connection: close\r\n"));
        assert!(reply.ends_with("\r\n\r\nok\n"));
    }

    #[test]
    fn head_has_the_length_but_no_body() {
        let fake = Fake::new(vec![]);
        let reply = exchange(b"HEAD /healthz HTTP/1.1\r\n\r\n", &fake);
        assert!(reply.contains("Content-Length: 3\r\n"));
        assert!(reply.ends_with("\r\n\r\n"));
    }

    #[test]
    fn a_method_that_isnt_served_says_which_are() {
        let fake = Fake::new(vec![]);
        let reply = exchange(b"POST / HTTP/1.1\r\n\r\n", &fake);
        assert!(reply.starts_with("HTTP/1.1 405 "), "{reply}");
        assert!(reply.contains("Allow: GET, HEAD\r\n"));
    }

    #[test]
    fn nonsense_is_a_400_and_silence_is_ignored() {
        let fake = Fake::new(vec![]);
        for request in [
            &b"hello\r\n\r\n"[..],
            b"GET / FTP/9\r\n\r\n",
            b"GET http://x/ HTTP/1.1\r\n\r\n",
            b"GET /\xff HTTP/1.1\r\n\r\n",
        ] {
            assert!(exchange(request, &fake).starts_with("HTTP/1.1 400 "));
        }
        assert_eq!(exchange(b"", &fake), "");
    }

    #[test]
    fn an_oversized_head_is_refused() {
        let fake = Fake::new(vec![]);
        let long = format!("GET /{} HTTP/1.1\r\n\r\n", "a".repeat(20_000));
        assert!(exchange(long.as_bytes(), &fake).starts_with("HTTP/1.1 431 "));
        let many = format!("GET / HTTP/1.1\r\n{}\r\n", "X: y\r\n".repeat(200));
        assert!(exchange(many.as_bytes(), &fake).starts_with("HTTP/1.1 431 "));
    }

    #[test]
    fn metric_names_are_checked_strictly() {
        assert!(valid_metric_name("net_br-ap_rx_bytes_per_sec"));
        assert!(valid_metric_name("a.b"));
        assert!(!valid_metric_name(""));
        assert!(!valid_metric_name(&"a".repeat(65)));
        assert!(!valid_metric_name("a b"));
        assert!(!valid_metric_name("a/b"));
        assert!(!valid_metric_name("é"));
    }
}
