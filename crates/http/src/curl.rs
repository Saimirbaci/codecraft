//! cURL command lines: Convert cURL to HTTP Request and Copy as cURL.

use crate::HttpError;
use crate::parse::{BodyPart, Request};
use crate::util;
use crate::vars::Variables;

/// Longest command line converted.
const MAX_COMMAND: usize = 16 << 20;
/// The multipart boundary used for `-F` fields.
const BOUNDARY: &str = "CodeCraftBoundary";

/// Split a shell command line into words: single and double quotes, `$'…'`, backslash escapes
/// and line continuations (`\` + newline, and `^` + newline from Windows shells).
pub fn shell_words(cmd: &str) -> Result<Vec<String>, HttpError> {
    if cmd.len() > MAX_COMMAND {
        return Err(HttpError::Parse("the command is too long".into()));
    }
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('\n') => {}
                Some('\r') => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                }
                Some(n) => {
                    cur.push(n);
                    in_word = true;
                }
                None => {}
            },
            '^' if matches!(chars.peek(), Some('\n' | '\r')) => {
                while matches!(chars.peek(), Some('\n' | '\r')) {
                    chars.next();
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => cur.push(ch),
                        None => return Err(HttpError::Parse("unterminated ' quote".into())),
                    }
                }
            }
            '$' if chars.peek() == Some(&'\'') => {
                chars.next();
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => cur.push('\n'),
                            Some('t') => cur.push('\t'),
                            Some('r') => cur.push('\r'),
                            Some('0') => cur.push('\0'),
                            Some(ch) => cur.push(ch),
                            None => return Err(HttpError::Parse("unterminated $' quote".into())),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err(HttpError::Parse("unterminated $' quote".into())),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\' | '$' | '`')) => cur.push(ch),
                            Some('\n') => {}
                            Some(ch) => {
                                cur.push('\\');
                                cur.push(ch);
                            }
                            None => return Err(HttpError::Parse("unterminated \" quote".into())),
                        },
                        Some(ch) => cur.push(ch),
                        None => return Err(HttpError::Parse("unterminated \" quote".into())),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

/// Options that take a value and are ignored.
const IGNORED_WITH_VALUE: &[&str] = &[
    "-o",
    "--output",
    "-m",
    "--max-time",
    "--connect-timeout",
    "-x",
    "--proxy",
    "--cacert",
    "--capath",
    "-E",
    "--cert",
    "--key",
    "-w",
    "--write-out",
    "-c",
    "--cookie-jar",
    "--retry",
    "--retry-delay",
    "--resolve",
    "--connect-to",
    "-K",
    "--config",
    "--limit-rate",
    "--max-redirs",
    "-r",
    "--range",
    "--interface",
    "--dns-servers",
    "-U",
    "--proxy-user",
];

/// Short options that take a value (`-X POST` or `-XPOST`).
const SHORT_WITH_VALUE: &str = "XHdFuAebTomxEcwKrU";

#[derive(Default)]
struct Builder {
    method: Option<String>,
    url: Option<String>,
    headers: Vec<(String, String)>,
    data: Vec<String>,
    data_file: Option<String>,
    form: Vec<String>,
    get: bool,
    head: bool,
    upload: Option<String>,
    bearer: Option<String>,
}

impl Builder {
    fn header(&mut self, name: &str, value: &str) {
        self.headers.push((name.to_string(), value.to_string()));
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    }
}

/// Convert a cURL command line into a request.
pub fn parse_curl(cmd: &str) -> Result<Request, HttpError> {
    let words = shell_words(cmd.trim())?;
    let mut it = words.into_iter().peekable();
    match it.next() {
        Some(w) if w == "curl" || w.ends_with("/curl") || w.eq_ignore_ascii_case("curl.exe") => {}
        _ => return Err(HttpError::Parse("not a cURL command (it must start with `curl`)".into())),
    }
    let mut b = Builder::default();
    while let Some(w) = it.next() {
        let (opt, inline) = if let Some(long) = w.strip_prefix("--") {
            match long.split_once('=') {
                Some((name, value)) if !name.is_empty() => (format!("--{name}"), Some(value.to_string())),
                _ => (w.clone(), None),
            }
        } else if w.starts_with('-') && w.len() > 1 {
            // `-XPOST`, `-sSL`, `-H…`.
            let mut chars = w.chars().skip(1);
            let first = chars.next().unwrap_or('-');
            if SHORT_WITH_VALUE.contains(first) {
                let rest: String = chars.collect();
                (format!("-{first}"), (!rest.is_empty()).then_some(rest))
            } else {
                // A cluster of flags; a value-taking flag at its end takes the next word.
                let last = w.chars().last().unwrap_or('-');
                if SHORT_WITH_VALUE.contains(last) && w.len() > 2 {
                    (format!("-{last}"), None)
                } else {
                    if w.contains('I') {
                        b.head = true;
                    }
                    if w.contains('G') {
                        b.get = true;
                    }
                    continue;
                }
            }
        } else {
            if b.url.is_none() {
                b.url = Some(w);
            }
            continue;
        };
        let value = |it: &mut std::iter::Peekable<std::vec::IntoIter<String>>| -> Result<String, HttpError> {
            match &inline {
                Some(v) => Ok(v.clone()),
                None => it.next().ok_or_else(|| HttpError::Parse(format!("{opt} needs a value"))),
            }
        };
        match opt.as_str() {
            "-X" | "--request" => b.method = Some(value(&mut it)?.to_uppercase()),
            "--url" => b.url = Some(value(&mut it)?),
            "-H" | "--header" => {
                let h = value(&mut it)?;
                if let Some((k, v)) = h.split_once(':') {
                    let (k, v) = (k.trim(), v.trim());
                    if !k.is_empty() && !v.is_empty() {
                        b.header(k, v);
                    }
                }
            }
            "-d" | "--data" | "--data-ascii" | "--data-binary" => {
                let d = value(&mut it)?;
                match d.strip_prefix('@') {
                    Some(file) if b.data.is_empty() && b.data_file.is_none() => b.data_file = Some(file.to_string()),
                    _ => b.data.push(d),
                }
            }
            "--data-raw" => b.data.push(value(&mut it)?),
            "--data-urlencode" => {
                let d = value(&mut it)?;
                let encoded = match d.split_once('=') {
                    Some((name, v)) => format!("{name}={}", util::url_encode(v)),
                    None => util::url_encode(d.strip_prefix('=').unwrap_or(&d)),
                };
                b.data.push(encoded);
            }
            "--json" => {
                b.data.push(value(&mut it)?);
                if !b.has_header("Content-Type") {
                    b.header("Content-Type", "application/json");
                }
                if !b.has_header("Accept") {
                    b.header("Accept", "application/json");
                }
            }
            "-F" | "--form" | "--form-string" => b.form.push(value(&mut it)?),
            "-u" | "--user" => {
                let creds = value(&mut it)?;
                b.header("Authorization", &format!("Basic {}", util::base64(creds.as_bytes())));
            }
            "--oauth2-bearer" => b.bearer = Some(value(&mut it)?),
            "-A" | "--user-agent" => {
                let ua = value(&mut it)?;
                b.header("User-Agent", &ua);
            }
            "-e" | "--referer" => {
                let r = value(&mut it)?;
                b.header("Referer", &r);
            }
            "-b" | "--cookie" => {
                let c = value(&mut it)?;
                if c.contains('=') {
                    b.header("Cookie", &c);
                }
            }
            "-T" | "--upload-file" => b.upload = Some(value(&mut it)?),
            "-I" | "--head" => b.head = true,
            "-G" | "--get" => b.get = true,
            o if IGNORED_WITH_VALUE.contains(&o) => {
                value(&mut it)?;
            }
            _ => {}
        }
    }
    build(b)
}

fn build(mut b: Builder) -> Result<Request, HttpError> {
    let mut url = b.url.take().ok_or_else(|| HttpError::Parse("the cURL command has no URL".into()))?;
    if let Some(token) = &b.bearer {
        let token = token.clone();
        b.header("Authorization", &format!("Bearer {token}"));
    }
    let mut body = Vec::new();
    let mut method = b.method.clone();
    if b.get && !b.data.is_empty() {
        url.push(if url.contains('?') { '&' } else { '?' });
        url.push_str(&b.data.join("&"));
        b.data.clear();
        method.get_or_insert_with(|| "GET".into());
    }
    if let Some(file) = &b.data_file {
        body.push(BodyPart::File(file.clone()));
    } else if !b.data.is_empty() {
        body.push(BodyPart::Text(b.data.join("&")));
    }
    if !body.is_empty() && !b.has_header("Content-Type") {
        b.header("Content-Type", "application/x-www-form-urlencoded");
    }
    if !b.form.is_empty() {
        b.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("Content-Type"));
        b.header("Content-Type", &format!("multipart/form-data; boundary={BOUNDARY}"));
        body = multipart(&b.form);
    }
    if let Some(file) = &b.upload {
        body = vec![BodyPart::File(file.clone())];
        method.get_or_insert_with(|| "PUT".into());
    }
    let method = method.unwrap_or_else(|| {
        if b.head {
            "HEAD".into()
        } else if body.is_empty() {
            "GET".into()
        } else {
            "POST".into()
        }
    });
    Ok(Request { method, url, headers: b.headers, body, ..Request::default() })
}

/// A multipart body for `-F name=value` / `-F name=@file` fields.
fn multipart(fields: &[String]) -> Vec<BodyPart> {
    let mut parts = Vec::new();
    let mut text = String::new();
    for f in fields {
        let (name, value) = f.split_once('=').unwrap_or((f.as_str(), ""));
        text.push_str(&format!("--{BOUNDARY}\n"));
        match value.strip_prefix('@') {
            Some(file) => {
                let file = file.split(';').next().unwrap_or(file);
                let base = std::path::Path::new(file)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                text.push_str(&format!("Content-Disposition: form-data; name=\"{name}\"; filename=\"{base}\"\n\n"));
                parts.push(BodyPart::Text(std::mem::take(&mut text)));
                parts.push(BodyPart::File(file.to_string()));
                text.push('\n');
            }
            None => {
                let value = value.strip_prefix('<').unwrap_or(value);
                text.push_str(&format!("Content-Disposition: form-data; name=\"{name}\"\n\n{value}\n"));
            }
        }
    }
    text.push_str(&format!("--{BOUNDARY}--"));
    parts.push(BodyPart::Text(text));
    parts
}

/// Quote a word for a POSIX shell (single quotes; `'` as `'\''`).
pub fn shell_quote(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@%+,".contains(c)) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// A cURL command for a request, with variables substituted (unknown ones stay as written).
/// A body read from one file becomes `--data-binary @file`.
pub fn to_curl(req: &Request, vars: &Variables) -> String {
    let sub = |t: &str| vars.substitute(t).0;
    let mut url = sub(&req.url);
    let headers: Vec<(String, String)> = req.headers.iter().map(|(k, v)| (sub(k), sub(v))).collect();
    if url.starts_with('/')
        && let Some((_, host)) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host"))
    {
        url = format!("http://{}{url}", host.trim());
    }
    let mut lines = vec![format!("curl -X {} {}", req.method, shell_quote(&url))];
    if req.method == "GET" && req.body.is_empty() {
        lines = vec![format!("curl {}", shell_quote(&url))];
    } else if req.method == "HEAD" {
        lines = vec![format!("curl -I {}", shell_quote(&url))];
    }
    for (k, v) in &headers {
        lines.push(format!("-H {}", shell_quote(&format!("{k}: {v}"))));
    }
    match req.body.as_slice() {
        [] => {}
        [BodyPart::File(f)] => lines.push(format!("--data-binary {}", shell_quote(&format!("@{}", sub(f))))),
        parts => {
            let mut text = String::new();
            for p in parts {
                match p {
                    BodyPart::Text(t) => text.push_str(&sub(t)),
                    BodyPart::File(f) | BodyPart::FileWithVariables(f) => text.push_str(&format!("<{}>", sub(f))),
                }
            }
            lines.push(format!("--data-raw {}", shell_quote(&text)));
        }
    }
    if req.no_redirect {
        lines.push("--max-redirs 0".into());
    }
    lines.join(" \\\n    ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse, to_http_text};

    #[test]
    fn words() {
        assert_eq!(
            shell_words(
                r#"curl 'a b' "c \"d\" \$e" f\ g $'x\ny' \
  h"#
            )
            .unwrap(),
            vec!["curl", "a b", "c \"d\" $e", "f g", "x\ny", "h"]
        );
        assert!(shell_words("curl 'oops").is_err());
        assert_eq!(shell_words("curl ^\r\n -v").unwrap(), vec!["curl", "-v"]);
    }

    #[test]
    fn converts_common_options() {
        let r = parse_curl(
            r#"curl -X POST 'https://api.example.org/v1/items?x=1' \
              -H 'Content-Type: application/json' -H "Authorization: Bearer t0k" \
              --data-raw '{"name":"a b"}' --compressed -sSL -o /dev/null"#,
        )
        .unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://api.example.org/v1/items?x=1");
        assert_eq!(
            r.headers,
            vec![("Content-Type".into(), "application/json".into()), ("Authorization".into(), "Bearer t0k".into())]
        );
        assert_eq!(r.body, vec![BodyPart::Text(r#"{"name":"a b"}"#.into())]);

        let r = parse_curl("curl -d a=1 -d b=2 http://h/p").unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.body, vec![BodyPart::Text("a=1&b=2".into())]);
        assert_eq!(r.headers, vec![("Content-Type".into(), "application/x-www-form-urlencoded".into())]);

        let r = parse_curl("curl -G --data-urlencode 'q=a b' http://h/s").unwrap();
        assert_eq!((r.method.as_str(), r.url.as_str()), ("GET", "http://h/s?q=a%20b"));
        assert!(r.body.is_empty());

        let r = parse_curl("curl -u user:pass -A agent -e http://ref --url=http://h -I").unwrap();
        assert_eq!(r.method, "HEAD");
        assert_eq!(r.url, "http://h");
        assert_eq!(r.headers[0], ("Authorization".into(), "Basic dXNlcjpwYXNz".into()));
        assert_eq!(r.headers[1], ("User-Agent".into(), "agent".into()));

        let r = parse_curl("curl -XPUT -HX-A:1 --data-binary @body.bin http://h").unwrap();
        assert_eq!(r.method, "PUT");
        assert_eq!(r.headers[0], ("X-A".into(), "1".into()));
        assert_eq!(r.body, vec![BodyPart::File("body.bin".into())]);

        let r = parse_curl("curl --json '{\"a\":1}' http://h").unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.headers.len(), 2);

        let r = parse_curl("curl -F name=x -F file=@./a.png http://h/up").unwrap();
        assert_eq!(r.method, "POST");
        assert!(r.headers[0].1.starts_with("multipart/form-data; boundary="));
        assert!(matches!(&r.body[1], BodyPart::File(f) if f == "./a.png"));

        assert!(parse_curl("wget http://x").is_err());
        assert!(parse_curl("curl -H").is_err());
        assert!(parse_curl("curl -v").is_err());
    }

    #[test]
    fn round_trips_through_http_text_and_back_to_curl() {
        let cmds = [
            "curl http://localhost:8080/users",
            "curl -X POST 'http://h/x' -H 'Content-Type: application/json' --data-raw '{\"it'\\''s\": 1}'",
            "curl -X DELETE 'http://h/items/1' -H 'Authorization: Bearer x'",
            "curl -X PUT http://h/f -H 'Content-Type: application/octet-stream' --data-binary @./file.bin",
        ];
        for cmd in cmds {
            let r = parse_curl(cmd).unwrap();
            let text = to_http_text(&r);
            let parsed = parse(&text);
            let back = &parsed.requests[0];
            assert_eq!((&back.method, &back.url, &back.headers, &back.body), (&r.method, &r.url, &r.headers, &r.body));
            let curl = to_curl(back, &Variables::default());
            let again = parse_curl(&curl).unwrap();
            assert_eq!(
                (&again.method, &again.url, &again.headers, &again.body),
                (&r.method, &r.url, &r.headers, &r.body),
                "{curl}"
            );
        }
    }

    #[test]
    fn to_curl_substitutes_variables() {
        let f = parse("@host = example.org\nGET /a\nHost: {{host}}\nX: {{missing}}\n");
        let v = Variables::new(&f.variables, Default::default());
        let c = to_curl(&f.requests[0], &v);
        assert_eq!(c, "curl http://example.org/a \\\n    -H 'Host: example.org' \\\n    -H 'X: {{missing}}'");
    }
}
