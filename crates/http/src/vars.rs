//! `{{variable}}` substitution and resolving a parsed request into what is sent.
//!
//! Lookup order: file variables (`@name = value`, the last declaration wins), then the selected
//! environment, then dynamic variables (`$uuid`, `$timestamp`, `$isoTimestamp`, `$randomInt`,
//! `$random.*`, `$processEnv.NAME`). Unknown variables stay as written and are reported.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::HttpError;
use crate::parse::{BodyPart, FileVariable, Request};
use crate::util::{self, Rng};

/// Nested substitutions followed (`@a = {{b}}`, `@b = …`).
const MAX_DEPTH: usize = 8;
/// Longest random string a dynamic variable produces.
const MAX_RANDOM_LEN: usize = 10_000;
/// Largest substituted text (bigger inputs are returned unchanged).
const MAX_TEXT: usize = 64 << 20;
/// Largest body file read.
pub const MAX_BODY_FILE: u64 = 256 << 20;

/// Variables available to a request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Variables {
    pub file: BTreeMap<String, String>,
    pub env: BTreeMap<String, String>,
}

impl Variables {
    pub fn new(file: &[FileVariable], env: BTreeMap<String, String>) -> Self {
        Self { file: file.iter().map(|v| (v.name.clone(), v.value.clone())).collect(), env }
    }

    fn raw(&self, name: &str) -> Option<String> {
        self.file.get(name).or_else(|| self.env.get(name)).cloned().or_else(|| dynamic(name))
    }

    /// The value of `name` with nested variables substituted.
    pub fn lookup(&self, name: &str) -> Option<String> {
        let raw = self.raw(name.trim())?;
        let mut unresolved = Vec::new();
        Some(self.subst(&raw, 1, &mut unresolved))
    }

    /// Substitute every `{{name}}` in `text`; returns the text and the names left unresolved.
    pub fn substitute(&self, text: &str) -> (String, Vec<String>) {
        let mut unresolved = Vec::new();
        let out = self.subst(text, 0, &mut unresolved);
        unresolved.dedup();
        (out, unresolved)
    }

    fn subst(&self, text: &str, depth: usize, unresolved: &mut Vec<String>) -> String {
        if !text.contains("{{") || text.len() > MAX_TEXT {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            out.push_str(rest.get(..start).unwrap_or_default());
            let after = rest.get(start + 2..).unwrap_or_default();
            let Some(end) = after.find("}}") else {
                out.push_str(rest.get(start..).unwrap_or_default());
                return out;
            };
            let name = after.get(..end).unwrap_or_default().trim();
            let value = if depth < MAX_DEPTH { self.raw(name) } else { None };
            match value {
                Some(v) => out.push_str(&self.subst(&v, depth + 1, unresolved)),
                None => {
                    if !unresolved.iter().any(|u| u == name) {
                        unresolved.push(name.to_string());
                    }
                    out.push_str("{{");
                    out.push_str(after.get(..end).unwrap_or_default());
                    out.push_str("}}");
                }
            }
            rest = after.get(end + 2..).unwrap_or_default();
        }
        out.push_str(rest);
        out
    }

    /// Every variable name known (file and environment), sorted.
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.file.keys().chain(self.env.keys()).cloned().collect();
        v.sort();
        v.dedup();
        v
    }
}

/// `n` in `$random.alphabetic(n)`, capped.
fn count_arg(args: &str) -> usize {
    args.trim().parse::<usize>().unwrap_or(10).min(MAX_RANDOM_LEN)
}

/// A dynamic variable's value, or `None` if `name` is not one.
pub fn dynamic(name: &str) -> Option<String> {
    let name = name.trim();
    if !name.starts_with('$') {
        return None;
    }
    if let Some(var) = name.strip_prefix("$processEnv.").or_else(|| name.strip_prefix("$env.")) {
        return std::env::var(var).ok();
    }
    let (func, args) = match name.split_once('(') {
        Some((f, rest)) => (f.trim(), rest.strip_suffix(')').unwrap_or(rest)),
        None => (name, ""),
    };
    let mut rng = Rng::new();
    let random_string = |alphabet: &[u8], rng: &mut Rng| -> String {
        let n = count_arg(args);
        (0..n).map(|_| rng.pick(alphabet)).collect()
    };
    const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const HEX: &[u8] = b"0123456789abcdef";
    let value = match func {
        "$uuid" | "$random.uuid" => util::uuid_v4(),
        "$timestamp" => util::unix_seconds().to_string(),
        "$isoTimestamp" => util::iso_timestamp(util::unix_seconds()),
        "$randomInt" => rng.range(0, 1000).to_string(),
        "$random.integer" => {
            let mut it = args.split(',').map(|a| a.trim().parse::<i64>().ok());
            let lo = it.next().flatten().unwrap_or(0);
            let hi = it.next().flatten().unwrap_or(1000);
            rng.range(lo, hi).to_string()
        }
        "$random.float" => {
            let mut it = args.split(',').map(|a| a.trim().parse::<f64>().ok());
            let lo = it.next().flatten().unwrap_or(0.0);
            let hi = it.next().flatten().unwrap_or(1000.0);
            let unit = (rng.next() >> 11) as f64 / (1u64 << 53) as f64;
            format!("{}", lo + (hi - lo) * unit)
        }
        "$random.alphabetic" => random_string(ALPHA, &mut rng),
        "$random.alphanumeric" => random_string(ALNUM, &mut rng),
        "$random.hexadecimal" => random_string(HEX, &mut rng),
        "$random.email" => {
            let user: String = (0..8).map(|_| rng.pick(b"abcdefghijklmnopqrstuvwxyz")).collect();
            format!("{user}@example.com")
        }
        _ => return None,
    };
    Some(value)
}

/// A request ready to send.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRequest {
    pub name: Option<String>,
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub follow_redirects: bool,
    /// Variables that had no value (left as `{{name}}`).
    pub unresolved: Vec<String>,
}

impl ResolvedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The request as `.http` text (the body as text when it is UTF-8, else a size note).
    pub fn to_http_text(&self) -> String {
        let mut out = format!("{} {}\n", self.method, self.url);
        for (k, v) in &self.headers {
            out.push_str(&format!("{k}: {v}\n"));
        }
        if !self.body.is_empty() {
            out.push('\n');
            match std::str::from_utf8(&self.body) {
                Ok(t) => out.push_str(t),
                Err(_) => out.push_str(&format!("<binary body: {} bytes>", self.body.len())),
            }
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
}

/// Read a body file relative to `base_dir`.
pub fn read_body_file(base_dir: &Path, path: &str) -> Result<Vec<u8>, HttpError> {
    let p = base_dir.join(path.trim());
    let meta = std::fs::metadata(&p).map_err(|e| HttpError::Request(format!("cannot read {}: {e}", p.display())))?;
    if meta.len() > MAX_BODY_FILE {
        return Err(HttpError::Request(format!("{} is too large to send ({} bytes)", p.display(), meta.len())));
    }
    std::fs::read(&p).map_err(|e| HttpError::Request(format!("cannot read {}: {e}", p.display())))
}

/// `Authorization: Basic user password` or `Basic user:password` → base64 credentials (a
/// base64 token has neither spaces nor colons and is kept).
fn basic_auth(name: &str, value: String) -> String {
    if !name.trim().eq_ignore_ascii_case("authorization") {
        return value;
    }
    let Some(rest) = value.strip_prefix("Basic ") else { return value };
    let rest = rest.trim();
    let creds = match rest.split_once(char::is_whitespace) {
        Some((user, pass)) => format!("{user}:{}", pass.trim()),
        None if rest.contains(':') => rest.to_string(),
        None => return value,
    };
    format!("Basic {}", util::base64(creds.as_bytes()))
}

/// Substitute variables, read body files (relative to `base_dir`) and complete the URL
/// (`/path` with a `Host` header, a missing `http://` scheme).
pub fn resolve(req: &Request, vars: &Variables, base_dir: &Path) -> Result<ResolvedRequest, HttpError> {
    let mut unresolved = Vec::new();
    let mut sub = |t: &str| {
        let (s, u) = vars.substitute(t);
        for name in u {
            if !unresolved.contains(&name) {
                unresolved.push(name);
            }
        }
        s
    };
    let headers: Vec<(String, String)> = req.headers.iter().map(|(k, v)| (sub(k), basic_auth(k, sub(v)))).collect();
    let mut url = sub(&req.url);
    if url.starts_with('/') {
        if let Some((_, host)) = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")) {
            url = format!("http://{}{url}", host.trim());
        }
    } else if !url.contains("://") && !url.is_empty() {
        url = format!("http://{url}");
    }
    let mut body = Vec::new();
    for part in &req.body {
        match part {
            BodyPart::Text(t) => body.extend_from_slice(sub(t).as_bytes()),
            BodyPart::File(f) => body.extend_from_slice(&read_body_file(base_dir, &sub(f))?),
            BodyPart::FileWithVariables(f) => {
                let bytes = read_body_file(base_dir, &sub(f))?;
                body.extend_from_slice(sub(&String::from_utf8_lossy(&bytes)).as_bytes());
            }
        }
        if body.len() as u64 > MAX_BODY_FILE {
            return Err(HttpError::Request("the request body is too large".into()));
        }
    }
    if url.is_empty() {
        return Err(HttpError::Request("the request has no URL".into()));
    }
    Ok(ResolvedRequest {
        name: req.name.clone(),
        method: if req.method.is_empty() { "GET".into() } else { req.method.clone() },
        url,
        headers,
        body,
        follow_redirects: !req.no_redirect,
        unresolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    fn vars() -> Variables {
        let mut env = BTreeMap::new();
        env.insert("host".to_string(), "env-host".to_string());
        env.insert("port".to_string(), "8080".to_string());
        Variables::new(
            &[
                FileVariable { name: "host".into(), value: "file-host".into(), line: 0 },
                FileVariable { name: "base".into(), value: "http://{{host}}:{{port}}".into(), line: 1 },
                FileVariable { name: "loop".into(), value: "{{loop}}".into(), line: 2 },
            ],
            env,
        )
    }

    #[test]
    fn substitution_order_nesting_and_unknowns() {
        let v = vars();
        assert_eq!(v.substitute("{{base}}/x").0, "http://file-host:8080/x");
        assert_eq!(v.substitute("{{ port }}").0, "8080");
        let (s, u) = v.substitute("{{nope}} and {{nope}} {{other}}");
        assert_eq!(s, "{{nope}} and {{nope}} {{other}}");
        assert_eq!(u, vec!["nope".to_string(), "other".to_string()]);
        // Self-reference stops at the depth limit.
        let (s, u) = v.substitute("{{loop}}");
        assert_eq!(s, "{{loop}}");
        assert_eq!(u, vec!["loop".to_string()]);
        assert_eq!(v.substitute("{{unterminated").0, "{{unterminated");
        assert_eq!(v.lookup("base").as_deref(), Some("http://file-host:8080"));
    }

    #[test]
    fn dynamic_variables() {
        let v = Variables::default();
        assert_eq!(v.substitute("{{$uuid}}").0.len(), 36);
        assert!(v.substitute("{{$timestamp}}").0.parse::<u64>().is_ok());
        assert!(v.substitute("{{$isoTimestamp}}").0.ends_with('Z'));
        let n: i64 = v.substitute("{{$randomInt}}").0.parse().unwrap();
        assert!((0..1000).contains(&n));
        let n: i64 = v.substitute("{{$random.integer(5, 7)}}").0.parse().unwrap();
        assert!((5..7).contains(&n));
        assert_eq!(v.substitute("{{$random.alphabetic(12)}}").0.len(), 12);
        assert_eq!(v.substitute("{{$random.hexadecimal(99999999999)}}").0.len(), MAX_RANDOM_LEN);
        assert!(v.substitute("{{$random.email}}").0.contains('@'));
        assert_eq!(v.substitute("{{$nope}}").1, vec!["$nope".to_string()]);
    }

    #[test]
    fn resolve_builds_url_headers_and_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("body.json"), "{\"h\": \"{{host}}\"}").unwrap();
        let f = parse(
            "@host = example.org\nPOST /api\nHost: {{host}}\n\n< ./body.json\n###\nPUT {{host}}/x\n\n<@ ./body.json\n###\n# @no-redirect\nGET /missing\n\n< ./nope\n",
        );
        let v = Variables::new(&f.variables, BTreeMap::new());
        let r = resolve(&f.requests[0], &v, dir.path()).unwrap();
        assert_eq!(r.url, "http://example.org/api");
        assert_eq!(r.header("host"), Some("example.org"));
        assert_eq!(r.body, b"{\"h\": \"{{host}}\"}");
        assert!(r.follow_redirects);
        let r = resolve(&f.requests[1], &v, dir.path()).unwrap();
        assert_eq!(r.url, "http://example.org/x");
        assert_eq!(r.body, b"{\"h\": \"example.org\"}");
        assert!(resolve(&f.requests[2], &v, dir.path()).is_err());
        assert!(r.to_http_text().starts_with("PUT http://example.org/x\n\n{"));
        let f =
            parse("GET http://h\nAuthorization: Basic {{u}} secret\n###\nGET http://h\nAuthorization: Basic dTpw\n");
        let mut env = BTreeMap::new();
        env.insert("u".to_string(), "u".to_string());
        let v = Variables::new(&[], env);
        assert_eq!(
            resolve(&f.requests[0], &v, dir.path()).unwrap().header("Authorization"),
            Some("Basic dTpzZWNyZXQ=")
        );
        assert_eq!(resolve(&f.requests[1], &v, dir.path()).unwrap().header("Authorization"), Some("Basic dTpw"));
    }
}
