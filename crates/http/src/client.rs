//! Sending a resolved request (blocking; callers run it on a worker thread).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::HttpError;
use crate::vars::ResolvedRequest;

/// How a request is sent and its response kept.
#[derive(Clone, Debug)]
pub struct ExecOptions {
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    /// Body bytes kept in memory (the rest is only written to `body_file`).
    pub max_keep: usize,
    /// Body bytes read at most (the response is cut there).
    pub max_total: u64,
    /// Write the whole body here too.
    pub body_file: Option<PathBuf>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            max_keep: 16 << 20,
            max_total: 1 << 30,
            body_file: None,
        }
    }
}

/// A received response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub status: u16,
    pub status_text: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
    /// The first [`ExecOptions::max_keep`] bytes of the body.
    #[serde(skip)]
    pub body: Vec<u8>,
    /// Bytes received.
    pub body_len: u64,
    /// The body was cut at [`ExecOptions::max_total`].
    pub cut: bool,
    pub elapsed_ms: u64,
    /// Where the body was written (when asked to).
    pub body_file: Option<PathBuf>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }

    /// `HTTP/1.1 200 OK`.
    pub fn status_line(&self) -> String {
        format!("{} {} {}", self.version, self.status, self.status_text).trim().to_string()
    }
}

/// Send `req` and read the response. HTTP error statuses (4xx/5xx) are responses, not errors.
pub fn execute(req: &ResolvedRequest, opts: &ExecOptions) -> Result<Response, HttpError> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(opts.connect_timeout)
        .timeout_read(opts.read_timeout)
        .timeout_write(opts.read_timeout)
        .redirects(if req.follow_redirects { 10 } else { 0 })
        .user_agent("CodeCraft HTTP Client")
        .build();
    let mut r = agent.request(&req.method, &req.url);
    for (k, v) in &req.headers {
        r = r.set(k, v);
    }
    let started = Instant::now();
    let result = if req.body.is_empty() && matches!(req.method.as_str(), "GET" | "HEAD" | "DELETE" | "OPTIONS") {
        r.call()
    } else {
        r.send_bytes(&req.body)
    };
    let resp = match result {
        Ok(resp) | Err(ureq::Error::Status(_, resp)) => resp,
        Err(ureq::Error::Transport(t)) => return Err(HttpError::Transport(t.to_string())),
    };
    let mut out = Response {
        status: resp.status(),
        status_text: resp.status_text().to_string(),
        version: resp.http_version().to_string(),
        ..Response::default()
    };
    let names = resp.headers_names();
    let mut done: Vec<String> = Vec::new();
    for name in names {
        if done.iter().any(|d| d.eq_ignore_ascii_case(&name)) {
            continue;
        }
        done.push(name.clone());
        for v in resp.all(&name) {
            out.headers.push((name.clone(), v.to_string()));
        }
    }
    let mut sink = match &opts.body_file {
        Some(p) => {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            Some(std::fs::File::create(p).map_err(|e| HttpError::Io(format!("cannot write {}: {e}", p.display())))?)
        }
        None => None,
    };
    let mut reader = resp.into_reader().take(opts.max_total.saturating_add(1));
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(HttpError::Transport(format!("reading the response failed: {e}"))),
        };
        let chunk = buf.get(..n).unwrap_or_default();
        let room = opts.max_total.saturating_sub(out.body_len);
        let chunk = if (n as u64) > room {
            out.cut = true;
            chunk.get(..room as usize).unwrap_or_default()
        } else {
            chunk
        };
        out.body_len = out.body_len.saturating_add(chunk.len() as u64);
        let keep = opts.max_keep.saturating_sub(out.body.len()).min(chunk.len());
        out.body.extend_from_slice(chunk.get(..keep).unwrap_or_default());
        if let Some(f) = sink.as_mut() {
            f.write_all(chunk).map_err(|e| HttpError::Io(format!("cannot write the response: {e}")))?;
        }
        if out.cut {
            break;
        }
    }
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    if sink.is_some() {
        out.body_file = opts.body_file.clone();
    }
    Ok(out)
}
