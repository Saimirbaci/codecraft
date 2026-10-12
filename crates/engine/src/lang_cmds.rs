//! Language server management commands.

use serde_json::{Value, json};

use codecraft_lsp::ServerKind;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::params::opt_str;
use crate::{CommandError, LspStatus, Session};

fn parse_kind(name: &str) -> Option<ServerKind> {
    Some(match name {
        "rust" | "rust-analyzer" => ServerKind::RustAnalyzer,
        "c" | "cpp" | "clangd" => ServerKind::Clangd,
        "python" => ServerKind::Python,
        "zig" | "zls" => ServerKind::Zls,
        "typescript" | "javascript" | "ts" | "js" => ServerKind::TypeScript,
        _ => return None,
    })
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "lsp.restart",
            "Restart Language Server",
            None,
            "{server?: rust|clangd|python|zig|typescript} — restart one server (default: the active file's, else all)",
            Context::Global,
            has_project,
            restart,
        ),
        spec("lsp.status", "Language Servers", None, "→ [{server, status, memoryMb}]", Context::Global, always, status),
    ]
}

fn restart(s: &mut Session, p: &Value) -> CmdResult {
    let kind = match opt_str(p, "server") {
        Some(name) => {
            Some(parse_kind(name).ok_or_else(|| CommandError::BadParams(format!("unknown server `{name}`")))?)
        }
        None => s.ws.active().and_then(|(_, d)| crate::server_kind(d.language)),
    };
    s.restart_server(kind);
    s.status = match kind {
        Some(k) => format!("Restarting {}", k.label()),
        None => "Restarting language servers".into(),
    };
    ok()
}

fn status(s: &mut Session, _: &Value) -> CmdResult {
    let list: Vec<Value> = ServerKind::ALL
        .into_iter()
        .map(|k| {
            let status = match s.server_status(k) {
                LspStatus::Off => "off".to_string(),
                LspStatus::Starting => "starting".into(),
                LspStatus::Ready => "ready".into(),
                LspStatus::Unavailable(m) => format!("unavailable: {m}"),
                LspStatus::Stopped(m) => format!("stopped: {m}"),
            };
            json!({ "server": k.label(), "name": s.server_name(k), "status": status, "memoryMb": s.server_memory_mb(k) })
        })
        .collect();
    Ok(Value::Array(list))
}
