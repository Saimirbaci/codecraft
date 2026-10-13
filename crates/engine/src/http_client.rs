//! HTTP Client: `.http` files, environments, running requests and their responses/history (M11 track H).
//!
//! Requests run on a worker thread (one per Run / Run All, requests in order); results come back
//! through a channel drained by [`pump`]. Responses of this session are [`Exchange`]s shown in
//! Services › HTTP Requests; every run is also recorded in the project's history
//! (`.codecraft/httpRequests/`, see [`codecraft_http::history`]).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use codecraft_http::history::{self, HistoryEntry};
use codecraft_http::runner::{self, RunResult, RunSpec};
use codecraft_http::{BodyPart, ExecOptions, HttpFile, LineContext, Request, ResolvedRequest, Response};
use codecraft_syntax::Language;

pub use codecraft_http::format::human_size;

use crate::popup::{CompletionItem, CompletionPopup};
use crate::services::{self, ServicesSection};
use crate::{CommandError, Popup, Session};

/// Responses kept in Services (oldest dropped first).
const MAX_EXCHANGES: usize = 30;
/// Response body bytes kept in memory per response (the full body is in the history file).
const MAX_KEEP: usize = 4 << 20;
/// How often the environment names are re-read for the selector.
const ENV_REFRESH: Duration = Duration::from_secs(2);

/// One request sent in this session and its outcome.
#[derive(Clone, Debug, Serialize)]
pub struct Exchange {
    pub id: u64,
    pub title: String,
    pub method: String,
    pub url: String,
    /// The `.http` file and request line it came from.
    pub source: Option<(PathBuf, usize)>,
    pub env: Option<String>,
    pub state: ExchangeState,
}

#[derive(Clone, Debug, Serialize)]
pub enum ExchangeState {
    Running,
    Done(Box<Completed>),
    Failed(String),
}

/// A received response.
#[derive(Clone, Debug, Serialize)]
pub struct Completed {
    pub request: ResolvedRequest,
    pub response: Response,
    /// The body for display (pretty JSON, text, or a note for binary data; truncated).
    pub display: String,
    /// Where `>> file` wrote the body.
    pub output_file: Option<PathBuf>,
}

impl Exchange {
    pub fn is_running(&self) -> bool {
        matches!(self.state, ExchangeState::Running)
    }

    /// `200 OK`, `Running…` or the error.
    pub fn summary(&self) -> String {
        match &self.state {
            ExchangeState::Running => "Running…".into(),
            ExchangeState::Done(c) => format!("{} {}", c.response.status, c.response.status_text).trim().to_string(),
            ExchangeState::Failed(e) => e.clone(),
        }
    }

    /// The exchange for agents: summary fields, plus headers, body and request with `full`.
    pub fn to_json(&self, full: bool) -> Value {
        let mut v = json!({
            "id": self.id,
            "title": self.title,
            "method": self.method,
            "url": self.url,
            "source": self.source.as_ref().map(|(p, l)| json!({ "path": p, "line": l })),
            "env": self.env,
            "summary": self.summary(),
        });
        match &self.state {
            ExchangeState::Running => v["state"] = json!("running"),
            ExchangeState::Failed(e) => {
                v["state"] = json!("failed");
                v["error"] = json!(e);
            }
            ExchangeState::Done(c) => {
                v["state"] = json!("done");
                v["status"] = json!(c.response.status);
                v["statusText"] = json!(c.response.status_text);
                v["elapsedMs"] = json!(c.response.elapsed_ms);
                v["size"] = json!(c.response.body_len);
                v["url"] = json!(c.request.url);
                if !c.request.unresolved.is_empty() {
                    v["unresolved"] = json!(c.request.unresolved);
                }
                if full {
                    v["headers"] = json!(c.response.headers);
                    v["body"] = json!(c.display);
                    v["bodyFile"] = json!(c.response.body_file);
                    v["request"] = json!(c.request.to_http_text());
                    v["outputFile"] = json!(c.output_file);
                }
            }
        }
        v
    }
}

enum Msg {
    Started(u64),
    Done { id: u64, result: Box<RunResult>, display: String },
}

impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Msg::Started(id) => write!(f, "Started({id})"),
            Msg::Done { id, .. } => write!(f, "Done({id})"),
        }
    }
}

#[derive(Debug, Default)]
pub struct HttpState {
    /// Responses of this session, oldest first.
    pub exchanges: Vec<Exchange>,
    /// The exchange shown in Services.
    pub selected: Option<u64>,
    /// The environment requests run with (`None`: only `$shared` variables).
    pub environment: Option<String>,
    /// Environment names for the active request file (refreshed by [`refresh_environments`]).
    pub env_names: Vec<String>,
    env_checked: Option<(PathBuf, Instant)>,
    /// Convert cURL to HTTP Request: the dialog is open with this text.
    pub curl_dialog: Option<String>,
    /// The UI asks for a Postman collection file, then runs the import with `{path}`.
    pub pick_postman: bool,
    /// The UI asks where to save this exchange's response, then runs `http.saveResponse`.
    pub save_pick: Option<u64>,
    /// The project's request history, newest first (as last loaded).
    pub history: Vec<HistoryEntry>,
    /// Services › HTTP Requests shows the history instead of this session's responses.
    pub show_history: bool,
    /// Explicit history directory (tests and embedders); otherwise `<project>/.codecraft/httpRequests`.
    pub history_dir_override: Option<PathBuf>,
    next_id: u64,
    running: usize,
    channel: Option<(Sender<Msg>, Receiver<Msg>)>,
}

impl HttpState {
    /// Whether background work is in flight.
    pub fn busy(&self) -> bool {
        self.running > 0
    }

    pub fn exchange(&self, id: u64) -> Option<&Exchange> {
        self.exchanges.iter().find(|e| e.id == id)
    }

    /// The selected exchange, else the newest.
    pub fn current(&self) -> Option<&Exchange> {
        self.selected.and_then(|id| self.exchange(id)).or_else(|| self.exchanges.last())
    }
}

/// Drain background work. Returns true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let Some((_, rx)) = s.http.channel.as_ref() else { return false };
    let msgs: Vec<Msg> = rx.try_iter().take(256).collect();
    if msgs.is_empty() {
        return false;
    }
    for m in msgs {
        match m {
            Msg::Started(id) => s.http.selected = Some(id),
            Msg::Done { id, result, display } => finish(s, id, *result, display),
        }
    }
    true
}

fn finish(s: &mut Session, id: u64, result: RunResult, display: String) {
    s.http.running = s.http.running.saturating_sub(1);
    let RunResult { resolved, response, error, env_errors, history, output_file } = result;
    if let Some(h) = history {
        s.http.history.insert(0, h);
        s.http.history.truncate(history::MAX_ENTRIES);
    }
    let Some(ex) = s.http.exchanges.iter_mut().find(|e| e.id == id) else { return };
    if let Some(r) = &resolved {
        ex.url = r.url.clone();
    }
    let unresolved = resolved.as_ref().map(|r| r.unresolved.clone()).unwrap_or_default();
    match (response, resolved, error) {
        (Some(mut response), Some(request), _) => {
            if response.body_file.is_some() {
                // The full body is on disk; the display text is enough in memory.
                response.body = Vec::new();
            }
            s.status = format!(
                "{} {}: {} {} ({} ms, {})",
                ex.method,
                ex.url,
                response.status,
                response.status_text,
                response.elapsed_ms,
                codecraft_http::format::human_size(response.body_len)
            );
            ex.state = ExchangeState::Done(Box::new(Completed { request, response, display, output_file }));
        }
        (_, _, error) => {
            let mut e = error.unwrap_or_else(|| "the request failed".into());
            if !unresolved.is_empty() {
                e.push_str(&format!(" (unresolved variables: {})", unresolved.join(", ")));
            }
            s.status = format!("{} {}: {e}", ex.method, ex.url);
            ex.state = ExchangeState::Failed(e);
        }
    }
    if !env_errors.is_empty() {
        s.status = format!("{} — {}", s.status, env_errors.join("; "));
    } else if !unresolved.is_empty() && matches!(ex.state, ExchangeState::Done(_)) {
        s.status = format!("{} — unresolved variables: {}", s.status, unresolved.join(", "));
    }
}

// ------------------------------------------------------------------------------- files

/// Is the active editor an HTTP request file?
pub fn is_http_editor(s: &Session) -> bool {
    s.ws.active().is_some_and(|(_, d)| {
        d.language == Language::Http || d.path.as_deref().is_some_and(codecraft_http::is_http_file)
    })
}

/// `check`, or enabled in a request file (shared commands the HTTP Client also handles:
/// Basic completion, Run Context Configuration from the gutter).
pub(crate) fn or_http_editor(s: &Session, check: fn(&Session) -> Result<(), String>) -> Result<(), String> {
    if is_http_editor(s) { Ok(()) } else { check(s) }
}

/// The text of `path` (from its open editor, else from disk).
pub(crate) fn file_text(s: &Session, path: &Path) -> Result<String, CommandError> {
    match s.ws.doc_by_path(path).and_then(|id| s.ws.doc(id)) {
        Some(d) => Ok(d.doc.text()),
        None => codecraft_fs::read_text_file(path).map_err(|e| CommandError::Failed(e.to_string())),
    }
}

/// The request file to act on: `path` param, else the active editor's file; with its text and
/// the caret line (when it is the active file).
pub(crate) fn target_file(
    s: &Session,
    path: Option<PathBuf>,
) -> Result<(PathBuf, String, Option<usize>), CommandError> {
    let active = s
        .ws
        .active()
        .and_then(|(e, d)| d.path.clone().map(|p| (p, d.doc.text(), d.doc.char_to_line(e.selections.primary().head))));
    match (path, active) {
        (Some(p), Some((ap, text, line))) if crate::normalize_path(&p) == crate::normalize_path(&ap) => {
            Ok((ap, text, Some(line)))
        }
        (Some(p), _) => {
            let p = crate::normalize_path(&p);
            let text = file_text(s, &p)?;
            Ok((p, text, None))
        }
        (None, Some((p, text, line))) => Ok((p, text, Some(line))),
        (None, None) => Err(CommandError::Disabled("open an HTTP request file (.http)".into())),
    }
}

/// The project root when `path` is inside it.
fn root_for(s: &Session, path: &Path) -> Option<PathBuf> {
    s.ws.project.as_ref().map(|p| p.root.clone()).filter(|r| path.starts_with(r))
}

/// The history directory (needs a project).
pub fn history_dir(s: &Session) -> Option<PathBuf> {
    s.http.history_dir_override.clone().or_else(|| s.ws.project.as_ref().map(|p| history::dir(&p.root)))
}

/// Environments for a request file.
pub(crate) fn environments(s: &Session, path: &Path) -> codecraft_http::Environments {
    let dir = path.parent().unwrap_or(Path::new("."));
    codecraft_http::Environments::load(dir, root_for(s, path).as_deref())
}

/// Re-read the environment names for the active request file (throttled; cheap to call per frame).
pub fn refresh_environments(s: &mut Session) {
    let Some(path) = s.ws.active().and_then(|(_, d)| d.path.clone()).filter(|p| codecraft_http::is_http_file(p)) else {
        return;
    };
    let fresh = s.http.env_checked.as_ref().is_some_and(|(p, at)| *p == path && at.elapsed() < ENV_REFRESH);
    if fresh {
        return;
    }
    s.http.env_names = environments(s, &path).names();
    s.http.env_checked = Some((path, Instant::now()));
}

/// The variables a request in `path` sees with the selected environment.
pub(crate) fn variables(s: &Session, path: &Path, file: &HttpFile) -> codecraft_http::Variables {
    let envs = environments(s, path);
    codecraft_http::Variables::new(&file.variables, envs.variables(s.http.environment.as_deref()))
}

// ------------------------------------------------------------------------------- running

/// Run `requests` (in order) from the request file `path` on a worker thread; returns the
/// exchange ids.
pub(crate) fn start(
    s: &mut Session,
    path: &Path,
    file: &HttpFile,
    requests: Vec<Request>,
) -> Result<Vec<u64>, CommandError> {
    if requests.is_empty() {
        return Err(CommandError::Failed("no request to run here".into()));
    }
    let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let root = root_for(s, path);
    let history_dir = history_dir(s);
    let env = s.http.environment.clone();
    let tx = match &s.http.channel {
        Some((tx, _)) => tx.clone(),
        None => {
            let (tx, rx) = channel();
            s.http.channel = Some((tx.clone(), rx));
            tx
        }
    };
    let mut jobs = Vec::new();
    for r in requests {
        s.http.next_id = s.http.next_id.wrapping_add(1);
        let id = s.http.next_id;
        s.http.exchanges.push(Exchange {
            id,
            title: r.title(),
            method: r.method.clone(),
            url: r.url.clone(),
            source: Some((path.to_path_buf(), r.line)),
            env: env.clone(),
            state: ExchangeState::Running,
        });
        let spec = RunSpec {
            request: r,
            file_variables: file.variables.clone(),
            env: env.clone(),
            base_dir: base_dir.clone(),
            root: root.clone(),
            history_dir: history_dir.clone(),
            options: ExecOptions { max_keep: MAX_KEEP, ..ExecOptions::default() },
        };
        jobs.push((id, spec));
    }
    let ids: Vec<u64> = jobs.iter().map(|(id, _)| *id).collect();
    // Drop the oldest finished responses beyond the limit.
    while s.http.exchanges.len() > MAX_EXCHANGES {
        match s.http.exchanges.iter().position(|e| !e.is_running()) {
            Some(i) => {
                s.http.exchanges.remove(i);
            }
            None => break,
        }
    }
    let n = jobs.len();
    let spawned = std::thread::Builder::new().name("http-client".into()).spawn(move || {
        for (id, spec) in jobs {
            let _ = tx.send(Msg::Started(id));
            let result = runner::run(&spec);
            let display = result
                .response
                .as_ref()
                .map(|r| codecraft_http::format::display_body(r.content_type(), &r.body, r.body_len))
                .unwrap_or_default();
            if tx.send(Msg::Done { id, result: Box::new(result), display }).is_err() {
                return;
            }
        }
    });
    if let Err(e) = spawned {
        for id in &ids {
            if let Some(ex) = s.http.exchanges.iter_mut().find(|x| x.id == *id) {
                ex.state = ExchangeState::Failed(format!("cannot start a worker thread: {e}"));
            }
        }
        return Err(CommandError::Failed(format!("cannot start a worker thread: {e}")));
    }
    s.http.running = s.http.running.saturating_add(n);
    s.http.selected = ids.first().copied();
    s.http.show_history = false;
    services::show(s, ServicesSection::Http);
    s.status = if n == 1 { "Running the request…".into() } else { format!("Running {n} requests…") };
    Ok(ids)
}

/// Run the request at `line` of `path` (gutter ▶, Run Context Configuration).
pub(crate) fn run_at(s: &mut Session, path: &Path, line: usize) -> Result<Value, CommandError> {
    let text = file_text(s, path)?;
    let file = codecraft_http::parse(&text);
    let req = codecraft_http::parse::request_at(&file, line)
        .cloned()
        .ok_or_else(|| CommandError::Failed("no request here".into()))?;
    let title = req.title();
    let ids = start(s, path, &file, vec![req])?;
    Ok(json!({ "ids": ids, "title": title }))
}

// ------------------------------------------------------------------------------- completion

fn item(
    label: &str,
    insert: String,
    kind: &str,
    detail: Option<&str>,
    rank: u8,
    replace_start: usize,
    filter: &str,
) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        detail: detail.map(str::to_string),
        kind: kind.to_string(),
        filter_text: filter.to_string(),
        sort_text: format!("{rank}{}", label.to_lowercase()),
        insert_text: insert,
        replace_start: Some(replace_start),
        resolved: true,
        ..CompletionItem::default()
    }
}

/// Completion in a request file: methods on the request line, header names in the header
/// block, media types after `Content-Type:`/`Accept:`, variables after `{{`. With `auto`, only
/// opens when there is something to offer and stays quiet otherwise.
pub(crate) fn complete(s: &mut Session, auto: bool) -> Result<Value, CommandError> {
    let Some((e, d)) = s.ws.active() else { return Err(CommandError::Disabled("no editor is open".into())) };
    let editor = e.id;
    let caret = e.selections.primary().head.min(d.doc.len_chars());
    let line = d.doc.char_to_line(caret);
    let line_start = d.doc.line_start(line);
    let before: Vec<char> = d.doc.slice(line_start..caret).chars().collect();
    let text = d.doc.text();
    let path = d.path.clone();
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut anchor = before.len();
    while anchor > 0 && before.get(anchor - 1).is_some_and(|c| is_ident(*c)) {
        anchor -= 1;
    }
    let anchor_off = line_start + anchor;
    let mut items = Vec::new();
    // `{{name` before the caret: variables.
    let before_str: String = before.iter().collect();
    let var_start = before_str.rfind("{{").filter(|i| !before_str.get(*i..).unwrap_or_default().contains("}}"));
    if let Some(vs) = var_start {
        let typed: String = before_str.get(vs + 2..).unwrap_or_default().to_string();
        if typed.chars().all(|c| is_ident(c) || matches!(c, '$' | '.' | '-')) {
            let start = line_start + before_str.get(..vs + 2).unwrap_or_default().chars().count();
            let file = codecraft_http::parse(&text);
            let mut names: Vec<(String, &str)> = Vec::new();
            if let Some(p) = &path {
                let vars = variables(s, p, &file);
                names.extend(vars.file.keys().map(|k| (k.clone(), "file variable")));
                names.extend(vars.env.keys().map(|k| (k.clone(), "environment")));
            } else {
                names.extend(file.variables.iter().map(|v| (v.name.clone(), "file variable")));
            }
            names.extend(codecraft_http::DYNAMIC_VARIABLES.iter().map(|v| (v.to_string(), "dynamic")));
            // The popup filters on the identifier after the anchor; `$`/`.` parts match whole.
            let prefix_len = typed.chars().count().saturating_sub(caret.saturating_sub(anchor_off));
            let lead: String = typed.chars().take(prefix_len).collect();
            for (name, detail) in names {
                if !name.starts_with(&lead) {
                    continue;
                }
                let filter: String = name.chars().skip(prefix_len).collect();
                let rank = if detail == "dynamic" { 2 } else { 1 };
                items.push(item(&name, format!("{name}}}}}"), "variable", Some(detail), rank, start, &filter));
            }
        }
    } else {
        let ctx = codecraft_http::line_context(&text, line);
        let trimmed: String = before.iter().collect::<String>().trim_start().to_string();
        match ctx {
            LineContext::RequestLine if !trimmed.contains(char::is_whitespace) => {
                for m in codecraft_http::METHODS {
                    items.push(item(m, format!("{m} "), "keyword", Some("method"), 1, anchor_off, m));
                }
            }
            LineContext::Header if !trimmed.contains(':') && !before.first().is_some_and(|c| c.is_whitespace()) => {
                // Header names contain `-`: the popup filters on the part after the last `-`.
                let token_start = before.iter().rposition(|c| !(is_ident(*c) || *c == '-')).map_or(0, |i| i + 1);
                let lead: String = before.get(token_start..anchor).unwrap_or_default().iter().collect();
                for h in codecraft_http::HEADERS {
                    if h.len() < lead.len() || !h.get(..lead.len()).is_some_and(|p| p.eq_ignore_ascii_case(&lead)) {
                        continue;
                    }
                    let filter = h.get(lead.len()..).unwrap_or_default();
                    items.push(item(h, format!("{h}: "), "field", Some("header"), 1, line_start + token_start, filter));
                }
            }
            LineContext::Header => {
                let name = trimmed.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
                if matches!(name.as_str(), "content-type" | "accept") {
                    let value_start = before.iter().position(|c| *c == ':').map_or(anchor, |i| i + 1);
                    let value_start = (value_start..anchor)
                        .find(|&i| before.get(i).is_some_and(|c| !c.is_whitespace()))
                        .unwrap_or(anchor);
                    let lead: String = before.get(value_start..anchor).unwrap_or_default().iter().collect();
                    for m in codecraft_http::MEDIA_TYPES {
                        if !m.starts_with(&lead) {
                            continue;
                        }
                        let filter = m.get(lead.len()..).unwrap_or_default();
                        items.push(item(
                            m,
                            m.to_string(),
                            "value",
                            Some("media type"),
                            1,
                            line_start + value_start,
                            filter,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    if items.is_empty() {
        if !auto {
            s.status = "No suggestions".into();
        }
        return Ok(json!({ "items": 0 }));
    }
    let n = items.len();
    s.popup = Some(Popup::Completion(CompletionPopup {
        editor,
        anchor: anchor_off,
        items,
        auto,
        ..CompletionPopup::default()
    }));
    s.refresh_completion_filter();
    Ok(json!({ "items": n }))
}

/// After a typed char in a request file: open completion at the start of a method or header
/// name, a media type, or after `{{`.
pub(crate) fn after_typed(s: &mut Session, typed: char) {
    let Some((e, d)) = s.ws.active() else { return };
    if e.selections.len() != 1 {
        return;
    }
    let caret = e.selections.primary().head;
    let prev = caret.checked_sub(2).and_then(|i| d.doc.char_at(i));
    let word_start = (typed.is_alphabetic() || typed == '$') && !prev.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let braces = typed == '{' && prev == Some('{');
    if word_start || braces {
        let _ = complete(s, true);
    }
}

// ------------------------------------------------------------------------------- helpers

/// The `{{name}}` around the caret of the active editor.
pub(crate) fn variable_at_caret(s: &Session) -> Option<String> {
    let (e, d) = s.ws.active()?;
    let caret = e.selections.primary().head;
    let line = d.doc.char_to_line(caret);
    let text = d.doc.line_text(line)?;
    let col = caret.saturating_sub(d.doc.line_start(line));
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i + 1 < chars.len() {
        if chars.get(i) == Some(&'{') && chars.get(i + 1) == Some(&'{') {
            let close = (i + 2..chars.len().saturating_sub(1))
                .find(|&j| chars.get(j) == Some(&'}') && chars.get(j + 1) == Some(&'}'))?;
            if (i..=close + 2).contains(&col) {
                let name: String = chars.get(i + 2..close).unwrap_or_default().iter().collect();
                let name = name.trim().to_string();
                return (!name.is_empty() && !name.starts_with('$')).then_some(name);
            }
            i = close + 2;
        } else {
            i += 1;
        }
    }
    None
}

/// The body of a request as text for display (file parts as `< path`).
pub fn body_preview(req: &Request) -> String {
    req.body
        .iter()
        .map(|p| match p {
            BodyPart::Text(t) => t.clone(),
            BodyPart::File(f) => format!("< {f}"),
            BodyPart::FileWithVariables(f) => format!("<@ {f}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_workspace::Project;

    fn session(root: &Path) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.ws.project = Some(Project::new(root.to_path_buf()));
        s
    }

    fn open(s: &mut Session, path: &Path, text: &str, caret: usize) {
        std::fs::write(path, text).unwrap();
        s.open_file(path).unwrap();
        s.select_active(|_, _| codecraft_text::SelectionSet::caret(caret));
    }

    fn labels(s: &Session) -> Vec<String> {
        match &s.popup {
            Some(Popup::Completion(c)) => {
                c.visible.iter().filter_map(|i| c.items.get(*i)).map(|i| i.label.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    #[test]
    fn completes_methods_headers_media_types_and_variables() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        std::fs::write(dir.path().join("http-client.env.json"), r#"{"dev": {"host": "h"}}"#).unwrap();
        s.http.environment = Some("dev".into());
        let p = dir.path().join("a.http");

        open(&mut s, &p, "PO", 2);
        complete(&mut s, false).unwrap();
        assert_eq!(labels(&s), vec!["POST"]);
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "POST ");

        let set = |s: &mut Session, text: &str| {
            s.ws.active_mut().unwrap().1.doc.replace_all(text, &codecraft_text::SelectionSet::caret(0)).unwrap();
            s.select_active(|_, _| codecraft_text::SelectionSet::caret(text.chars().count()));
        };
        set(&mut s, "GET /x\nContent-T");
        complete(&mut s, false).unwrap();
        assert_eq!(labels(&s), vec!["Content-Type"]);
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "GET /x\nContent-Type: ");

        set(&mut s, "GET /x\nAccept: application/js");
        complete(&mut s, false).unwrap();
        assert_eq!(labels(&s), vec!["application/json"]);

        set(&mut s, "@base = x\nGET http://{{ho");
        complete(&mut s, false).unwrap();
        assert_eq!(labels(&s).first().map(String::as_str), Some("host"));
        s.execute("editor.completion.accept", &Value::Null).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "@base = x\nGET http://{{host}}");

        set(&mut s, "GET http://{{$random.al");
        complete(&mut s, false).unwrap();
        assert_eq!(labels(&s), vec!["$random.alphabetic(10)", "$random.alphanumeric(10)"]);

        // In a body nothing is offered.
        set(&mut s, "GET /x\n\nbod");
        s.popup = None;
        assert_eq!(complete(&mut s, true).unwrap()["items"], 0);
        assert!(s.popup.is_none());
    }

    #[test]
    fn variable_under_caret() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let p = dir.path().join("a.http");
        open(&mut s, &p, "GET http://{{ host }}/{{$uuid}}", 14);
        assert_eq!(variable_at_caret(&s).as_deref(), Some("host"));
        s.select_active(|_, _| codecraft_text::SelectionSet::caret(26));
        assert_eq!(variable_at_caret(&s), None);
    }
}
