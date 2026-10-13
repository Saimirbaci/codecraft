//! Presenting responses: pretty JSON (re-indented, key order kept), text for XML/HTML and
//! others, a note for binary bodies; sizes and file extensions.

/// Largest body shown (in chars); the rest is cut with a note.
pub const MAX_DISPLAY: usize = 2 << 20;

/// The kind of a response body, from its `Content-Type` (or a sniff of the bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BodyKind {
    Json,
    Xml,
    Html,
    Text,
    Binary,
}

impl BodyKind {
    pub fn detect(content_type: Option<&str>, body: &[u8]) -> Self {
        let ct = content_type.unwrap_or("").to_ascii_lowercase();
        let mime = ct.split(';').next().unwrap_or("").trim();
        if mime.ends_with("json") || mime.ends_with("+json") {
            return Self::Json;
        }
        if mime.contains("html") {
            return Self::Html;
        }
        if mime.ends_with("xml") || mime.ends_with("+xml") {
            return Self::Xml;
        }
        if mime.starts_with("image/")
            || mime.starts_with("audio/")
            || mime.starts_with("video/")
            || matches!(mime, "application/octet-stream" | "application/pdf" | "application/zip" | "application/gzip")
        {
            return Self::Binary;
        }
        let head = body.get(..body.len().min(4096)).unwrap_or(body);
        if head.contains(&0)
            || (std::str::from_utf8(head).is_err() && String::from_utf8_lossy(head).matches('\u{FFFD}').count() > 4)
        {
            return Self::Binary;
        }
        let start = head.iter().position(|b| !b.is_ascii_whitespace()).and_then(|i| head.get(i)).copied();
        match start {
            Some(b'{' | b'[') if mime.is_empty() || mime == "text/plain" => Self::Json,
            Some(b'<') if mime.is_empty() => {
                let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
                if lower.contains("<html") || lower.contains("<!doctype html") { Self::Html } else { Self::Xml }
            }
            _ => Self::Text,
        }
    }

    /// File extension for a saved body.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Html => "html",
            Self::Text => "txt",
            Self::Binary => "bin",
        }
    }
}

/// Re-indent JSON text (2 spaces), keeping key order and the exact tokens. Not validating:
/// garbage stays readable.
pub fn pretty_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    let mut indent = 0usize;
    let mut chars = text.chars().peekable();
    let newline = |out: &mut String, indent: usize| {
        out.push('\n');
        for _ in 0..indent.min(64) {
            out.push_str("  ");
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push('"');
                while let Some(ch) = chars.next() {
                    out.push(ch);
                    match ch {
                        '\\' => {
                            if let Some(n) = chars.next() {
                                out.push(n);
                            }
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '{' | '[' => {
                out.push(c);
                // Skip whitespace to see whether the container is empty.
                while chars.peek().is_some_and(|ch| ch.is_whitespace()) {
                    chars.next();
                }
                if matches!(chars.peek(), Some('}' | ']')) {
                    if let Some(close) = chars.next() {
                        out.push(close);
                    }
                } else {
                    indent += 1;
                    newline(&mut out, indent);
                }
            }
            '}' | ']' => {
                indent = indent.saturating_sub(1);
                newline(&mut out, indent);
                out.push(c);
            }
            ',' => {
                out.push(',');
                newline(&mut out, indent);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

/// Cut `text` to [`MAX_DISPLAY`] chars (at a char boundary) with a note.
pub fn truncate_display(text: String) -> String {
    match text.char_indices().nth(MAX_DISPLAY) {
        Some((byte, _)) => {
            let total = text.len();
            let mut t = text.get(..byte).unwrap_or_default().to_string();
            t.push_str(&format!(
                "\n\n… (response truncated for display: {} of {})",
                human_size(byte as u64),
                human_size(total as u64)
            ));
            t
        }
        None => text,
    }
}

/// The body as display text: pretty JSON, text for XML/HTML/other, a note for binary bodies.
pub fn display_body(content_type: Option<&str>, body: &[u8], total_len: u64) -> String {
    let kind = BodyKind::detect(content_type, body);
    let text = match kind {
        BodyKind::Binary => return format!("(binary data, {})", human_size(total_len)),
        BodyKind::Json => {
            let t = String::from_utf8_lossy(body);
            if t.len() <= 32 << 20 { pretty_json(&t) } else { t.into_owned() }
        }
        _ => String::from_utf8_lossy(body).into_owned(),
    };
    let mut text = truncate_display(text);
    if total_len > body.len() as u64 {
        text.push_str(&format!(
            "\n\n… (only the first {} of {} kept in memory)",
            human_size(body.len() as u64),
            human_size(total_len)
        ));
    }
    text
}

/// `512 B`, `1.5 KB`, `2.0 MB`.
pub fn human_size(n: u64) -> String {
    const KB: f64 = 1024.0;
    let f = n as f64;
    if f < KB {
        format!("{n} B")
    } else if f < KB * KB {
        format!("{:.1} KB", f / KB)
    } else if f < KB * KB * KB {
        format!("{:.1} MB", f / (KB * KB))
    } else {
        format!("{:.1} GB", f / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_reindented_in_order() {
        assert_eq!(
            pretty_json(r#"{"b":1,"a":[1, 2,{}],"s":"x,{\"y\":[]}", "e": []}"#),
            "{\n  \"b\": 1,\n  \"a\": [\n    1,\n    2,\n    {}\n  ],\n  \"s\": \"x,{\\\"y\\\":[]}\",\n  \"e\": []\n}"
        );
        // Garbage never panics.
        for t in ["}}}]]", "\"unterminated", "{\"a\":\\", "[[[[["] {
            let _ = pretty_json(t);
        }
    }

    #[test]
    fn detects_kinds() {
        assert_eq!(BodyKind::detect(Some("application/json; charset=utf-8"), b"{}"), BodyKind::Json);
        assert_eq!(BodyKind::detect(Some("application/problem+json"), b"{}"), BodyKind::Json);
        assert_eq!(BodyKind::detect(Some("text/html"), b""), BodyKind::Html);
        assert_eq!(BodyKind::detect(Some("application/xml"), b""), BodyKind::Xml);
        assert_eq!(BodyKind::detect(Some("image/png"), b"x"), BodyKind::Binary);
        assert_eq!(BodyKind::detect(None, b"  [1]"), BodyKind::Json);
        assert_eq!(BodyKind::detect(None, b"<!DOCTYPE html><html>"), BodyKind::Html);
        assert_eq!(BodyKind::detect(None, b"<a/>"), BodyKind::Xml);
        assert_eq!(BodyKind::detect(None, b"a\0b"), BodyKind::Binary);
        assert_eq!(BodyKind::detect(None, b"hello"), BodyKind::Text);
    }

    #[test]
    fn display_and_sizes() {
        assert_eq!(display_body(Some("application/json"), b"{\"a\":1}", 7), "{\n  \"a\": 1\n}");
        assert_eq!(display_body(Some("image/png"), b"\x89PNG", 2048), "(binary data, 2.0 KB)");
        assert!(display_body(None, b"abc", 10).contains("only the first 3 B of 10 B"));
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(1536), "1.5 KB");
        let big = "é".repeat(MAX_DISPLAY + 10);
        let t = truncate_display(big);
        assert!(t.contains("truncated"));
    }
}
