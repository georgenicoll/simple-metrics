//! Accepting connections on the Unix socket and answering requests.

use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::protocol::{MAX_REQUEST_BYTES, parse_request, respond, respond_error};
use crate::state::Shared;

/// The most connections served at once. Each has a thread, and this is a
/// tool for one web app on the same machine: more than this is a fault.
pub const MAX_CONNECTIONS: usize = 16;

/// How long a connection may sit silent, or a client be slow to take a
/// response, before the connection is dropped.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Accepts connections on `listener` for ever, answering each on its own
/// thread.
pub fn serve(listener: &UnixListener, shared: &Arc<Shared>) -> ! {
    let active = Arc::new(AtomicUsize::new(0));
    loop {
        match listener.accept() {
            Ok((stream, _)) => accept(stream, shared, &active),
            Err(error) => {
                // Usually running out of file descriptors: back off, don't spin.
                crate::log(format_args!("accept failed: {error}"));
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn accept(stream: UnixStream, shared: &Arc<Shared>, active: &Arc<AtomicUsize>) {
    let Some(slot) = Slot::acquire(active) else {
        let mut writer = &stream;
        respond_error(&mut writer, "too many connections").ok();
        return;
    };
    let shared = Arc::clone(shared);
    let spawned = thread::Builder::new()
        .name("connection".to_owned())
        .spawn(move || {
            let _slot = slot; // released when this connection ends
            if let Err(error) = serve_connection(&stream, &shared) {
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

fn serve_connection(stream: &UnixStream, shared: &Shared) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    handle_connection(BufReader::new(stream), BufWriter::new(stream), shared)
}

/// One of the [`MAX_CONNECTIONS`]; freed when dropped.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn acquire(active: &Arc<AtomicUsize>) -> Option<Self> {
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(Self(Arc::clone(active)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Reads requests from `reader` and writes a response to `writer` for each,
/// until the client closes the connection or asks for something too long.
///
/// # Errors
/// If reading or writing fails.
pub fn handle_connection<R: BufRead, W: Write>(
    mut reader: R,
    mut writer: W,
    shared: &Shared,
) -> io::Result<()> {
    let mut line = Vec::new();
    loop {
        match read_line(&mut reader, &mut line)? {
            Line::Eof => return Ok(()),
            Line::TooLong => {
                respond_error(&mut writer, "request too long")?;
                return Ok(());
            }
            Line::Complete => {}
        }
        let Ok(text) = std::str::from_utf8(&line) else {
            respond_error(&mut writer, "request is not valid UTF-8")?;
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match parse_request(text) {
            Ok(request) => respond(&mut writer, &request, shared)?,
            Err(error) => respond_error(&mut writer, &error.to_string())?,
        }
    }
}

enum Line {
    Complete,
    TooLong,
    Eof,
}

/// Reads one line into `line`, never buffering more than
/// [`MAX_REQUEST_BYTES`]: a client can't make the server hold an unbounded
/// amount by never sending a newline.
fn read_line<R: BufRead>(reader: &mut R, line: &mut Vec<u8>) -> io::Result<Line> {
    line.clear();
    let limit = u64::try_from(MAX_REQUEST_BYTES).unwrap_or(u64::MAX);
    let read = reader.by_ref().take(limit + 1).read_until(b'\n', line)?;
    if read == 0 {
        Ok(Line::Eof)
    } else if line.last() != Some(&b'\n') && line.len() > MAX_REQUEST_BYTES {
        Ok(Line::TooLong)
    } else {
        Ok(Line::Complete)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::num::NonZeroUsize;

    use super::*;
    use crate::metrics::Schema;
    use crate::store::Store;

    fn shared() -> Shared {
        let schema = Schema::new(&["eth0".to_owned()]);
        let store = Store::new(NonZeroUsize::new(4).unwrap(), schema.len()).unwrap();
        let shared = Shared::new(schema, Duration::from_secs(5), store);
        shared.write().push(1000, &[1.0; 7]).unwrap();
        shared
    }

    /// Feeds `input` to a connection and returns each line of its output.
    fn converse(input: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        handle_connection(Cursor::new(input.to_vec()), &mut out, &shared()).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(ToOwned::to_owned)
            .collect()
    }

    fn ok(line: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(line).unwrap()["ok"] == true
    }

    #[test]
    fn answers_each_request_on_a_connection_in_order() {
        let replies = converse(b"{\"op\":\"info\"}\n{\"op\":\"latest\"}\n{\"op\":\"metrics\"}\n");
        assert_eq!(replies.len(), 3);
        assert!(replies.iter().all(|r| ok(r)));
        assert!(replies[0].contains("\"interval_ms\""));
        assert!(replies[1].contains("\"timestamp\""));
        assert!(replies[2].contains("\"cpu_percent\""));
    }

    #[test]
    fn a_final_request_with_no_newline_is_still_answered() {
        let replies = converse(b"{\"op\":\"info\"}");
        assert_eq!(replies.len(), 1);
        assert!(ok(&replies[0]));
    }

    #[test]
    fn a_bad_request_gets_an_error_and_the_connection_carries_on() {
        let replies =
            converse(b"nonsense\n{\"op\":\"info\"}\n{\"op\":\"nope\"}\n{\"op\":\"info\"}\n");
        assert_eq!(replies.len(), 4);
        let flags: Vec<bool> = replies.iter().map(|r| ok(r)).collect();
        assert_eq!(flags, [false, true, false, true]);
        assert!(replies[0].contains("not valid JSON"));
        assert!(replies[2].contains("unknown op"));
    }

    #[test]
    fn blank_lines_are_ignored() {
        let replies = converse(b"\n\n   \r\n{\"op\":\"info\"}\r\n\n");
        assert_eq!(replies.len(), 1);
    }

    #[test]
    fn invalid_utf8_is_an_error_not_a_crash() {
        let replies = converse(b"\xff\xfe\n{\"op\":\"info\"}\n");
        assert_eq!(replies.len(), 2);
        assert!(replies[0].contains("UTF-8"));
        assert!(ok(&replies[1]));
    }

    #[test]
    fn a_request_that_is_too_long_is_refused_and_the_connection_closed() {
        let mut input = vec![b'x'; MAX_REQUEST_BYTES + 10];
        input.extend_from_slice(b"\n{\"op\":\"info\"}\n");
        let replies = converse(&input);
        assert_eq!(replies.len(), 1);
        assert!(replies[0].contains("request too long"));
    }

    #[test]
    fn a_request_at_exactly_the_limit_is_accepted() {
        let padding = MAX_REQUEST_BYTES - "{\"op\":\"info\",\"p\":\"\"}".len();
        let request = format!(
            "{{\"op\":\"info\",\"p\":\"{}\"}}\n",
            "x".repeat(padding - 1)
        );
        assert!(request.len() <= MAX_REQUEST_BYTES + 1);
        let replies = converse(request.as_bytes());
        assert_eq!(replies.len(), 1);
        assert!(ok(&replies[0]), "{}", replies[0]);
    }

    #[test]
    fn silence_ends_the_conversation_quietly() {
        assert!(converse(b"").is_empty());
    }

    #[test]
    fn at_most_max_connections_slots_are_handed_out() {
        let active = Arc::new(AtomicUsize::new(0));
        let slots: Vec<Slot> = (0..MAX_CONNECTIONS)
            .map(|_| Slot::acquire(&active).unwrap())
            .collect();
        assert!(Slot::acquire(&active).is_none());
        assert_eq!(active.load(Ordering::SeqCst), MAX_CONNECTIONS);
        drop(slots);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(Slot::acquire(&active).is_some());
    }
}
