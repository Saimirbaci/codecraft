//! Import a Postman collection (v2.0/v2.1 JSON) as a `.http` file plus environment variables.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::HttpError;
use crate::parse::{BodyPart, Request, to_http_text};
use crate::util;

/// Deepest folder nesting followed.
const MAX_DEPTH: usize = 32;
/// Most requests imported.
const MAX_REQUESTS: usize = 10_000;

/// The result of an import.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    /// The collection's name (for the file name).
    pub name: String,
    /// The `.http` file text.
    pub http: String,
    /// Collection variables (for `http-client.env.json`).
    pub variables: BTreeMap<String, String>,
    pub requests: usize,
    /// Things that could not be converted (shown to the user).
    pub warnings: Vec<String>,
}

fn s(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn enabled(v: &Value) -> bool {
    !v.get("disabled").and_then(Value::as_bool).unwrap_or(false)
}

/// Postman dynamic variables → HTTP Client ones.
fn convert_vars(text: &str) -> String {
    text.replace("{{$guid}}", "{{$uuid}}").replace("{{$randomUUID}}", "{{$uuid}}")
}

/// A key/value list (`[{key, value, disabled}]`).
fn pairs(v: Option<&Value>) -> Vec<(String, String)> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|p| enabled(p))
                .map(|p| (s(p.get("key").unwrap_or(&Value::Null)), s(p.get("value").unwrap_or(&Value::Null))))
                .collect()
        })
        .unwrap_or_default()
}

/// The URL of a request (a string, or an object with `raw` or host/path/query parts).
fn url(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(u)) => u.clone(),
        Some(o @ Value::Object(_)) => {
            if let Some(raw) = o.get("raw").and_then(Value::as_str) {
                return raw.to_string();
            }
            let join = |key: &str, sep: &str| match o.get(key) {
                Some(Value::Array(a)) => a.iter().map(s).collect::<Vec<_>>().join(sep),
                Some(other) => s(other),
                None => String::new(),
            };
            let mut u = String::new();
            if let Some(p) = o.get("protocol").and_then(Value::as_str) {
                u.push_str(p);
                u.push_str("://");
            }
            u.push_str(&join("host", "."));
            if let Some(port) = o.get("port") {
                u.push(':');
                u.push_str(&s(port));
            }
            let path = join("path", "/");
            if !path.is_empty() {
                u.push('/');
                u.push_str(path.trim_start_matches('/'));
            }
            let query = pairs(o.get("query"));
            if !query.is_empty() {
                u.push('?');
                u.push_str(&query.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"));
            }
            u
        }
        _ => String::new(),
    }
}

/// `Authorization` header for a Postman auth object.
fn auth_header(auth: &Value, warnings: &mut Vec<String>, title: &str) -> Option<(String, String)> {
    let kind = auth.get("type").and_then(Value::as_str)?;
    let field = |name: &str| -> Option<String> {
        match auth.get(kind) {
            Some(Value::Array(a)) => a
                .iter()
                .find(|p| p.get("key").and_then(Value::as_str) == Some(name))
                .map(|p| s(p.get("value").unwrap_or(&Value::Null))),
            Some(Value::Object(o)) => o.get(name).map(s),
            _ => None,
        }
    };
    match kind {
        "noauth" => None,
        "bearer" => Some(("Authorization".into(), format!("Bearer {}", field("token").unwrap_or_default()))),
        "basic" => {
            let user = field("username").unwrap_or_default();
            let pass = field("password").unwrap_or_default();
            if user.contains("{{") || pass.contains("{{") {
                // Keep the variables visible; the HTTP Client encodes `Basic user pass` itself.
                Some(("Authorization".into(), format!("Basic {user} {pass}")))
            } else {
                Some(("Authorization".into(), format!("Basic {}", util::base64(format!("{user}:{pass}").as_bytes()))))
            }
        }
        "apikey" => {
            let key = field("key").unwrap_or_else(|| "X-API-Key".into());
            let value = field("value").unwrap_or_default();
            if field("in").as_deref() == Some("query") {
                warnings.push(format!("{title}: API key in the query string was not converted"));
                None
            } else {
                Some((key, value))
            }
        }
        other => {
            warnings.push(format!("{title}: {other} authentication was not converted"));
            None
        }
    }
}

fn body(b: &Value, headers: &mut Vec<(String, String)>, warnings: &mut Vec<String>, title: &str) -> Vec<BodyPart> {
    let has = |headers: &Vec<(String, String)>, n: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(n));
    match b.get("mode").and_then(Value::as_str).unwrap_or("") {
        "raw" => {
            let raw = b.get("raw").map(s).unwrap_or_default();
            if raw.is_empty() {
                return Vec::new();
            }
            let lang = b.pointer("/options/raw/language").and_then(Value::as_str).unwrap_or("");
            if !has(headers, "Content-Type") {
                let ct = match lang {
                    "json" => Some("application/json"),
                    "xml" => Some("application/xml"),
                    "html" => Some("text/html"),
                    "text" => Some("text/plain"),
                    "javascript" => Some("application/javascript"),
                    _ => None,
                };
                if let Some(ct) = ct {
                    headers.push(("Content-Type".into(), ct.into()));
                }
            }
            vec![BodyPart::Text(convert_vars(&raw))]
        }
        "urlencoded" => {
            if !has(headers, "Content-Type") {
                headers.push(("Content-Type".into(), "application/x-www-form-urlencoded".into()));
            }
            let fields = pairs(b.get("urlencoded"));
            if fields.is_empty() {
                return Vec::new();
            }
            vec![BodyPart::Text(fields.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&"))]
        }
        "formdata" => {
            const BOUNDARY: &str = "WebAppBoundary";
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case("Content-Type"));
            headers.push(("Content-Type".into(), format!("multipart/form-data; boundary={BOUNDARY}")));
            let mut parts = Vec::new();
            let mut text = String::new();
            for f in b.get("formdata").and_then(Value::as_array).into_iter().flatten().filter(|f| enabled(f)) {
                let key = f.get("key").map(s).unwrap_or_default();
                text.push_str(&format!("--{BOUNDARY}\n"));
                if f.get("type").and_then(Value::as_str) == Some("file") {
                    let src = match f.get("src") {
                        Some(Value::Array(a)) => a.first().map(s).unwrap_or_default(),
                        Some(v) => s(v),
                        None => String::new(),
                    };
                    let name = std::path::Path::new(&src)
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    text.push_str(&format!("Content-Disposition: form-data; name=\"{key}\"; filename=\"{name}\"\n\n"));
                    parts.push(BodyPart::Text(std::mem::take(&mut text)));
                    parts.push(BodyPart::File(src));
                    text.push('\n');
                } else {
                    let value = f.get("value").map(s).unwrap_or_default();
                    text.push_str(&format!("Content-Disposition: form-data; name=\"{key}\"\n\n{value}\n"));
                }
            }
            text.push_str(&format!("--{BOUNDARY}--"));
            parts.push(BodyPart::Text(text));
            parts
        }
        "file" => match b.pointer("/file/src").map(s) {
            Some(src) if !src.is_empty() => vec![BodyPart::File(src)],
            _ => Vec::new(),
        },
        "graphql" => {
            let query = b.pointer("/graphql/query").map(s).unwrap_or_default();
            let variables = b.pointer("/graphql/variables").map(s).filter(|v| !v.trim().is_empty());
            let vars: Value = variables.and_then(|v| serde_json::from_str(&v).ok()).unwrap_or(Value::Null);
            if !has(headers, "Content-Type") {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            let payload = serde_json::json!({ "query": query, "variables": vars });
            vec![BodyPart::Text(serde_json::to_string_pretty(&payload).unwrap_or_default())]
        }
        "" => Vec::new(),
        other => {
            warnings.push(format!("{title}: {other} body was not converted"));
            Vec::new()
        }
    }
}

fn walk(
    items: &Value,
    folder: &str,
    inherited_auth: Option<&Value>,
    depth: usize,
    out: &mut Imported,
    reqs: &mut Vec<Request>,
) {
    let Some(items) = items.as_array() else { return };
    if depth > MAX_DEPTH {
        out.warnings.push(format!("{folder}: folders nested too deeply"));
        return;
    }
    for item in items {
        if reqs.len() >= MAX_REQUESTS {
            out.warnings.push(format!("only the first {MAX_REQUESTS} requests were imported"));
            return;
        }
        let name = item.get("name").map(s).unwrap_or_default();
        let title = if folder.is_empty() { name.clone() } else { format!("{folder} / {name}") };
        let auth = item.get("auth").or(inherited_auth);
        if let Some(children) = item.get("item") {
            walk(children, &title, auth, depth + 1, out, reqs);
            continue;
        }
        let Some(req) = item.get("request") else { continue };
        // A request may be just its URL.
        let (method, url_v, headers_v, body_v, req_auth) = match req {
            Value::String(u) => ("GET".to_string(), Some(Value::String(u.clone())), None, None, None),
            r => (
                r.get("method").and_then(Value::as_str).unwrap_or("GET").to_uppercase(),
                r.get("url").cloned(),
                r.get("header").cloned(),
                r.get("body").cloned(),
                r.get("auth").cloned(),
            ),
        };
        let mut headers: Vec<(String, String)> =
            pairs(headers_v.as_ref()).into_iter().map(|(k, v)| (k, convert_vars(&v))).collect();
        if let Some(a) = req_auth.as_ref().or(auth)
            && let Some(h) = auth_header(a, &mut out.warnings, &title)
            && !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(&h.0))
        {
            headers.push(h);
        }
        let body = body_v.as_ref().map(|b| body(b, &mut headers, &mut out.warnings, &title)).unwrap_or_default();
        let url = convert_vars(&url(url_v.as_ref()));
        if url.is_empty() {
            out.warnings.push(format!("{title}: no URL"));
            continue;
        }
        reqs.push(Request { name: Some(title), method, url, headers, body, ..Request::default() });
    }
}

/// Convert a collection's JSON text.
pub fn import(json: &str) -> Result<Imported, HttpError> {
    let v: Value = serde_json::from_str(json).map_err(|e| HttpError::Parse(format!("not a JSON file: {e}")))?;
    let info = v.get("info").ok_or_else(|| HttpError::Parse("not a Postman collection (no `info`)".into()))?;
    let items = v.get("item").ok_or_else(|| HttpError::Parse("not a Postman collection (no `item`)".into()))?;
    let mut out =
        Imported { name: info.get("name").map(s).unwrap_or_else(|| "collection".into()), ..Imported::default() };
    for (k, val) in pairs(v.get("variable")) {
        out.variables.insert(k, val);
    }
    let mut reqs = Vec::new();
    walk(items, "", v.get("auth"), 0, &mut out, &mut reqs);
    let mut http = String::new();
    for r in &reqs {
        if !http.is_empty() {
            http.push('\n');
        }
        http.push_str(&to_http_text(r));
    }
    out.http = http;
    out.requests = reqs.len();
    Ok(out)
}

/// A file name for the collection (`My API` → `My API.http`, unsafe characters replaced).
pub fn file_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.') { c } else { '_' })
        .take(100)
        .collect();
    let clean = clean.trim().trim_matches('.');
    if clean.is_empty() { "collection.http".into() } else { format!("{clean}.http") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    const COLLECTION: &str = r#"{
      "info": { "name": "Pet Store", "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json" },
      "auth": { "type": "bearer", "bearer": [{ "key": "token", "value": "{{token}}", "type": "string" }] },
      "variable": [{ "key": "baseUrl", "value": "http://localhost:8080" }, { "key": "off", "value": "x", "disabled": true }],
      "item": [
        { "name": "Pets", "item": [
          { "name": "List pets", "request": { "method": "GET",
              "header": [{ "key": "Accept", "value": "application/json" }, { "key": "X-Off", "value": "1", "disabled": true }],
              "url": { "raw": "{{baseUrl}}/pets?limit=10", "host": ["{{baseUrl}}"], "path": ["pets"] } } },
          { "name": "Create pet", "request": { "method": "POST", "header": [],
              "body": { "mode": "raw", "raw": "{\"id\": \"{{$guid}}\"}", "options": { "raw": { "language": "json" } } },
              "url": "{{baseUrl}}/pets", "auth": { "type": "basic", "basic": [{"key": "username", "value": "u"}, {"key": "password", "value": "p"}] } } }
        ]},
        { "name": "Login", "request": { "method": "post",
            "body": { "mode": "urlencoded", "urlencoded": [{ "key": "user", "value": "a" }, { "key": "pw", "value": "b" }] },
            "url": { "protocol": "https", "host": ["example", "org"], "path": ["login"], "query": [{"key": "x", "value": "1"}] } } },
        { "name": "Upload", "request": { "method": "POST",
            "body": { "mode": "formdata", "formdata": [{ "key": "f", "type": "file", "src": "/tmp/a.png" }, { "key": "t", "value": "v", "type": "text" }] },
            "url": "{{baseUrl}}/upload", "auth": { "type": "oauth2" } } },
        { "name": "Bare", "request": "http://example.org/bare" }
      ]
    }"#;

    #[test]
    fn imports_folders_auth_bodies_and_variables() {
        let imp = import(COLLECTION).unwrap();
        assert_eq!(imp.name, "Pet Store");
        assert_eq!(imp.requests, 5);
        assert_eq!(imp.variables.get("baseUrl").map(String::as_str), Some("http://localhost:8080"));
        assert!(!imp.variables.contains_key("off"));
        assert_eq!(imp.warnings, vec!["Upload: oauth2 authentication was not converted".to_string()]);
        let f = parse(&imp.http);
        assert_eq!(f.requests.len(), 5);
        let r = &f.requests[0];
        assert_eq!(r.name.as_deref(), Some("Pets / List pets"));
        assert_eq!(r.url, "{{baseUrl}}/pets?limit=10");
        assert_eq!(
            r.headers,
            vec![("Accept".into(), "application/json".into()), ("Authorization".into(), "Bearer {{token}}".into())]
        );
        let r = &f.requests[1];
        assert_eq!(r.method, "POST");
        assert!(r.headers.contains(&("Authorization".into(), "Basic dTpw".into())));
        assert!(r.headers.contains(&("Content-Type".into(), "application/json".into())));
        assert_eq!(r.body, vec![BodyPart::Text("{\"id\": \"{{$uuid}}\"}".into())]);
        let r = &f.requests[2];
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://example.org/login?x=1");
        assert_eq!(r.body, vec![BodyPart::Text("user=a&pw=b".into())]);
        let r = &f.requests[3];
        assert!(r.body.contains(&BodyPart::File("/tmp/a.png".into())));
        assert_eq!(f.requests[4].url, "http://example.org/bare");
    }

    #[test]
    fn rejects_non_collections_and_names_files() {
        assert!(import("[]").is_err());
        assert!(import("{\"info\": {}}").is_err());
        assert!(import("nope").is_err());
        assert_eq!(import("{\"info\": {}, \"item\": 5}").unwrap().requests, 0);
        assert_eq!(file_name("My API / v2"), "My API _ v2.http");
        assert_eq!(file_name("../.."), "_.http");
        assert_eq!(file_name("..."), "collection.http");
    }
}
