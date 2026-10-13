//! HTTP Client commands: Tools › HTTP Client and running requests (M11 track H).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_http::{curl, env, history, postman};
use codecraft_text::SelectionSet;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::http_client::{self as http, ExchangeState};
use crate::params::{opt_path, opt_str, opt_usize};
use crate::services::{self, ServicesSection};
use crate::{CommandError, Effect, Session};

/// The template appended by Create Request in HTTP Client.
const NEW_REQUEST: &str = "###\nGET http://localhost:8080/api/items?id=99\nAccept: application/json\n\n";
/// Scratch file Create Request in HTTP Client appends to.
const GENERATED: &str = "generated-requests.http";
/// Scratch folder of the built-in example requests.
const COLLECTION_DIR: &str = "http-requests-collection";

/// Built-in examples (Open HTTP Requests Collection): (file name, text).
pub const EXAMPLES: &[(&str, &str)] = &[
    (
        "get-requests.http",
        "### A simple GET request\nGET https://httpbin.org/get\n\n### GET with query parameters\nGET https://httpbin.org/get?show_env=1&id={{$uuid}}\nAccept: application/json\n\n### A long URL split over lines\nGET https://httpbin.org/anything\n    ?page=1\n    &size=20\n\n### Follow a redirect, or not\n# @no-redirect\nGET https://httpbin.org/redirect/1\n",
    ),
    (
        "post-requests.http",
        "### Send a JSON body\nPOST https://httpbin.org/post\nContent-Type: application/json\n\n{\n  \"id\": \"{{$uuid}}\",\n  \"time\": {{$timestamp}},\n  \"value\": \"content\"\n}\n\n### Send a form\nPOST https://httpbin.org/post\nContent-Type: application/x-www-form-urlencoded\n\nid=999&value=content\n\n### Send the body from a file\nPOST https://httpbin.org/post\nContent-Type: application/json\n\n< ./input.json\n\n### Multipart form with a file\nPOST https://httpbin.org/post\nContent-Type: multipart/form-data; boundary=WebAppBoundary\n\n--WebAppBoundary\nContent-Disposition: form-data; name=\"element-name\"\nContent-Type: text/plain\n\nName\n--WebAppBoundary\nContent-Disposition: form-data; name=\"data\"; filename=\"input.json\"\nContent-Type: application/json\n\n< ./input.json\n--WebAppBoundary--\n",
    ),
    (
        "requests-with-authorization.http",
        "### Basic authorization (user and password are encoded for you)\nGET https://httpbin.org/basic-auth/user/passwd\nAuthorization: Basic user passwd\n\n### Bearer token from the environment\nGET https://httpbin.org/bearer\nAuthorization: Bearer {{token}}\n",
    ),
    (
        "requests-with-environment.http",
        "# Variables come from http-client.env.json and http-client.private.env.json next to this\n# file; pick the environment in Services › HTTP Requests.\n@path = anything\n\n### Uses {{host}} from the selected environment\nGET {{host}}/{{path}}\nAccept: application/json\n\n### Saves the response to a file\nGET {{host}}/json\n>> ./response.json\n",
    ),
    ("input.json", "{\n  \"name\": \"CodeCraft\",\n  \"kind\": \"example\"\n}\n"),
    (
        "http-client.env.json",
        "{\n  \"dev\": {\n    \"host\": \"https://httpbin.org\",\n    \"token\": \"dev-token\"\n  },\n  \"prod\": {\n    \"host\": \"https://httpbin.org\",\n    \"token\": \"prod-token\"\n  }\n}\n",
    ),
];

fn has_http_editor(s: &Session) -> Result<(), String> {
    if http::is_http_editor(s) { Ok(()) } else { Err("open an HTTP request file (.http)".into()) }
}

fn has_exchange(s: &Session) -> Result<(), String> {
    if s.http.exchanges.is_empty() { Err("no HTTP responses yet".into()) } else { Ok(()) }
}

fn failed(e: impl std::fmt::Display) -> CommandError {
    CommandError::Failed(e.to_string())
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------------------ Tools › HTTP Client
        spec(
            "tools.httpClient.createRequestInHttpClient",
            "Create Request in HTTP Client",
            None,
            "Append a request template to the scratch file generated-requests.http and open it; returns {path}",
            Context::Global,
            always,
            create_request,
        ),
        spec(
            "tools.httpClient.openHttpRequestsCollection",
            "Open HTTP Requests Collection",
            None,
            "{name?: string} — open a built-in example file (get-requests.http, post-requests.http, requests-with-authorization.http, requests-with-environment.http); returns {dir, files}",
            Context::Global,
            always,
            open_collection,
        ),
        spec(
            "tools.httpClient.showHttpRequestsHistory",
            "Show HTTP Requests History",
            None,
            "Open the project's request log (.codecraft/httpRequests/http-requests-log.http) and list the history in Services; returns the entries",
            Context::Global,
            has_project,
            show_history,
        ),
        spec(
            "tools.httpClient.convertCURLToHttpRequest",
            "Convert cURL to HTTP Request",
            None,
            "{curl?: string} — insert the request for a cURL command at the caret of the active .http file (else into a new scratch file); without `curl` the IDE asks for it",
            Context::Global,
            always,
            convert_curl,
        ),
        spec(
            "tools.httpClient.addToHttpClient.variableInPublicFile",
            "Variable in Public File",
            None,
            "{name?: string, value?: string, env?: string} — add the variable (default: the {{variable}} at the caret) to http-client.env.json under the environment (default: the selected one, else dev) and open the file",
            Context::Global,
            has_http_editor,
            |s, p| add_variable(s, p, false),
        ),
        spec(
            "tools.httpClient.addToHttpClient.variableInPrivateFile",
            "Variable in Private File",
            None,
            "{name?: string, value?: string, env?: string} — like Variable in Public File, in http-client.private.env.json",
            Context::Global,
            has_http_editor,
            |s, p| add_variable(s, p, true),
        ),
        spec(
            "tools.httpClient.importFromPostmanCollectionFile",
            "Import from Postman Collection File",
            None,
            "{path?: string, target?: dir} — convert a Postman collection (v2.0/v2.1 JSON) into <name>.http plus its variables in http-client.env.json ($shared) in the project root (or `target`); without a path the IDE asks for the file",
            Context::Global,
            always,
            import_postman,
        ),
        // ------------------------------------------------------------ running
        spec(
            "http.run",
            "Run HTTP Request",
            None,
            "{path?: string, line?: int} — run the request at the caret (or at the given 0-based line) with the selected environment; returns {ids}",
            Context::Global,
            always,
            run,
        ),
        spec(
            "http.runAll",
            "Run All Requests in File",
            None,
            "{path?: string} — run every request of the file, in order; returns {ids}",
            Context::Global,
            always,
            run_all,
        ),
        spec(
            "http.copyAsCurl",
            "Copy as cURL",
            None,
            "{path?: string, line?: int} — the request at the caret as a cURL command (variables substituted) on the clipboard; returns {curl}",
            Context::Global,
            always,
            copy_as_curl,
        ),
        spec(
            "http.environments",
            "HTTP Client Environments",
            None,
            "{path?: string} — environment names for a request file (default: the active one) and the selected one",
            Context::Global,
            always,
            environments,
        ),
        spec(
            "http.selectEnvironment",
            "Select HTTP Client Environment",
            None,
            "{env?: string|null} — the environment requests run with (null: none, only $shared variables)",
            Context::Global,
            always,
            select_environment,
        ),
        // ------------------------------------------------------------ responses
        spec(
            "http.responses",
            "HTTP Responses",
            None,
            "This session's requests and their status (newest last)",
            Context::Global,
            always,
            |s, _| Ok(json!(s.http.exchanges.iter().map(|e| e.to_json(false)).collect::<Vec<_>>())),
        ),
        spec(
            "http.response",
            "HTTP Response",
            None,
            "{id?: int} — one response in full (headers, body as displayed, request sent); default: the selected one",
            Context::Global,
            has_exchange,
            |s, p| {
                let id = exchange_id(s, p)?;
                let e = s.http.exchange(id).ok_or_else(|| CommandError::BadParams(format!("no response {id}")))?;
                Ok(e.to_json(true))
            },
        ),
        spec(
            "http.selectResponse",
            "Select HTTP Response",
            None,
            "{id: int}",
            Context::Global,
            has_exchange,
            |s, p| {
                let id = exchange_id(s, p)?;
                s.http.selected = Some(id);
                s.http.show_history = false;
                services::show(s, ServicesSection::Http);
                ok()
            },
        ),
        spec(
            "http.rerun",
            "Rerun HTTP Request",
            None,
            "{id?: int} — run the response's request again from its file",
            Context::Global,
            has_exchange,
            rerun,
        ),
        spec(
            "http.saveResponse",
            "Save Response to File",
            None,
            "{id?: int, path?: string} — save the response body; without a path the IDE asks where",
            Context::Global,
            has_exchange,
            save_response,
        ),
        spec(
            "http.openResponseInEditor",
            "Open Response in Editor",
            None,
            "{id?: int} — open the response body in an editor tab",
            Context::Global,
            has_exchange,
            open_response,
        ),
        spec(
            "http.clearResponses",
            "Clear HTTP Responses",
            None,
            "Forget this session's finished responses",
            Context::Global,
            always,
            |s, _| {
                s.http.exchanges.retain(|e| e.is_running());
                s.http.selected = None;
                ok()
            },
        ),
        spec(
            "http.openHistoryEntry",
            "Open HTTP History Entry",
            None,
            "{index: int} — open the response body of a history entry (0 = newest)",
            Context::Global,
            has_project,
            open_history_entry,
        ),
        spec(
            "http.clearHistory",
            "Clear HTTP Requests History",
            None,
            "Delete the project's request history and saved responses",
            Context::Global,
            has_project,
            |s, _| {
                let dir = http::history_dir(s).ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
                history::clear(&dir).map_err(failed)?;
                s.http.history.clear();
                ok()
            },
        ),
        spec("http.closeCurlDialog", "Close Convert cURL Dialog", None, "", Context::Global, always, |s, _| {
            s.http.curl_dialog = None;
            ok()
        }),
        spec(
            "http.complete",
            "HTTP Request Completion",
            None,
            "Completion in a .http file: methods, header names, media types, {{variables}}",
            Context::Editor,
            has_http_editor,
            |s, _| http::complete(s, false),
        ),
    ]
}

fn exchange_id(s: &Session, p: &Value) -> Result<u64, CommandError> {
    match opt_usize(p, "id")? {
        Some(id) => Ok(id as u64),
        None => s.http.current().map(|e| e.id).ok_or_else(|| CommandError::Disabled("no HTTP responses yet".into())),
    }
}

fn scratch_dir(s: &Session) -> Result<PathBuf, CommandError> {
    let dir = s.scratch.dir().ok_or_else(|| failed("cannot find the home directory; set CODECRAFT_HOME"))?;
    std::fs::create_dir_all(&dir).map_err(|e| failed(format!("cannot create {}: {e}", dir.display())))?;
    Ok(dir)
}

/// Open `path` with the caret at the start of `line`.
fn open_at(s: &mut Session, path: &Path, line: usize) -> Result<(), CommandError> {
    s.open_file(path)?;
    s.select_active(|d, _| SelectionSet::caret(d.line_start(line.min(d.len_lines().saturating_sub(1)))));
    Ok(())
}

fn create_request(s: &mut Session, _: &Value) -> CmdResult {
    let path = scratch_dir(s)?.join(GENERATED);
    let mut text = if path.exists() {
        std::fs::read_to_string(&path).map_err(|e| failed(format!("cannot read {}: {e}", path.display())))?
    } else {
        String::new()
    };
    if !text.is_empty() && !text.ends_with("\n\n") {
        text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
    }
    let line = text.matches('\n').count() + 1;
    text.push_str(NEW_REQUEST);
    // An open editor gets the template as an edit (undoable); otherwise write the file.
    if let Some(id) = s.ws.doc_by_path(&crate::normalize_path(&path)) {
        s.open_file(&path)?;
        let current = s.ws.doc(id).map(|d| d.doc.text()).unwrap_or_default();
        let mut add = String::new();
        if !current.is_empty() && !current.ends_with("\n\n") {
            add.push_str(if current.ends_with('\n') { "\n" } else { "\n\n" });
        }
        add.push_str(NEW_REQUEST);
        let line = (current.clone() + &add).matches('\n').count().saturating_sub(NEW_REQUEST.matches('\n').count()) + 1;
        s.edit_active(codecraft_text::EditGroup::Standalone, |d, _, _| {
            codecraft_text::ops::insert_text(d, &SelectionSet::caret(d.len_chars()), &add)
        })?;
        s.select_active(|d, _| SelectionSet::caret(d.line_start(line)));
        return Ok(json!({ "path": path }));
    }
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(failed)?;
    open_at(s, &path, line)?;
    Ok(json!({ "path": path }))
}

fn open_collection(s: &mut Session, p: &Value) -> CmdResult {
    let dir = scratch_dir(s)?.join(COLLECTION_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| failed(format!("cannot create {}: {e}", dir.display())))?;
    for (name, text) in EXAMPLES {
        let path = dir.join(name);
        if !path.exists() {
            codecraft_fs::write_text_file_atomic(&path, text).map_err(failed)?;
        }
    }
    let files: Vec<&str> = EXAMPLES.iter().map(|(n, _)| *n).filter(|n| n.ends_with(".http")).collect();
    let name = opt_str(p, "name").unwrap_or("get-requests.http");
    let name = files
        .iter()
        .find(|f| **f == name || f.trim_end_matches(".http") == name)
        .ok_or_else(|| CommandError::BadParams(format!("no example `{name}`; one of {}", files.join(", "))))?;
    open_at(s, &dir.join(name), 0)?;
    Ok(json!({ "dir": dir, "files": files }))
}

fn show_history(s: &mut Session, _: &Value) -> CmdResult {
    let dir = http::history_dir(s).ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    s.http.history = history::load(&dir);
    s.http.show_history = true;
    services::show(s, ServicesSection::Http);
    let log = dir.join(history::LOG);
    if log.is_file() {
        s.open_file(&log)?;
    } else {
        s.status = "No requests in the history yet".into();
    }
    Ok(json!({ "dir": dir, "entries": s.http.history }))
}

fn open_history_entry(s: &mut Session, p: &Value) -> CmdResult {
    let index = crate::params::req_usize(p, "index")?;
    let dir = http::history_dir(s).ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    if s.http.history.is_empty() {
        s.http.history = history::load(&dir);
    }
    let entry =
        s.http.history.get(index).ok_or_else(|| CommandError::BadParams(format!("no history entry {index}")))?;
    let file =
        entry.body_file.clone().filter(|f| !f.contains(['/', '\\']) && !f.starts_with('.')).ok_or_else(|| {
            failed(entry.error.clone().unwrap_or_else(|| "this request has no saved response".into()))
        })?;
    let path = dir.join(file);
    if !path.is_file() {
        return Err(failed(format!("{} no longer exists", path.display())));
    }
    s.open_file(&path)?;
    Ok(json!({ "path": path }))
}

/// Insert `text` at the start of the caret line of the active .http editor, else into a new
/// scratch `.http` file.
fn insert_request(s: &mut Session, text: &str) -> Result<PathBuf, CommandError> {
    if http::is_http_editor(s)
        && let Some(path) = s.ws.active().and_then(|(_, d)| d.path.clone())
    {
        s.edit_active(codecraft_text::EditGroup::Standalone, |d, sel, _| {
            let line = d.char_to_line(sel.primary().head);
            let at = d.line_start(line);
            let mut t = text.to_string();
            if !t.ends_with("\n\n") {
                t.push('\n');
            }
            codecraft_text::ops::insert_text(d, &SelectionSet::caret(at), &t)
        })?;
        return Ok(path);
    }
    let dir = scratch_dir(s)?;
    let path = crate::scratch::next_name(&dir, "http").ok_or_else(|| failed("too many scratch files"))?;
    codecraft_fs::write_text_file_atomic(&path, text).map_err(failed)?;
    open_at(s, &path, 0)?;
    Ok(path)
}

fn convert_curl(s: &mut Session, p: &Value) -> CmdResult {
    let Some(cmd) = opt_str(p, "curl") else {
        s.http.curl_dialog = Some(String::new());
        return Ok(json!({ "dialog": true }));
    };
    let req = curl::parse_curl(cmd).map_err(|e| CommandError::BadParams(e.to_string()))?;
    let text = codecraft_http::parse::to_http_text(&req);
    let path = insert_request(s, &text)?;
    s.http.curl_dialog = None;
    Ok(json!({ "path": path, "request": text }))
}

fn add_variable(s: &mut Session, p: &Value, private: bool) -> CmdResult {
    let name = match opt_str(p, "name") {
        Some(n) => n.trim().to_string(),
        None => http::variable_at_caret(s)
            .ok_or_else(|| CommandError::BadParams("put the caret on a {{variable}} or pass `name`".into()))?,
    };
    if name.is_empty() || name.contains(['{', '}']) {
        return Err(CommandError::BadParams(format!("invalid variable name `{name}`")));
    }
    let value = opt_str(p, "value").unwrap_or("");
    let env_name =
        opt_str(p, "env").map(str::to_string).or_else(|| s.http.environment.clone()).unwrap_or_else(|| "dev".into());
    let (path, _, _) = http::target_file(s, None)?;
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let file = if private { env::PRIVATE_FILE } else { env::PUBLIC_FILE };
    // An existing environment file up to the project root, else one next to the request file.
    let root = s.ws.project.as_ref().map(|p| p.root.clone()).filter(|r| dir.starts_with(r));
    let target = env::find_file(&dir, root.as_deref(), file).unwrap_or_else(|| dir.join(file));
    let line = env::set_variable(&target, &env_name, &name, value).map_err(failed)?;
    open_at(s, &target, line)?;
    if s.http.environment.is_none() {
        s.http.environment = Some(env_name.clone());
    }
    Ok(json!({ "path": target, "env": env_name, "name": name }))
}

fn import_postman(s: &mut Session, p: &Value) -> CmdResult {
    let Some(src) = opt_path(p, "path") else {
        s.http.pick_postman = true;
        return Ok(json!({ "pick": true }));
    };
    let meta = std::fs::metadata(&src).map_err(|e| failed(format!("cannot read {}: {e}", src.display())))?;
    if meta.len() > 64 << 20 {
        return Err(failed(format!("{} is too large", src.display())));
    }
    let json_text = std::fs::read_to_string(&src).map_err(|e| failed(format!("cannot read {}: {e}", src.display())))?;
    let imported = postman::import(&json_text).map_err(|e| CommandError::BadParams(e.to_string()))?;
    let target = opt_path(p, "target")
        .or_else(|| s.ws.project.as_ref().map(|p| p.root.clone()))
        .or_else(|| src.parent().map(Path::to_path_buf))
        .ok_or_else(|| CommandError::BadParams("pass `target`".into()))?;
    std::fs::create_dir_all(&target).map_err(|e| failed(format!("cannot create {}: {e}", target.display())))?;
    let base = postman::file_name(&imported.name);
    let stem = base.trim_end_matches(".http").to_string();
    let out = (0..1000)
        .map(|i| if i == 0 { target.join(&base) } else { target.join(format!("{stem}-{i}.http")) })
        .find(|p| !p.exists())
        .ok_or_else(|| failed("too many files with the collection's name"))?;
    codecraft_fs::write_text_file_atomic(&out, &imported.http).map_err(failed)?;
    if !imported.variables.is_empty() {
        env::merge_variables(&target.join(env::PUBLIC_FILE), env::SHARED, &imported.variables).map_err(failed)?;
    }
    s.http.pick_postman = false;
    open_at(s, &out, 0)?;
    s.status = format!("Imported {} requests from {}", imported.requests, imported.name);
    if !imported.warnings.is_empty() {
        s.status.push_str(&format!(" ({} not converted: {})", imported.warnings.len(), imported.warnings.join("; ")));
    }
    Ok(json!({
        "path": out,
        "requests": imported.requests,
        "variables": imported.variables.len(),
        "warnings": imported.warnings,
    }))
}

fn run(s: &mut Session, p: &Value) -> CmdResult {
    let (path, _, caret_line) = http::target_file(s, opt_path(p, "path"))?;
    let line = opt_usize(p, "line")?.or(caret_line).unwrap_or(0);
    http::run_at(s, &path, line)
}

fn run_all(s: &mut Session, p: &Value) -> CmdResult {
    let (path, text, _) = http::target_file(s, opt_path(p, "path"))?;
    let file = codecraft_http::parse(&text);
    let requests = file.requests.clone();
    let ids = http::start(s, &path, &file, requests)?;
    Ok(json!({ "ids": ids }))
}

fn copy_as_curl(s: &mut Session, p: &Value) -> CmdResult {
    let (path, text, caret_line) = http::target_file(s, opt_path(p, "path"))?;
    let line = opt_usize(p, "line")?.or(caret_line).unwrap_or(0);
    let file = codecraft_http::parse(&text);
    let req = codecraft_http::parse::request_at(&file, line).ok_or_else(|| failed("no request here"))?;
    let vars = http::variables(s, &path, &file);
    let cmd = curl::to_curl(req, &vars);
    s.effects.push(Effect::CopyToClipboard(cmd.clone()));
    s.status = "Copied the request as a cURL command".into();
    Ok(json!({ "curl": cmd }))
}

fn environments(s: &mut Session, p: &Value) -> CmdResult {
    let (path, _, _) = http::target_file(s, opt_path(p, "path"))?;
    let envs = http::environments(s, &path);
    Ok(json!({ "names": envs.names(), "selected": s.http.environment, "files": envs.files, "errors": envs.errors }))
}

fn select_environment(s: &mut Session, p: &Value) -> CmdResult {
    s.http.environment = match p.get("env") {
        None | Some(Value::Null) => None,
        Some(Value::String(e)) if e.trim().is_empty() => None,
        Some(Value::String(e)) => Some(e.trim().to_string()),
        Some(_) => return Err(CommandError::BadParams("`env` must be a string or null".into())),
    };
    ok()
}

fn rerun(s: &mut Session, p: &Value) -> CmdResult {
    let id = exchange_id(s, p)?;
    let (path, line) = s
        .http
        .exchange(id)
        .and_then(|e| e.source.clone())
        .ok_or_else(|| CommandError::BadParams(format!("no response {id}")))?;
    http::run_at(s, &path, line)
}

fn save_response(s: &mut Session, p: &Value) -> CmdResult {
    let id = exchange_id(s, p)?;
    let Some(target) = opt_path(p, "path") else {
        s.http.save_pick = Some(id);
        return Ok(json!({ "pick": true }));
    };
    s.http.save_pick = None;
    let e = s.http.exchange(id).ok_or_else(|| CommandError::BadParams(format!("no response {id}")))?;
    let ExchangeState::Done(c) = &e.state else { return Err(failed("the request has no response")) };
    match &c.response.body_file {
        Some(src) => std::fs::copy(src, &target).map(|_| ()),
        None => std::fs::write(&target, &c.response.body),
    }
    .map_err(|e| failed(format!("cannot write {}: {e}", target.display())))?;
    s.status = format!("Saved the response to {}", target.display());
    Ok(json!({ "path": target }))
}

fn open_response(s: &mut Session, p: &Value) -> CmdResult {
    let id = exchange_id(s, p)?;
    let e = s.http.exchange(id).ok_or_else(|| CommandError::BadParams(format!("no response {id}")))?;
    let ExchangeState::Done(c) = &e.state else { return Err(failed("the request has no response")) };
    let path = match &c.response.body_file {
        Some(f) => f.clone(),
        None => {
            let ext = codecraft_http::format::BodyKind::detect(c.response.content_type(), &c.response.body).extension();
            let body = c.response.body.clone();
            let path = scratch_dir(s)?.join(format!("http-response-{id}.{ext}"));
            std::fs::write(&path, body).map_err(|e| failed(format!("cannot write {}: {e}", path.display())))?;
            path
        }
    };
    s.open_file(&path)?;
    Ok(json!({ "path": path }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_workspace::Project;

    fn session(root: &Path) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.scratch.dir_override = Some(root.join("scratches"));
        s
    }

    #[test]
    fn create_request_and_collection_use_scratches() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let r = s.execute("tools.httpClient.createRequestInHttpClient", &Value::Null).unwrap();
        let path = PathBuf::from(r["path"].as_str().unwrap());
        assert!(path.ends_with(GENERATED));
        s.execute("tools.httpClient.createRequestInHttpClient", &Value::Null).unwrap();
        let text = s.ws.active().unwrap().1.doc.text();
        assert_eq!(text.matches("###").count(), 2);
        assert_eq!(codecraft_http::parse(&text).requests.len(), 2);

        let r = s.execute("tools.httpClient.openHttpRequestsCollection", &json!({ "name": "post-requests" })).unwrap();
        assert_eq!(r["files"].as_array().unwrap().len(), 4);
        let (_, d) = s.ws.active().unwrap();
        assert!(d.path.as_ref().unwrap().ends_with("post-requests.http"));
        assert_eq!(d.language, codecraft_syntax::Language::Http);
        assert!(s.execute("tools.httpClient.openHttpRequestsCollection", &json!({ "name": "nope" })).is_err());
        // Every example parses into requests.
        for (name, text) in EXAMPLES.iter().filter(|(n, _)| n.ends_with(".http")) {
            assert!(!codecraft_http::parse(text).requests.is_empty(), "{name}");
        }
    }

    #[test]
    fn curl_conversion_and_copy_as_curl() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        assert_eq!(s.execute("tools.httpClient.convertCURLToHttpRequest", &Value::Null).unwrap()["dialog"], true);
        assert!(s.http.curl_dialog.is_some());
        assert!(s.execute("tools.httpClient.convertCURLToHttpRequest", &json!({ "curl": "wget x" })).is_err());
        let r = s
            .execute(
                "tools.httpClient.convertCURLToHttpRequest",
                &json!({ "curl": "curl -H 'X-A: 1' http://example.org/a" }),
            )
            .unwrap();
        assert!(s.http.curl_dialog.is_none());
        assert!(r["path"].as_str().unwrap().ends_with(".http"));
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "###\nGET http://example.org/a\nX-A: 1\n");
        // A second conversion goes into the same (active) file at the caret line.
        s.execute(
            "tools.httpClient.convertCURLToHttpRequest",
            &json!({ "curl": "curl -X DELETE http://example.org/b" }),
        )
        .unwrap();
        let text = s.ws.active().unwrap().1.doc.text();
        assert_eq!(codecraft_http::parse(&text).requests.len(), 2);

        s.select_active(|_, _| SelectionSet::caret(0));
        let r = s.execute("http.copyAsCurl", &Value::Null).unwrap();
        assert_eq!(r["curl"], "curl -X DELETE http://example.org/b");
        assert!(s.effects.iter().any(|e| matches!(e, Effect::CopyToClipboard(c) if c.contains("DELETE"))));
    }

    #[test]
    fn variables_and_environments() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        s.ws.project = Some(Project::new(dir.path().to_path_buf()));
        let req = dir.path().join("api.http");
        std::fs::write(&req, "GET http://{{host}}/x\n").unwrap();
        s.open_file(&req).unwrap();
        s.select_active(|_, _| SelectionSet::caret(14));
        let r = s
            .execute("tools.httpClient.addToHttpClient.variableInPublicFile", &json!({ "value": "localhost" }))
            .unwrap();
        assert_eq!(r["name"], "host");
        assert_eq!(r["env"], "dev");
        assert!(dir.path().join(env::PUBLIC_FILE).is_file());
        s.open_file(&req).unwrap();
        s.execute(
            "tools.httpClient.addToHttpClient.variableInPrivateFile",
            &json!({ "name": "token", "value": "t", "env": "prod" }),
        )
        .unwrap();
        s.open_file(&req).unwrap();
        let r = s.execute("http.environments", &Value::Null).unwrap();
        assert_eq!(r["names"], json!(["dev", "prod"]));
        assert_eq!(r["selected"], "dev");
        s.execute("http.selectEnvironment", &json!({ "env": null })).unwrap();
        assert_eq!(s.http.environment, None);
        assert!(s.execute("http.selectEnvironment", &json!({ "env": 3 })).is_err());
        s.execute("http.selectEnvironment", &json!({ "env": "dev" })).unwrap();
        let r = s.execute("http.copyAsCurl", &Value::Null).unwrap();
        assert_eq!(r["curl"], "curl http://localhost/x");
        crate::http_client::refresh_environments(&mut s);
        assert_eq!(s.http.env_names, vec!["dev".to_string(), "prod".to_string()]);
    }

    #[test]
    fn postman_import_writes_http_and_env_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        assert_eq!(s.execute("tools.httpClient.importFromPostmanCollectionFile", &Value::Null).unwrap()["pick"], true);
        assert!(s.http.pick_postman);
        let col = dir.path().join("c.json");
        std::fs::write(
            &col,
            r#"{"info": {"name": "Demo API"}, "variable": [{"key": "base", "value": "http://localhost"}],
                "item": [{"name": "Ping", "request": {"method": "GET", "url": "{{base}}/ping"}}]}"#,
        )
        .unwrap();
        let out = dir.path().join("out");
        let r = s
            .execute("tools.httpClient.importFromPostmanCollectionFile", &json!({ "path": col, "target": out }))
            .unwrap();
        assert_eq!(r["requests"], 1);
        assert!(!s.http.pick_postman);
        let http_file = out.join("Demo API.http");
        assert_eq!(std::fs::read_to_string(&http_file).unwrap(), "### Ping\nGET {{base}}/ping\n");
        let envs = codecraft_http::Environments::load(&out, None);
        assert_eq!(envs.variables(None).get("base").map(String::as_str), Some("http://localhost"));
        // Importing again does not overwrite.
        let r = s
            .execute("tools.httpClient.importFromPostmanCollectionFile", &json!({ "path": col, "target": out }))
            .unwrap();
        assert!(r["path"].as_str().unwrap().ends_with("Demo API-1.http"));
        std::fs::write(&col, "{}").unwrap();
        assert!(s.execute("tools.httpClient.importFromPostmanCollectionFile", &json!({ "path": col })).is_err());
    }

    /// A tiny HTTP server on 127.0.0.1 answering `n` connections: `/json` with JSON, anything
    /// else with an echo of the method and body.
    fn server(n: usize) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{BufRead, BufReader, Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for _ in 0..n {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                let mut parts = first.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let out = if path == "/json" {
                    r#"{"ok":true,"n":[1,2]}"#.to_string()
                } else {
                    json!({ "method": method, "body": String::from_utf8_lossy(&body) }).to_string()
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    out.len()
                );
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(out.as_bytes()).unwrap();
            }
        });
        (format!("127.0.0.1:{port}"), handle)
    }

    fn wait(s: &mut Session) {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while s.http.busy() && std::time::Instant::now() < until {
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        s.pump();
        assert!(!s.http.busy(), "requests did not finish");
    }

    #[test]
    fn runs_requests_from_the_gutter_and_records_history() {
        let (host, server) = server(4);
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        s.ws.project = Some(Project::new(dir.path().to_path_buf()));
        let req = dir.path().join("api.http");
        let text = format!(
            "@host = {host}\n### one\nGET http://{{{{host}}}}/json\n\n### two\nPOST http://{{{{host}}}}/echo\nContent-Type: text/plain\n\nhello\n"
        );
        std::fs::write(&req, &text).unwrap();
        s.open_file(&req).unwrap();
        assert_eq!(crate::run_configs::gutter_run_lines(&req, &text), vec![2, 5]);

        // The gutter ▶ runs `run.contextConfiguration` with the request line.
        let r = s.execute("run.contextConfiguration", &json!({ "path": req, "line": 2 })).unwrap();
        assert_eq!(r["title"], "one");
        assert!(s.http.busy());
        assert_eq!(s.services.section, ServicesSection::Http);
        wait(&mut s);
        let resp = s.execute("http.response", &Value::Null).unwrap();
        assert_eq!(resp["status"], 200);
        assert_eq!(resp["body"], "{\n  \"ok\": true,\n  \"n\": [\n    1,\n    2\n  ]\n}");
        assert!(resp["bodyFile"].as_str().unwrap().ends_with(".200.json"));
        assert!(s.status.contains("200 OK"), "{}", s.status);

        let r = s.execute("http.runAll", &Value::Null).unwrap();
        assert_eq!(r["ids"].as_array().unwrap().len(), 2);
        wait(&mut s);
        assert_eq!(s.http.exchanges.len(), 3);
        let last = s.execute("http.response", &json!({ "id": s.http.exchanges[2].id })).unwrap();
        assert_eq!(last["state"], "done");
        assert!(last["body"].as_str().unwrap().contains("\"body\": \"hello\""));
        assert!(last["request"].as_str().unwrap().starts_with("POST http://127.0.0.1:"));

        let saved = dir.path().join("saved.json");
        let id = s.http.exchanges[0].id;
        s.execute("http.saveResponse", &json!({ "id": id, "path": saved })).unwrap();
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), r#"{"ok":true,"n":[1,2]}"#);
        assert_eq!(s.execute("http.saveResponse", &json!({ "id": id })).unwrap()["pick"], true);
        assert_eq!(s.http.save_pick, Some(id));
        s.execute("http.openResponseInEditor", &json!({ "id": id })).unwrap();
        assert!(s.ws.active().unwrap().1.path.as_ref().unwrap().to_string_lossy().ends_with(".200.json"));

        s.execute("http.rerun", &json!({ "id": id })).unwrap();
        wait(&mut s);
        assert_eq!(s.http.exchanges.len(), 4);
        server.join().unwrap();

        let h = s.execute("tools.httpClient.showHttpRequestsHistory", &Value::Null).unwrap();
        assert_eq!(h["entries"].as_array().unwrap().len(), 4);
        assert!(s.http.show_history);
        assert!(s.ws.active().unwrap().1.path.as_ref().unwrap().ends_with(history::LOG));
        s.execute("http.openHistoryEntry", &json!({ "index": 0 })).unwrap();
        assert_eq!(s.execute("http.responses", &Value::Null).unwrap().as_array().unwrap().len(), 4);
        s.execute("http.clearResponses", &Value::Null).unwrap();
        assert!(s.http.exchanges.is_empty());
        s.execute("http.clearHistory", &Value::Null).unwrap();
        assert!(history::load(&http::history_dir(&s).unwrap()).is_empty());
    }

    #[test]
    fn failures_are_reported_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let req = dir.path().join("x.http");
        std::fs::write(
            &req,
            format!(
                "GET http://127.0.0.1:{port}/\n###\nGET http://{{{{nope}}}}/\n###\nPOST http://h/\n\n< ./missing.json\n"
            ),
        )
        .unwrap();
        s.open_file(&req).unwrap();
        s.execute("http.runAll", &Value::Null).unwrap();
        wait(&mut s);
        assert!(s.http.exchanges.iter().all(|e| matches!(e.state, ExchangeState::Failed(_))));
        let ExchangeState::Failed(e) = &s.http.exchanges[1].state else { panic!("expected a failure") };
        assert!(e.contains("unresolved variables: nope"), "{e}");
        let ExchangeState::Failed(e) = &s.http.exchanges[2].state else { panic!("expected a failure") };
        assert!(e.contains("missing.json"), "{e}");
        // No project: nothing is recorded.
        assert!(http::history_dir(&s).is_none());
    }

    #[test]
    fn hostile_params_never_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session(dir.path());
        for (id, p) in [
            ("http.run", json!({ "line": -1 })),
            ("http.run", json!({ "path": 5 })),
            ("http.runAll", json!({ "path": "/nonexistent/x.http" })),
            ("http.copyAsCurl", json!({ "line": 99999999999u64 })),
            ("http.response", json!({ "id": "x" })),
            ("http.saveResponse", json!({ "id": 99 })),
            ("http.openHistoryEntry", json!({ "index": 3 })),
            ("http.selectResponse", json!({ "id": 1 })),
            ("tools.httpClient.addToHttpClient.variableInPublicFile", json!({ "name": "{{x}}" })),
        ] {
            let _ = s.execute(id, &p);
        }
    }
}
