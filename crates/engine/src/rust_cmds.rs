//! Commands of the Rust tools (M12 track RS): macro expansion, the Rust REPL, Cargo.toml
//! dependency updates and the advisory inspections.

use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_workspace::{NotificationLevel, ToolWindowId};

use crate::cargo_deps;
use crate::commands::{CmdResult, CommandSpec, Context, always, has_lsp_editor, has_project, ok, simple, spec};
use crate::inspections::{JobKind, ScopeKind, Tool};
use crate::params::{bool_or, opt_path, opt_str, req_str};
use crate::rust_repl;
use crate::{CommandError, Effect, Session};

fn rust_lsp_editor(s: &Session) -> Result<(), String> {
    has_lsp_editor(s)?;
    match s.ws.active().map(|(_, d)| d.language) {
        Some(Language::Rust) => Ok(()),
        _ => Err("macro expansion works in Rust files".into()),
    }
}

fn has_expansion(s: &Session) -> Result<(), String> {
    s.rust.macros.view.as_ref().map(|_| ()).ok_or_else(|| "no macro expansion is shown".into())
}

fn has_editor_text(s: &Session) -> Result<(), String> {
    s.ws.active().map(|_| ()).ok_or_else(|| "no editor is open".into())
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------ macro expansion
        spec(
            "rust.showRecursiveMacroExpansion",
            "Show Recursive Macro Expansion",
            None,
            "— expand the macro call at the caret (`name!(…)`, a derive or an attribute macro) recursively with rust-analyzer and show it in a popup (rustfmt-formatted); read it back with rust.macroExpansion.get",
            Context::Editor,
            rust_lsp_editor,
            |s, _| crate::macro_expand::request(s),
        ),
        simple("rust.macroExpansion.get", "Get Macro Expansion", always, |s, _| {
            Ok(json!({ "pending": s.rust.macros.pending.is_some(), "expansion": s.rust.macros.view }))
        }),
        simple("rust.macroExpansion.copy", "Copy Macro Expansion", has_expansion, |s, _| {
            let text = s.rust.macros.view.as_ref().map(|v| v.text.clone()).unwrap_or_default();
            s.clipboard = Some((text.clone(), false));
            s.effects.push(Effect::CopyToClipboard(text));
            s.status = "Macro expansion copied".into();
            ok()
        }),
        simple("rust.macroExpansion.close", "Close Macro Expansion", always, |s, _| {
            s.rust.macros.view = None;
            ok()
        }),
        // ------------------------------------------------ Rust REPL
        spec(
            "tools.rust.rustRepl",
            "Rust REPL",
            None,
            "— open the Rust REPL tool window and start evcxr (an install hint is shown when evcxr is missing)",
            Context::Global,
            always,
            open_repl,
        ),
        simple("view.toolWindows.rustRepl", "Rust REPL", always, |s, _| {
            s.ws.layout.toggle(ToolWindowId::RustRepl);
            ok()
        }),
        spec(
            "rust.repl.execute",
            "Execute in Rust REPL",
            None,
            "{code?: string, force?: bool} — run `code` (default: the input draft) in the REPL, starting it if needed. A draft with unbalanced brackets or an open string gets a new line instead (status `incomplete`) unless `force`; `code` always runs",
            Context::Global,
            always,
            execute,
        ),
        spec(
            "rust.repl.sendSelection",
            "Send Selection to Rust REPL",
            None,
            "— run the editor's selection (or the caret line) in the Rust REPL",
            Context::Editor,
            has_editor_text,
            send_selection,
        ),
        simple("rust.repl.restart", "Restart Rust REPL", always, |s, _| {
            s.rust.repl.stop();
            start(s)?;
            s.ws.layout.show(ToolWindowId::RustRepl);
            ok()
        }),
        simple("rust.repl.stop", "Stop Rust REPL", always, |s, _| {
            s.rust.repl.stop();
            ok()
        }),
        simple("rust.repl.clear", "Clear Rust REPL", always, |s, _| {
            s.rust.repl.clear();
            ok()
        }),
        simple("rust.repl.historyPrevious", "Previous REPL Input", always, |s, _| {
            s.rust.repl.browse_history(true);
            ok()
        }),
        simple("rust.repl.historyNext", "Next REPL Input", always, |s, _| {
            s.rust.repl.browse_history(false);
            ok()
        }),
        spec(
            "rust.repl.setDraft",
            "Set REPL Input",
            None,
            "{text: string} — replace the REPL's input draft",
            Context::Global,
            always,
            |s, p| {
                s.rust.repl.draft = req_str(p, "text")?.to_string();
                s.rust.repl.history_pos = None;
                ok()
            },
        ),
        simple("rust.repl.transcript", "Rust REPL Transcript", always, |s, _| {
            let r = &s.rust.repl;
            Ok(json!({
                "alive": r.is_alive(),
                "ready": r.ready,
                "running": r.running,
                "lastStatus": r.last_status,
                "text": r.text(),
                "history": r.history,
                "draft": r.draft,
            }))
        }),
        // ------------------------------------------------ Cargo.toml dependencies
        spec(
            "rust.cargo.dependencies",
            "List Cargo Dependencies",
            None,
            "{path?: string} — the dependencies of a Cargo.toml (default: the active one, else the project's) with their requirement, latest known version and status",
            Context::Global,
            always,
            |s, p| cargo_deps::list(s, opt_path(p, "path")),
        ),
        spec(
            "rust.cargo.checkDependencyUpdates",
            "Check Dependency Updates",
            None,
            "{path?: string} — look every dependency of a Cargo.toml up on crates.io again (answers arrive in the background; the newer-version inspection updates)",
            Context::Global,
            always,
            |s, p| cargo_deps::check_updates(s, opt_path(p, "path")),
        ),
        spec(
            "rust.cargo.updateDependency",
            "Update Dependency",
            None,
            "{name: string, version?: string, path?: string} — set a dependency's version requirement to `version` (default: the newest known), keeping its operator and precision",
            Context::Global,
            always,
            |s, p| cargo_deps::update_dependency(s, opt_path(p, "path"), req_str(p, "name")?, opt_str(p, "version")),
        ),
        spec(
            "rust.cargo.updateAllDependencies",
            "Update All Dependencies…",
            None,
            "{path?: string, preview?: bool} — update every dependency with a newer version; opens the refactoring preview (default) unless `preview` is false",
            Context::Global,
            always,
            |s, p| cargo_deps::update_all(s, opt_path(p, "path"), bool_or(p, "preview", true)),
        ),
        spec(
            "rust.cargo.setCratesIoLookups",
            "Crates.io Lookups",
            None,
            "{enabled?: bool, indexUrl?: string, apiUrl?: string} — turn crates.io lookups (completion, newer-version inspection) on or off (toggles without `enabled`); the URLs point them at another sparse index / API",
            Context::Global,
            always,
            |s, p| {
                let d = &mut s.rust.deps;
                let on = p.get("enabled").and_then(Value::as_bool).unwrap_or(!d.online);
                d.online = on;
                d.auto = on;
                if let Some(u) = opt_str(p, "indexUrl") {
                    d.index_url = u.to_string();
                }
                if let Some(u) = opt_str(p, "apiUrl") {
                    d.api_url = u.to_string();
                }
                s.status = format!("crates.io lookups {}", if on { "on" } else { "off" });
                Ok(json!({ "enabled": on }))
            },
        ),
        spec(
            "rust.cargo.openAdvisory",
            "Open Advisory",
            None,
            "{url: string} — open a security advisory in the browser",
            Context::Global,
            always,
            |s, p| {
                let url = req_str(p, "url")?;
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err(CommandError::BadParams("`url` must be an http(s) link".into()));
                }
                s.effects.push(Effect::OpenUrl(url.to_string()));
                ok()
            },
        ),
        spec(
            "rust.cargo.audit",
            "Check Dependencies for Advisories",
            None,
            "{tool?: \"cargo-audit\"|\"cargo-deny\"} — run cargo audit and/or cargo deny over the project into Inspection Results (advisories land on the dependency lines in Cargo.toml)",
            Context::Global,
            has_project,
            |s, p| {
                let tools = match opt_str(p, "tool") {
                    Some(t) => {
                        vec![Tool::parse(t).filter(|t| matches!(t, Tool::CargoAudit | Tool::CargoDeny)).ok_or_else(
                            || CommandError::BadParams("`tool` must be \"cargo-audit\" or \"cargo-deny\"".into()),
                        )?]
                    }
                    None => vec![Tool::CargoAudit, Tool::CargoDeny],
                };
                crate::inspections::start(s, JobKind::Inspect, ScopeKind::Project, None, Some(tools), false)?;
                ok()
            },
        ),
    ]
}

/// Start evcxr (an install hint in the transcript when it is missing).
fn start(s: &mut Session) -> Result<(), CommandError> {
    let cwd = s.project_root();
    match s.rust.repl.start(cwd.as_deref()) {
        Ok(()) => Ok(()),
        Err(e) => {
            if s.rust.repl.resolve_program().is_none() {
                s.notify(NotificationLevel::Info, "Rust REPL", rust_repl::INSTALL_HINT);
            }
            Err(CommandError::Failed(e))
        }
    }
}

fn open_repl(s: &mut Session, _: &Value) -> CmdResult {
    s.ws.layout.show(ToolWindowId::RustRepl);
    if s.rust.repl.is_alive() {
        return Ok(json!({ "installed": true, "started": false }));
    }
    if s.rust.repl.resolve_program().is_none() {
        let hint = format!("{}\n", rust_repl::INSTALL_HINT);
        if !s.rust.repl.text().ends_with(&hint) {
            s.rust.repl.push(rust_repl::ReplKind::System, &hint);
        }
        s.status = "evcxr is not installed".into();
        return Ok(json!({ "installed": false }));
    }
    start(s)?;
    Ok(json!({ "installed": true, "started": true }))
}

fn execute(s: &mut Session, p: &Value) -> CmdResult {
    let given = opt_str(p, "code").map(str::to_string);
    let from_draft = given.is_none();
    let code = given.unwrap_or_else(|| s.rust.repl.draft.clone());
    if code.trim().is_empty() {
        return Ok(json!({ "status": "empty" }));
    }
    let force = bool_or(p, "force", !from_draft);
    if !force && !rust_repl::is_complete(&code) {
        if from_draft {
            s.rust.repl.draft = rust_repl::continue_draft(&s.rust.repl.draft);
        }
        return Ok(json!({ "status": "incomplete" }));
    }
    if !s.rust.repl.is_alive() {
        start(s)?;
    }
    s.rust.repl.execute(&code).map_err(CommandError::Failed)?;
    s.rust.repl.remember(&code);
    if from_draft {
        s.rust.repl.draft.clear();
        s.rust.repl.history_pos = None;
    }
    s.ws.layout.show(ToolWindowId::RustRepl);
    Ok(json!({ "status": "running" }))
}

fn send_selection(s: &mut Session, _: &Value) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let sel = e.selections.primary();
    let code = if sel.start() == sel.end() {
        d.doc.line_text(d.doc.char_to_line(sel.head)).unwrap_or_default()
    } else {
        d.doc.slice(sel.start()..sel.end())
    };
    execute(s, &json!({ "code": code }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_without_tools_installed_never_fail_hard() {
        let mut s = Session::new();
        s.rust.repl.program = Some(std::path::PathBuf::from("/definitely/not/evcxr"));
        // A bad program path is "installed" but fails to start.
        assert!(s.execute("tools.rust.rustRepl", &Value::Null).is_err());
        s.rust.repl.program = None;
        let r = s.execute("rust.repl.execute", &json!({ "code": "fn f() {" , "force": false })).unwrap();
        assert_eq!(r["status"], "incomplete");
        assert!(s.execute("rust.macroExpansion.copy", &Value::Null).is_err());
        assert!(s.execute("rust.cargo.openAdvisory", &json!({ "url": "javascript:x" })).is_err());
        let r = s.execute("rust.cargo.setCratesIoLookups", &json!({ "enabled": false })).unwrap();
        assert_eq!(r["enabled"], false);
        assert!(s.execute("rust.cargo.dependencies", &Value::Null).is_err(), "no manifest");
        assert!(s.execute("rust.showRecursiveMacroExpansion", &Value::Null).is_err());
    }
}
