//! DAP base protocol framing: `Content-Length` headers followed by a JSON body (the same framing
//! as LSP; this is a copy of `codecraft-lsp`'s reader because L0 crates share no code).
//!
//! The reader is defensive: header names are case-insensitive, unknown headers (`Content-Type`)
//! are ignored, garbage between messages is skipped (and reported) until the next header, and a
//! message larger than the limit is read past and dropped. Every outcome consumes input, so a
//! misbehaving adapter can produce errors but never a busy loop.

use std::io::{self, BufRead, Read, Write};

use serde_json::Value;

/// The largest message body we accept (big `variables` replies are a few MiB at most).
pub(crate) const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
/// A header line longer than this is garbage.
const MAX_HEADER_LINE: usize = 8 * 1024;
/// More header lines than this in one block is garbage.
const MAX_HEADER_LINES: usize = 64;
/// Bodies are read into a buffer that starts at most this big and grows as bytes arrive.
const INITIAL_BODY_CAPACITY: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum Incoming {
    Message(Value),
    /// A framing or JSON problem. The reader has moved past it and can be called again.
    Malformed(String),
    /// The stream ended cleanly between messages.
    Eof,
    /// The underlying stream failed; the connection is unusable.
    Failed(io::Error),
}

enum Header {
    Length { len: u64, skipped: usize },
    Problem(String),
    Eof,
    Failed(io::Error),
}

enum HeaderLine {
    ContentLength(std::result::Result<u64, String>),
    Other,
    Garbage,
}

/// Reads framed messages from a buffered stream.
pub(crate) struct FrameReader<R> {
    inner: R,
    max_size: usize,
    /// A message found right after skipped garbage, returned on the next call.
    deferred: Option<Incoming>,
}

impl<R: BufRead> FrameReader<R> {
    pub(crate) fn new(inner: R) -> Self {
        Self::with_limit(inner, MAX_MESSAGE_SIZE)
    }

    pub(crate) fn with_limit(inner: R, max_size: usize) -> Self {
        Self { inner, max_size, deferred: None }
    }

    /// The next message (or problem). Blocks until a whole message has arrived.
    pub(crate) fn next_message(&mut self) -> Incoming {
        if let Some(deferred) = self.deferred.take() {
            return deferred;
        }
        let (len, skipped) = match self.read_header() {
            Header::Length { len, skipped } => (len, skipped),
            Header::Problem(problem) => return Incoming::Malformed(problem),
            Header::Eof => return Incoming::Eof,
            Header::Failed(e) => return Incoming::Failed(e),
        };
        let body = self.read_body(len);
        if skipped > 0 {
            self.deferred = Some(body);
            return Incoming::Malformed(skipped_message(skipped));
        }
        body
    }

    fn read_header(&mut self) -> Header {
        let mut content_length = None;
        let mut problem = None;
        let mut skipped = 0usize;
        let mut lines = 0usize;
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = match self.inner.by_ref().take(MAX_HEADER_LINE as u64).read_until(b'\n', &mut line) {
                Ok(n) => n,
                Err(e) => return Header::Failed(e),
            };
            if n == 0 {
                return if lines == 0 {
                    Header::Eof
                } else {
                    Header::Problem("the stream ended inside message headers".to_owned())
                };
            }
            if n >= MAX_HEADER_LINE && line.last() != Some(&b'\n') {
                // An endless line: skip it in chunks.
                skipped = skipped.saturating_add(n);
                lines += 1;
            } else {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end_matches(['\r', '\n']);
                if text.is_empty() {
                    if lines == 0 {
                        // Stray blank lines between messages are harmless.
                        continue;
                    }
                    break;
                }
                lines += 1;
                let parsed = match parse_header_line(text) {
                    header @ HeaderLine::ContentLength(_) => header,
                    other => match find_content_length(text) {
                        // Resync: a header can follow garbage on the same line, e.g. after a body
                        // that was longer than its Content-Length claimed.
                        Some((offset, header)) => {
                            skipped = skipped.saturating_add(offset);
                            header
                        }
                        None => {
                            if matches!(other, HeaderLine::Garbage) {
                                skipped = skipped.saturating_add(n);
                            }
                            other
                        }
                    },
                };
                match parsed {
                    HeaderLine::ContentLength(Ok(len)) => content_length = Some(len),
                    HeaderLine::ContentLength(Err(value)) => {
                        problem = Some(format!("invalid Content-Length header {value:?}"));
                    }
                    HeaderLine::Other | HeaderLine::Garbage => {}
                }
            }
            if lines > MAX_HEADER_LINES {
                return Header::Problem(match content_length {
                    Some(_) => format!("more than {MAX_HEADER_LINES} header lines"),
                    None => skipped_message(skipped),
                });
            }
        }
        if let Some(problem) = problem {
            return Header::Problem(problem);
        }
        match content_length {
            Some(len) => Header::Length { len, skipped },
            None if skipped > 0 => Header::Problem(skipped_message(skipped)),
            None => Header::Problem("message header without Content-Length".to_owned()),
        }
    }

    fn read_body(&mut self, len: u64) -> Incoming {
        let fits = usize::try_from(len).ok().filter(|len| *len <= self.max_size);
        let Some(size) = fits else {
            // Read past it so the next message is still framed correctly.
            return match io::copy(&mut self.inner.by_ref().take(len), &mut io::sink()) {
                Ok(_) => Incoming::Malformed(format!(
                    "dropped a message of {len} bytes (the limit is {} bytes)",
                    self.max_size
                )),
                Err(e) => Incoming::Failed(e),
            };
        };
        let mut body = Vec::with_capacity(size.min(INITIAL_BODY_CAPACITY));
        if let Err(e) = self.inner.by_ref().take(len).read_to_end(&mut body) {
            return Incoming::Failed(e);
        }
        if body.len() < size {
            return Incoming::Malformed(format!(
                "the stream ended inside a message body ({} of {size} bytes)",
                body.len()
            ));
        }
        match serde_json::from_slice(&body) {
            Ok(value) => Incoming::Message(value),
            Err(e) => Incoming::Malformed(format!("invalid JSON in a message body: {e}")),
        }
    }
}

fn skipped_message(skipped: usize) -> String {
    format!("skipped {skipped} bytes of output that is not a framed message")
}

fn parse_header_line(line: &str) -> HeaderLine {
    let Some((name, value)) = line.split_once(':') else {
        return HeaderLine::Garbage;
    };
    let (name, value) = (name.trim(), value.trim());
    if name.eq_ignore_ascii_case("content-length") {
        return HeaderLine::ContentLength(value.parse::<u64>().map_err(|_| value.to_owned()));
    }
    if !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        HeaderLine::Other
    } else {
        HeaderLine::Garbage
    }
}

/// A `Content-Length` header embedded in a garbage line: its byte offset and the parsed header.
fn find_content_length(line: &str) -> Option<(usize, HeaderLine)> {
    // ASCII lowercasing keeps byte offsets, so the offset is valid in `line` too.
    let offset = line.to_ascii_lowercase().find("content-length:")?;
    let header = parse_header_line(line.get(offset..)?);
    Some((offset, header))
}

/// Writes one framed message and flushes.
pub(crate) fn write_message<W: Write>(out: &mut W, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(out, "Content-Length: {}\r\n\r\n", body.len())?;
    out.write_all(&body)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use serde_json::json;

    use super::*;

    fn frame(body: &str) -> String {
        format!("Content-Length: {}\r\n\r\n{body}", body.len())
    }

    fn reader(input: impl Into<Vec<u8>>) -> FrameReader<BufReader<Cursor<Vec<u8>>>> {
        FrameReader::new(BufReader::new(Cursor::new(input.into())))
    }

    fn message(incoming: Incoming) -> Value {
        match incoming {
            Incoming::Message(value) => value,
            other => panic!("expected a message, got {other:?}"),
        }
    }

    fn malformed(incoming: Incoming) -> String {
        match incoming {
            Incoming::Malformed(problem) => problem,
            other => panic!("expected a malformed report, got {other:?}"),
        }
    }

    fn assert_eof(incoming: Incoming) {
        assert!(matches!(incoming, Incoming::Eof), "expected EOF, got {incoming:?}");
    }

    /// Hands out at most one byte per `read` call.
    struct Trickle(Cursor<Vec<u8>>);

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match buf.first_mut() {
                Some(slot) => self.0.read(std::slice::from_mut(slot)),
                None => Ok(0),
            }
        }
    }

    #[test]
    fn reads_one_message_then_eof() {
        let mut r = reader(frame(r#"{"jsonrpc":"2.0","id":1,"result":null}"#));
        assert_eq!(message(r.next_message()), json!({"jsonrpc": "2.0", "id": 1, "result": null}));
        assert_eof(r.next_message());
        assert_eof(r.next_message());
    }

    #[test]
    fn empty_stream_is_eof() {
        assert_eof(reader(Vec::new()).next_message());
    }

    #[test]
    fn header_names_are_case_insensitive_and_content_type_is_ignored() {
        let body = r#"{"a":1}"#;
        let input = format!(
            "content-type: application/vscode-jsonrpc; charset=utf-8\r\nCONTENT-LENGTH:{}\r\n\r\n{body}",
            body.len()
        );
        let mut r = reader(input);
        assert_eq!(message(r.next_message()), json!({"a": 1}));
        assert_eof(r.next_message());
    }

    #[test]
    fn accepts_bare_newlines_and_stray_blank_lines() {
        let body = r#"{"a":2}"#;
        let input = format!("\r\n\nContent-Length: {}\n\n{body}", body.len());
        let mut r = reader(input);
        assert_eq!(message(r.next_message()), json!({"a": 2}));
        assert_eof(r.next_message());
    }

    #[test]
    fn two_messages_in_one_buffer() {
        let input = format!("{}{}", frame(r#"{"n":1}"#), frame(r#"{"n":2}"#));
        let mut r = reader(input);
        assert_eq!(message(r.next_message()), json!({"n": 1}));
        assert_eq!(message(r.next_message()), json!({"n": 2}));
        assert_eof(r.next_message());
    }

    #[test]
    fn partial_reads_byte_by_byte() {
        let input = format!("{}{}", frame(r#"{"text":"héllo ✓"}"#), frame(r#"{"n":2}"#));
        let mut r = FrameReader::new(BufReader::with_capacity(1, Trickle(Cursor::new(input.into_bytes()))));
        assert_eq!(message(r.next_message()), json!({"text": "héllo ✓"}));
        assert_eq!(message(r.next_message()), json!({"n": 2}));
        assert_eof(r.next_message());
    }

    #[test]
    fn bad_content_length_is_reported_and_the_next_message_is_read() {
        let input = format!("Content-Length: abc\r\n\r\n{}", frame(r#"{"ok":true}"#));
        let mut r = reader(input);
        assert!(malformed(r.next_message()).contains("invalid Content-Length"));
        assert_eq!(message(r.next_message()), json!({"ok": true}));
        assert_eof(r.next_message());
    }

    #[test]
    fn missing_content_length_is_reported() {
        let input = format!("Content-Type: x\r\n\r\n{}", frame(r#"{"ok":1}"#));
        let mut r = reader(input);
        assert!(malformed(r.next_message()).contains("without Content-Length"));
        assert_eq!(message(r.next_message()), json!({"ok": 1}));
    }

    #[test]
    fn invalid_json_is_reported_and_framing_survives() {
        let input = format!("{}{}", frame("{not json"), frame(r#"{"ok":2}"#));
        let mut r = reader(input);
        assert!(malformed(r.next_message()).contains("invalid JSON"));
        assert_eq!(message(r.next_message()), json!({"ok": 2}));
        assert_eof(r.next_message());
    }

    #[test]
    fn oversized_message_is_skipped() {
        let big = format!(r#"{{"pad":"{}"}}"#, "x".repeat(100));
        let input = format!("{}{}", frame(&big), frame(r#"{"ok":3}"#));
        let mut r = FrameReader::with_limit(BufReader::new(Cursor::new(input.into_bytes())), 64);
        assert!(malformed(r.next_message()).contains("limit"));
        assert_eq!(message(r.next_message()), json!({"ok": 3}));
        assert_eof(r.next_message());
    }

    #[test]
    fn absurd_content_length_does_not_allocate_or_hang() {
        let mut r = reader("Content-Length: 18446744073709551615\r\n\r\n{}");
        assert!(malformed(r.next_message()).contains("limit"));
        assert_eof(r.next_message());
    }

    #[test]
    fn truncated_body_is_reported_then_eof() {
        let mut r = reader("Content-Length: 50\r\n\r\n{\"a\":");
        assert!(malformed(r.next_message()).contains("ended inside a message body"));
        assert_eof(r.next_message());
    }

    #[test]
    fn eof_inside_headers_is_reported_then_eof() {
        let mut r = reader("Content-Length: 5\r\n");
        assert!(malformed(r.next_message()).contains("inside message headers"));
        assert_eof(r.next_message());
    }

    #[test]
    fn garbage_before_a_header_is_skipped_and_reported() {
        let input = format!("hello from the server\n{}", frame(r#"{"ok":4}"#));
        let mut r = reader(input);
        assert!(malformed(r.next_message()).contains("skipped"));
        assert_eq!(message(r.next_message()), json!({"ok": 4}));
        assert_eof(r.next_message());
    }

    #[test]
    fn resyncs_on_a_header_glued_to_garbage() {
        // The first frame claims 2 bytes but carries 9: the rest runs into the next header.
        let input = format!("Content-Length: 2\r\n\r\n{{}}garbage{}", frame(r#"{"ok":5}"#));
        let mut r = reader(input);
        assert_eq!(message(r.next_message()), json!({}));
        assert!(malformed(r.next_message()).contains("skipped 7 bytes"));
        assert_eq!(message(r.next_message()), json!({"ok": 5}));
        assert_eof(r.next_message());
    }

    #[test]
    fn endless_garbage_line_is_bounded() {
        let mut input = vec![b'x'; MAX_HEADER_LINE * (MAX_HEADER_LINES + 2)];
        input.extend_from_slice(frame(r#"{"ok":6}"#).as_bytes());
        let mut r = reader(input);
        let mut problems = 0;
        loop {
            match r.next_message() {
                Incoming::Malformed(_) => problems += 1,
                Incoming::Message(value) => {
                    assert_eq!(value, json!({"ok": 6}));
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
            assert!(problems < 10, "too many reports for one garbage run");
        }
        assert!(problems >= 1);
    }

    #[test]
    fn many_garbage_lines_end_at_eof() {
        let input = "garbage line\n".repeat(1000);
        let mut r = reader(input);
        let mut problems = 0;
        while let Incoming::Malformed(_) = r.next_message() {
            problems += 1;
            assert!(problems < 100);
        }
        assert!(problems > 0);
    }

    #[test]
    fn non_utf8_header_bytes_are_garbage_not_a_panic() {
        let mut input = vec![0xff, 0xfe, b'\n'];
        input.extend_from_slice(frame("{}").as_bytes());
        let mut r = reader(input);
        assert!(malformed(r.next_message()).contains("skipped"));
        assert_eq!(message(r.next_message()), json!({}));
    }

    #[test]
    fn write_then_read_roundtrip() {
        let mut out = Vec::new();
        let value = json!({"jsonrpc": "2.0", "method": "x", "params": {"s": "ünï ✓"}});
        write_message(&mut out, &value).unwrap();
        write_message(&mut out, &json!(1)).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.starts_with("Content-Length: "));
        let mut r = reader(out);
        assert_eq!(message(r.next_message()), value);
        assert_eq!(message(r.next_message()), json!(1));
        assert_eof(r.next_message());
    }

    #[test]
    fn content_length_counts_bytes_not_chars() {
        let mut out = Vec::new();
        write_message(&mut out, &json!("é")).unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("Content-Length: 4\r\n\r\n"));
    }
}
