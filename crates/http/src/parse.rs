//! Parsing `.http` files into requests.
//!
//! A file is split into blocks by lines starting with `###` (the rest of that line names the
//! request). In a block, comment lines (`#`, `//`), file variables (`@name = value`) and blank
//! lines come first; then the request line (`METHOD URL [HTTP/x]`, or just a URL for `GET`),
//! indented continuation lines of the URL (`    &page=2`), headers (`Name: value`) up to a blank
//! line, then the body. Body lines `< ./file` (or `<@ ./file`, with variables substituted) are
//! file references. A response handler (`> {% … %}` or `> ./script.js`), an output redirect
//! (`>> ./file`, `>>! ./file`) or a response reference (`<> ./file`) ends the body.

use serde::{Deserialize, Serialize};

/// Most lines parsed; the rest of a huge file is ignored.
const MAX_LINES: usize = 1_000_000;

/// A parsed `.http` file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpFile {
    /// `@name = value` declarations, in file order.
    pub variables: Vec<FileVariable>,
    pub requests: Vec<Request>,
}

/// A file variable: `@name = value`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileVariable {
    pub name: String,
    pub value: String,
    pub line: usize,
}

/// One part of a request body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BodyPart {
    /// Literal text (variables are substituted when the request is resolved).
    Text(String),
    /// `< path`: the file's bytes, sent as they are.
    File(String),
    /// `<@ path`: the file's text with variables substituted.
    FileWithVariables(String),
}

/// Where a response is written: `>> path` (a new name when it exists) or `>>! path` (overwrite).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRedirect {
    pub path: String,
    pub overwrite: bool,
}

/// One request of a `.http` file (unresolved: `{{variables}}` are still in the text).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// From `# @name x`, else the text after `###`.
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    /// `HTTP/1.1`, `HTTP/2`, … when given (the client always speaks HTTP/1.1).
    pub version: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Empty when the request has no body.
    pub body: Vec<BodyPart>,
    /// First line of the request's block (its `###` line, or the first line of the file).
    pub start_line: usize,
    /// The request line.
    pub line: usize,
    /// Last line of the block (inclusive).
    pub end_line: usize,
    /// `# @no-redirect`: don't follow redirects.
    pub no_redirect: bool,
    /// The response handler script (`> {% … %}`) or its file (`> ./x.js`); kept, never run.
    pub handler: Option<String>,
    pub output: Option<OutputRedirect>,
}

impl Request {
    /// A short label: the name, else `METHOD url`.
    pub fn title(&self) -> String {
        match &self.name {
            Some(n) if !n.is_empty() => n.clone(),
            _ => format!("{} {}", self.method, self.url),
        }
    }

    /// The body when it is plain text only (no file parts).
    pub fn body_text(&self) -> Option<String> {
        if self.body.is_empty() {
            return None;
        }
        let mut out = String::new();
        for p in &self.body {
            match p {
                BodyPart::Text(t) => out.push_str(t),
                BodyPart::File(_) | BodyPart::FileWithVariables(_) => return None,
            }
        }
        Some(out)
    }
}

/// What a line of a request block is, for completion and highlighting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineContext {
    /// A `###` separator.
    Separator,
    /// A comment or file variable before the request line.
    Comment,
    /// Blank before the request line, or the request line itself.
    RequestLine,
    /// A URL continuation line or a header (up to the first blank line).
    Header,
    Body,
    /// A response handler, output redirect or response reference.
    Handler,
}

pub(crate) fn is_separator(line: &str) -> bool {
    line.starts_with("###")
}

pub(crate) fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//")
}

/// `@name = value` (or `@name value`).
fn file_variable(line: &str) -> Option<(String, String)> {
    let rest = line.trim_start().strip_prefix('@')?;
    let end = rest.find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))).unwrap_or(rest.len());
    let name = rest.get(..end)?;
    if name.is_empty() {
        return None;
    }
    let after = rest.get(end..)?.trim_start();
    let value = after.strip_prefix('=').unwrap_or(after).trim();
    Some((name.to_string(), value.to_string()))
}

/// A comment tag: `# @name value` / `// @no-redirect` → (`name`, `value`).
fn comment_tag(line: &str) -> Option<(String, String)> {
    let t = line.trim_start();
    let t = t.strip_prefix("//").or_else(|| t.strip_prefix('#'))?.trim_start();
    let rest = t.strip_prefix('@')?;
    let end = rest.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(rest.len());
    let tag = rest.get(..end)?.to_string();
    let value = rest.get(end..)?.trim_start();
    let value = value.strip_prefix('=').unwrap_or(value).trim();
    Some((tag, value.to_string()))
}

/// Starts a handler, redirect or response reference, ending the body.
fn is_trailer(line: &str) -> bool {
    line.starts_with("> ") || line.starts_with(">{%") || line == ">" || line.starts_with(">>") || line.starts_with("<>")
}

fn is_method(word: &str) -> bool {
    !word.is_empty() && word.len() <= 16 && word.chars().all(|c| c.is_ascii_uppercase() || c == '-')
}

/// `METHOD URL [HTTP/x]` or `URL`.
fn request_line(line: &str) -> (String, String, Option<String>) {
    let t = line.trim();
    let mut words = t.splitn(2, char::is_whitespace);
    let first = words.next().unwrap_or_default();
    let (method, rest) = if is_method(first) {
        (first.to_string(), words.next().unwrap_or_default().trim())
    } else {
        ("GET".to_string(), t)
    };
    // A trailing `HTTP/1.1` is the version.
    match rest.rsplit_once(char::is_whitespace) {
        Some((url, v)) if v.starts_with("HTTP/") => (method, url.trim().to_string(), Some(v.to_string())),
        _ => (method, rest.to_string(), None),
    }
}

/// Lines of `text`, split on `\n` with a trailing `\r` removed.
fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').take(MAX_LINES).map(|l| l.strip_suffix('\r').unwrap_or(l))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Preamble,
    Url,
    Headers,
    Body,
    Handler,
}

/// Parse a whole file.
pub fn parse(text: &str) -> HttpFile {
    let mut file = HttpFile::default();
    let mut block: Vec<(usize, &str)> = Vec::new();
    let mut block_start = 0;
    for (i, line) in lines(text).enumerate() {
        if is_separator(line) {
            parse_block(block_start, &block, &mut file);
            block.clear();
            block_start = i;
        }
        block.push((i, line));
    }
    parse_block(block_start, &block, &mut file);
    file
}

fn parse_block(start: usize, block: &[(usize, &str)], file: &mut HttpFile) {
    let Some(&(_, first)) = block.first() else { return };
    let end_line = block.last().map_or(start, |(i, _)| *i);
    let mut req = Request { start_line: start, end_line, ..Request::default() };
    if is_separator(first) {
        let name = first.trim_start_matches('#').trim();
        if !name.is_empty() {
            req.name = Some(name.to_string());
        }
    }
    let mut state = State::Preamble;
    let mut body: Vec<&str> = Vec::new();
    let mut handler: Vec<&str> = Vec::new();
    let mut in_script = false;
    for &(i, line) in block {
        if is_separator(line) {
            continue;
        }
        match state {
            State::Preamble => {
                if line.trim().is_empty() {
                    continue;
                }
                if is_comment(line) {
                    if let Some((tag, value)) = comment_tag(line) {
                        match tag.as_str() {
                            "name" if !value.is_empty() => req.name = Some(value),
                            "no-redirect" => req.no_redirect = true,
                            _ => {}
                        }
                    }
                    continue;
                }
                if let Some((name, value)) = file_variable(line) {
                    file.variables.push(FileVariable { name, value, line: i });
                    continue;
                }
                let (method, url, version) = request_line(line);
                req.method = method;
                req.url = url;
                req.version = version;
                req.line = i;
                state = State::Url;
            }
            State::Url | State::Headers => {
                if line.trim().is_empty() {
                    state = State::Body;
                    continue;
                }
                if is_trailer(line) {
                    state = State::Handler;
                    trailer(line, &mut req, &mut handler, &mut in_script);
                    continue;
                }
                if is_comment(line) {
                    continue;
                }
                if state == State::Url && line.starts_with([' ', '\t']) {
                    req.url.push_str(line.trim());
                    continue;
                }
                state = State::Headers;
                if let Some((name, value)) = line.split_once(':') {
                    let name = name.trim();
                    if !name.is_empty() {
                        req.headers.push((name.to_string(), value.trim().to_string()));
                    }
                }
            }
            State::Body => {
                if !in_script && is_trailer(line) {
                    state = State::Handler;
                    trailer(line, &mut req, &mut handler, &mut in_script);
                    continue;
                }
                body.push(line);
            }
            State::Handler => {
                if in_script {
                    handler.push(line);
                    if line.contains("%}") {
                        in_script = false;
                    }
                } else if is_trailer(line) {
                    trailer(line, &mut req, &mut handler, &mut in_script);
                }
            }
        }
    }
    if state == State::Preamble {
        return;
    }
    // Trailing blank and comment lines belong to the next request's preamble, not the body.
    while body.last().is_some_and(|l| l.trim().is_empty() || is_comment(l)) {
        body.pop();
    }
    req.body = body_parts(&body);
    if !handler.is_empty() && req.handler.is_none() {
        req.handler = Some(handler.join("\n"));
    }
    file.requests.push(req);
}

fn trailer<'a>(line: &'a str, req: &mut Request, handler: &mut Vec<&'a str>, in_script: &mut bool) {
    if let Some(rest) = line.strip_prefix(">>") {
        let (overwrite, path) = match rest.strip_prefix('!') {
            Some(p) => (true, p),
            None => (false, rest),
        };
        let path = path.trim();
        if !path.is_empty() {
            req.output = Some(OutputRedirect { path: path.to_string(), overwrite });
        }
    } else if line.starts_with("<>") {
        // A reference to an earlier response (history): nothing to do.
    } else if let Some(rest) = line.strip_prefix('>') {
        let rest = rest.trim();
        if rest.starts_with("{%") {
            handler.push(line);
            *in_script = !rest.contains("%}");
        } else if !rest.is_empty() {
            req.handler = Some(rest.to_string());
        }
    }
}

fn body_parts(lines: &[&str]) -> Vec<BodyPart> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, parts: &mut Vec<BodyPart>| {
        if !text.is_empty() {
            parts.push(BodyPart::Text(std::mem::take(text)));
        }
    };
    let n = lines.len();
    for (k, line) in lines.iter().enumerate() {
        let file = line
            .strip_prefix("<@")
            .map(|p| BodyPart::FileWithVariables(p.trim().to_string()))
            .or_else(|| line.strip_prefix("< ").map(|p| BodyPart::File(p.trim().to_string())));
        match file {
            Some(part) => {
                // The newline before a file reference separates it from the text (multipart).
                flush(&mut text, &mut parts);
                parts.push(part);
                if k + 1 < n {
                    text.push('\n');
                }
            }
            None => {
                text.push_str(line);
                if k + 1 < n {
                    text.push('\n');
                }
            }
        }
    }
    flush(&mut text, &mut parts);
    parts
}

/// The request whose block contains `line` (or the nearest one before it).
pub fn request_at(file: &HttpFile, line: usize) -> Option<&Request> {
    file.requests
        .iter()
        .find(|r| (r.start_line..=r.end_line).contains(&line))
        .or_else(|| file.requests.iter().rev().find(|r| r.start_line <= line))
}

/// Request lines (for the gutter ▶ icons).
pub fn request_lines(text: &str) -> Vec<usize> {
    parse(text).requests.iter().map(|r| r.line).collect()
}

/// What `line` of `text` is (see [`LineContext`]).
pub fn line_context(text: &str, line: usize) -> LineContext {
    let mut state = State::Preamble;
    let mut in_script = false;
    for (i, l) in lines(text).enumerate() {
        if is_separator(l) {
            state = State::Preamble;
            in_script = false;
            if i == line {
                return LineContext::Separator;
            }
            continue;
        }
        let ctx = match state {
            State::Preamble => {
                if l.trim().is_empty() {
                    LineContext::RequestLine
                } else if is_comment(l) || file_variable(l).is_some() {
                    LineContext::Comment
                } else {
                    state = State::Url;
                    LineContext::RequestLine
                }
            }
            State::Url | State::Headers => {
                if l.trim().is_empty() {
                    state = State::Body;
                    LineContext::Body
                } else if is_trailer(l) {
                    state = State::Handler;
                    in_script = l.contains("{%") && !l.contains("%}");
                    LineContext::Handler
                } else {
                    if !(state == State::Url && l.starts_with([' ', '\t'])) && !is_comment(l) {
                        state = State::Headers;
                    }
                    LineContext::Header
                }
            }
            State::Body => {
                if is_trailer(l) {
                    state = State::Handler;
                    in_script = l.contains("{%") && !l.contains("%}");
                    LineContext::Handler
                } else {
                    LineContext::Body
                }
            }
            State::Handler => {
                if in_script && l.contains("%}") {
                    in_script = false;
                }
                LineContext::Handler
            }
        };
        if i == line {
            return ctx;
        }
    }
    match state {
        State::Preamble => LineContext::RequestLine,
        State::Url | State::Headers => LineContext::Header,
        State::Body => LineContext::Body,
        State::Handler => LineContext::Handler,
    }
}

/// `.http` text for a request (the inverse of [`parse`] for one request).
pub fn to_http_text(req: &Request) -> String {
    let mut out = String::new();
    match &req.name {
        Some(n) if !n.is_empty() => {
            out.push_str("### ");
            out.push_str(n);
            out.push('\n');
        }
        _ => out.push_str("###\n"),
    }
    out.push_str(&req.method);
    out.push(' ');
    out.push_str(&req.url);
    if let Some(v) = &req.version {
        out.push(' ');
        out.push_str(v);
    }
    out.push('\n');
    for (k, v) in &req.headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push('\n');
    }
    if !req.body.is_empty() {
        out.push('\n');
        for p in &req.body {
            match p {
                BodyPart::Text(t) => out.push_str(t),
                BodyPart::File(f) => {
                    out.push_str("< ");
                    out.push_str(f);
                }
                BodyPart::FileWithVariables(f) => {
                    out.push_str("<@ ");
                    out.push_str(f);
                }
            }
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
@host = localhost:8080
@token = abc

### Get users
# @name listUsers
GET http://{{host}}/users?page=1 HTTP/1.1
Accept: application/json
// a comment between headers
Authorization: Bearer {{token}}

###
POST http://{{host}}/users
Content-Type: application/json

{
  \"name\": \"{{$uuid}}\"
}

> {%
    client.test(\"ok\", function() {});
%}

### Upload
PUT http://{{host}}/files
    ?name=a.txt
    &overwrite=true
Content-Type: text/plain

< ./data/a.txt
>> ./out.txt

https://example.org/plain
";

    #[test]
    fn parses_requests_variables_and_names() {
        let f = parse(SAMPLE);
        assert_eq!(
            f.variables.iter().map(|v| (v.name.as_str(), v.value.as_str())).collect::<Vec<_>>(),
            vec![("host", "localhost:8080"), ("token", "abc")]
        );
        assert_eq!(f.requests.len(), 3);
        let r = &f.requests[0];
        assert_eq!(r.name.as_deref(), Some("listUsers"));
        assert_eq!(r.method, "GET");
        assert_eq!(r.url, "http://{{host}}/users?page=1");
        assert_eq!(r.version.as_deref(), Some("HTTP/1.1"));
        assert_eq!(
            r.headers,
            vec![("Accept".into(), "application/json".into()), ("Authorization".into(), "Bearer {{token}}".into())]
        );
        assert!(r.body.is_empty());
        assert_eq!(r.line, 5);
        assert_eq!(r.start_line, 3);

        let r = &f.requests[1];
        assert_eq!(r.name, None);
        assert_eq!(r.method, "POST");
        assert_eq!(r.body, vec![BodyPart::Text("{\n  \"name\": \"{{$uuid}}\"\n}".into())]);
        assert!(r.handler.as_deref().is_some_and(|h| h.contains("client.test")));

        let r = &f.requests[2];
        assert_eq!(r.name.as_deref(), Some("Upload"));
        assert_eq!(r.url, "http://{{host}}/files?name=a.txt&overwrite=true");
        assert_eq!(r.body, vec![BodyPart::File("./data/a.txt".into())]);
        assert_eq!(r.output, Some(OutputRedirect { path: "./out.txt".into(), overwrite: false }));
        // A bare URL after the redirect is part of the same block (no `###`): ignored.
    }

    #[test]
    fn bare_url_and_lines() {
        let f = parse("https://example.org\n\n###\n\n# just a comment\n###\nhttp://a/b\n");
        assert_eq!(f.requests.len(), 2);
        assert_eq!(f.requests[0].method, "GET");
        assert_eq!(f.requests[0].url, "https://example.org");
        assert_eq!(f.requests[1].line, 6);
        assert_eq!(request_lines("GET /a\n###\nPOST /b\n"), vec![0, 2]);
        assert_eq!(request_at(&f, 0).map(|r| r.line), Some(0));
        assert_eq!(request_at(&f, 7).map(|r| r.line), Some(6));
    }

    #[test]
    fn body_keeps_inner_comments_drops_trailing() {
        let f = parse("POST /x\n\nline1\n# inner\nline2\n\n# about the next one\n");
        assert_eq!(f.requests[0].body, vec![BodyPart::Text("line1\n# inner\nline2".into())]);
        let f = parse("POST /x\nContent-Type: multipart/form-data; boundary=B\n\n--B\n< ./a.bin\n--B--\n");
        assert_eq!(
            f.requests[0].body,
            vec![BodyPart::Text("--B\n".into()), BodyPart::File("./a.bin".into()), BodyPart::Text("\n--B--".into())]
        );
        let f = parse("POST /x\n\n<@ ./t.json\n");
        assert_eq!(f.requests[0].body, vec![BodyPart::FileWithVariables("./t.json".into())]);
    }

    #[test]
    fn tags_and_crlf() {
        let f = parse("// @no-redirect\r\n# @name=login\r\nGET /a\r\nX: 1\r\n");
        let r = &f.requests[0];
        assert!(r.no_redirect);
        assert_eq!(r.name.as_deref(), Some("login"));
        assert_eq!(r.headers, vec![("X".into(), "1".into())]);
    }

    #[test]
    fn contexts() {
        let text = "### a\n# c\n\nGET /x\nAccept: */*\n\n{}\n> {%\nx\n%}\n###\n";
        let ctx: Vec<LineContext> = (0..11).map(|l| line_context(text, l)).collect();
        use LineContext::*;
        assert_eq!(
            ctx,
            vec![
                Separator,
                Comment,
                RequestLine,
                RequestLine,
                Header,
                Body,
                Body,
                Handler,
                Handler,
                Handler,
                Separator
            ]
        );
        assert_eq!(line_context("", 0), RequestLine);
        assert_eq!(line_context("GET /x", 5), Header);
        assert_eq!(line_context("GET /x\n", 5), Body);
    }

    #[test]
    fn round_trip_text() {
        let f = parse(SAMPLE);
        for r in &f.requests {
            let text = to_http_text(r);
            let again = parse(&text);
            let a = &again.requests[0];
            assert_eq!((&a.method, &a.url, &a.headers, &a.body), (&r.method, &r.url, &r.headers, &r.body), "{text}");
        }
    }

    #[test]
    fn hostile_input_never_panics() {
        for t in
            ["", "###", "\n\n\n", "@", "@=", "GET", "> {%", "<>", ">>!", "\u{1F600}: \u{1F600}\n\n< ", "\r\r\n###\r"]
        {
            let f = parse(t);
            for l in 0..5 {
                let _ = line_context(t, l);
            }
            let _ = request_at(&f, usize::MAX);
        }
    }
}
