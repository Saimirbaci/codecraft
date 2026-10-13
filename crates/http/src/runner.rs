//! Running one request end to end: environment, variables, sending, history and `>>` output.
//! Blocking; the engine calls [`run`] on a worker thread.

use std::path::{Path, PathBuf};

use crate::client::{ExecOptions, Response, execute};
use crate::env::Environments;
use crate::format::BodyKind;
use crate::history::{self, HistoryEntry};
use crate::parse::{FileVariable, Request};
use crate::util;
use crate::vars::{ResolvedRequest, Variables, resolve};

/// What to run.
#[derive(Clone, Debug, Default)]
pub struct RunSpec {
    pub request: Request,
    pub file_variables: Vec<FileVariable>,
    /// The selected environment.
    pub env: Option<String>,
    /// The request file's directory (body files, `>>` output and environment files are relative to it).
    pub base_dir: PathBuf,
    /// The project root: environment files are searched up to it.
    pub root: Option<PathBuf>,
    /// Where to record the history (none: no history).
    pub history_dir: Option<PathBuf>,
    pub options: ExecOptions,
}

/// What happened.
#[derive(Clone, Debug, Default)]
pub struct RunResult {
    pub resolved: Option<ResolvedRequest>,
    pub response: Option<Response>,
    pub error: Option<String>,
    /// Problems with the environment files.
    pub env_errors: Vec<String>,
    /// The history entry recorded.
    pub history: Option<HistoryEntry>,
    /// Where `>> file` wrote the response.
    pub output_file: Option<PathBuf>,
}

/// The variables a request sees.
pub fn variables(spec: &RunSpec) -> (Variables, Vec<String>) {
    let envs = Environments::load(&spec.base_dir, spec.root.as_deref());
    (Variables::new(&spec.file_variables, envs.variables(spec.env.as_deref())), envs.errors)
}

fn output_path(base: &Path, path: &str, overwrite: bool) -> PathBuf {
    let p = base.join(path.trim());
    if overwrite || !p.exists() {
        return p;
    }
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..10_000).map(|i| p.with_file_name(format!("{stem}-{i}{ext}"))).find(|c| !c.exists()).unwrap_or(p)
}

/// Resolve, send and record one request.
pub fn run(spec: &RunSpec) -> RunResult {
    let (vars, env_errors) = variables(spec);
    let mut out = RunResult { env_errors, ..RunResult::default() };
    let resolved = match resolve(&spec.request, &vars, &spec.base_dir) {
        Ok(r) => r,
        Err(e) => {
            out.error = Some(e.to_string());
            return out;
        }
    };
    let time = util::unix_seconds();
    let mut opts = spec.options.clone();
    let temp = spec.history_dir.as_deref().map(|d| {
        let _ = std::fs::create_dir_all(d);
        history::temp_body_path(d)
    });
    opts.body_file = temp.clone();
    let result = execute(&resolved, &opts);
    let mut entry = HistoryEntry {
        time,
        name: resolved.name.clone(),
        method: resolved.method.clone(),
        url: resolved.url.clone(),
        request: history::request_text(&resolved.to_http_text()),
        ..HistoryEntry::default()
    };
    match result {
        Ok(mut resp) => {
            entry.status = Some(resp.status);
            entry.status_text = resp.status_text.clone();
            entry.elapsed_ms = resp.elapsed_ms;
            entry.size = resp.body_len;
            entry.headers = resp.headers.clone();
            if let (Some(dir), Some(temp)) = (spec.history_dir.as_deref(), temp.as_deref()) {
                let ext = BodyKind::detect(resp.content_type(), &resp.body).extension();
                match history::finish_body_file(dir, temp, time, resp.status, ext) {
                    Ok(name) => {
                        resp.body_file = Some(dir.join(&name));
                        entry.body_file = Some(name);
                    }
                    Err(e) => {
                        resp.body_file = None;
                        let _ = std::fs::remove_file(temp);
                        out.env_errors.push(e.to_string());
                    }
                }
            }
            if let Some(o) = &spec.request.output {
                let target = output_path(&spec.base_dir, &o.path, o.overwrite);
                let written = match &resp.body_file {
                    Some(src) => std::fs::copy(src, &target).map(|_| ()),
                    None => std::fs::write(&target, &resp.body),
                };
                match written {
                    Ok(()) => out.output_file = Some(target),
                    Err(e) => out.env_errors.push(format!("cannot write {}: {e}", target.display())),
                }
            }
            out.response = Some(resp);
        }
        Err(e) => {
            if let Some(t) = temp.as_deref() {
                let _ = std::fs::remove_file(t);
            }
            entry.error = Some(e.to_string());
            out.error = Some(e.to_string());
        }
    }
    if let Some(dir) = spec.history_dir.as_deref() {
        match history::record(dir, entry.clone()) {
            Ok(_) => out.history = Some(entry),
            Err(e) => out.env_errors.push(e.to_string()),
        }
    }
    out.resolved = Some(resolved);
    out
}
