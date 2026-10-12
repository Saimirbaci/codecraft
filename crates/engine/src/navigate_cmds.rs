//! Navigate menu commands and the navigation popups' accept actions.

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_text::{Position, SelectionSet};
use codecraft_workspace::{Location, Severity};

use crate::commands::{
    CmdResult, CommandSpec, Context, always, editor, has_editor, has_lsp_editor, has_project, ok, simple, spec,
};
use crate::lsp::GotoKind;
use crate::params::{opt_usize, req_path, req_str};
use crate::popup::{StructureItem, SymbolItem};
use crate::{CommandError, Popup, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("navigate.back", "Back", always, back),
        simple("navigate.forward", "Forward", always, forward),
        simple("navigate.file", "File…", has_project, |s, _| {
            s.popup = Some(Popup::GotoFile { query: String::new(), selected: 0 });
            ok()
        }),
        simple("navigate.symbol", "Symbol…", has_project, |s, _| goto_symbol(s, false)),
        simple("navigate.type", "Type...", has_project, |s, _| goto_symbol(s, true)),
        spec(
            "navigate.searchEverywhere",
            "Search Everywhere",
            Some("Shift Shift"),
            "",
            Context::Global,
            always,
            |s, _| {
                s.popup = Some(Popup::SearchEverywhere {
                    query: String::new(),
                    selected: 0,
                    symbols: Vec::new(),
                    pending: false,
                });
                ok()
            },
        ),
        simple("help.findAction", "Find Action…", always, |s, _| {
            s.popup = Some(Popup::FindAction { query: String::new(), selected: 0 });
            ok()
        }),
        simple("navigate.text", "Text…", has_project, |s, _| {
            s.popup = Some(Popup::FindInFiles { query: Default::default(), mask: String::new(), replace: None });
            ok()
        }),
        spec(
            "navigate.lineColumn",
            "Line:Column…",
            None,
            "{line?: int, column?: int} — 1-based, like the dialog; without params the UI asks",
            Context::Global,
            has_editor,
            goto_line,
        ),
        editor("navigate.nextHighlightedError", "Next Highlighted Error", |s, _| next_error(s, true)),
        editor("navigate.previousHighlightedError", "Previous Highlighted Error", |s, _| next_error(s, false)),
        simple("navigate.lastEditLocation", "Last Edit Location", always, |s, _| {
            let Some(loc) = s.last_edit.clone() else { return ok() };
            s.navigate_to(&loc.path, loc.offset)?;
            ok()
        }),
        editor("navigate.navigateInFile.moveCaretToMatchingBrace", "Move Caret to Matching Brace", matching_brace),
        editor("navigate.navigateInFile.nextMethod", "Next Method", |s, _| step_method(s, true)),
        editor("navigate.navigateInFile.previousMethod", "Previous Method", |s, _| step_method(s, false)),
        spec("navigate.selectIn", "Select In…", None, "", Context::Global, has_editor, select_in),
        spec(
            "navigate.declarationOrUsages",
            "Declaration or Usages",
            None,
            "",
            Context::Editor,
            has_lsp_editor,
            |s, _| {
                s.lsp_goto(GotoKind::DeclarationOrUsages)?;
                ok()
            },
        ),
        spec("navigate.implementations", "Implementation(s)", None, "", Context::Editor, has_lsp_editor, |s, _| {
            s.lsp_goto(GotoKind::Implementation)?;
            ok()
        }),
        spec("navigate.typeDeclaration", "Type Declaration", None, "", Context::Editor, has_lsp_editor, |s, _| {
            s.lsp_goto(GotoKind::TypeDefinition)?;
            ok()
        }),
        spec(
            "edit.findUsages.findUsages",
            "Find Usages",
            Some("Alt+F7"),
            "",
            Context::Editor,
            has_lsp_editor,
            |s, _| {
                let word =
                    s.ws.active()
                        .map(|(e, d)| {
                            d.doc.slice(codecraft_text::movement::word_range_at(&d.doc, e.selections.primary().head))
                        })
                        .unwrap_or_default();
                s.lsp_references(format!("Usages of '{word}'"))?;
                ok()
            },
        ),
        spec("navigate.fileStructure", "File Structure", None, "", Context::Global, has_editor, file_structure),
        spec("navigate.filePath", "File Path", None, "", Context::Global, has_editor, |s, _| {
            let Some(path) = s.ws.active().and_then(|(_, d)| d.path.clone()) else { return ok() };
            let text = path
                .ancestors()
                .map(|p| p.display().to_string())
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            s.popup = Some(Popup::Message { title: "File Path".into(), text });
            ok()
        }),
        simple("view.recentFiles", "Recent Files", always, |s, _| {
            s.popup =
                Some(Popup::RecentFiles { query: String::new(), selected: if s.ws.tabs.len() > 1 { 1 } else { 0 } });
            ok()
        }),
        simple("view.recentLocations", "Recent Locations", always, |s, _| {
            s.popup = Some(Popup::RecentLocations { selected: 0 });
            ok()
        }),
        // Popup accept actions (also usable directly by agents).
        spec(
            "navigate.openLocation",
            "Open Location",
            None,
            "{path: string, line?: int, column?: int} — zero-based line/column",
            Context::Global,
            always,
            open_location,
        ),
        spec(
            "navigate.gotoFileQuery",
            "Go to File Results",
            None,
            "{query: string, limit?: int}",
            Context::Global,
            has_project,
            |s, p| {
                let q = req_str(p, "query")?;
                let limit = opt_usize(p, "limit")?.unwrap_or(50);
                let items: Vec<Value> = s
                    .goto_file_results(q, limit)
                    .into_iter()
                    .map(|(path, rel, _)| json!({ "path": path, "relative": rel }))
                    .collect();
                Ok(Value::Array(items))
            },
        ),
        spec(
            "navigate.symbolQuery",
            "Go to Symbol Query",
            None,
            "{query: string}",
            Context::Global,
            has_project,
            |s, p| {
                let q = req_str(p, "query")?.to_string();
                match &mut s.popup {
                    Some(Popup::GotoSymbol { query, pending, .. })
                    | Some(Popup::SearchEverywhere { query, pending, .. }) => {
                        *query = q.clone();
                        *pending = true;
                    }
                    _ => {}
                }
                if s.lsp_ready().is_ok() && !q.is_empty() {
                    s.lsp_workspace_symbols(&q)?;
                } else {
                    let items = s.lexical_symbols(&q);
                    if let Some(Popup::GotoSymbol { items: target, pending, .. }) = &mut s.popup {
                        *target = items;
                        *pending = false;
                    }
                }
                ok()
            },
        ),
    ]
}

fn back(s: &mut Session, _: &Value) -> CmdResult {
    let cur = s.ws.current_location();
    if let Some(loc) = s.ws.back(cur) {
        s.goto_location(&loc.path, loc.offset)?;
    }
    ok()
}

fn forward(s: &mut Session, _: &Value) -> CmdResult {
    if let Some(loc) = s.ws.forward() {
        s.goto_location(&loc.path, loc.offset)?;
    }
    ok()
}

fn goto_symbol(s: &mut Session, types_only: bool) -> CmdResult {
    let query =
        s.ws.active()
            .map(|(e, d)| {
                let sel = e.selections.primary();
                if sel.is_empty() { String::new() } else { d.doc.slice(sel.range()) }
            })
            .unwrap_or_default();
    s.popup = Some(Popup::GotoSymbol { query, selected: 0, types_only, items: Vec::new(), pending: false });
    ok()
}

fn goto_line(s: &mut Session, p: &Value) -> CmdResult {
    let Some(line) = opt_usize(p, "line")? else {
        let input =
            s.ws.active()
                .map(|(e, d)| {
                    let pos = d.doc.offset_to_position(e.selections.primary().head);
                    format!("{}:{}", pos.line + 1, pos.column + 1)
                })
                .unwrap_or_default();
        s.popup = Some(Popup::GotoLine { input });
        return ok();
    };
    let column = opt_usize(p, "column")?.unwrap_or(1);
    let cur = s.ws.current_location();
    s.select_active(|d, _| {
        SelectionSet::caret(d.position_to_offset(Position::new(line.saturating_sub(1), column.saturating_sub(1))))
    });
    if let Some(cur) = cur {
        s.ws.push_location(cur);
    }
    s.popup = None;
    ok()
}

/// F2 / Shift+F2: next/previous error (warnings when there are no errors), wrapping around.
fn next_error(s: &mut Session, forward: bool) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let Some(path) = d.path.clone() else { return ok() };
    let caret = e.selections.primary().head;
    let problems = s.ws.problems_for(&path);
    let has_errors = problems.iter().any(|p| p.severity == Severity::Error);
    let mut offsets: Vec<(usize, String)> = problems
        .iter()
        .filter(|p| if has_errors { p.severity == Severity::Error } else { p.severity <= Severity::Warning })
        .map(|p| (d.doc.position_to_offset(p.start), p.message.clone()))
        .collect();
    offsets.sort();
    offsets.dedup_by_key(|(o, _)| *o);
    let target = if forward {
        offsets.iter().find(|(o, _)| *o > caret).or(offsets.first())
    } else {
        offsets.iter().rev().find(|(o, _)| *o < caret).or(offsets.last())
    };
    match target.cloned() {
        Some((o, msg)) => {
            s.select_active(|_, _| SelectionSet::caret(o));
            s.status = msg;
        }
        None => s.status = "No errors or warnings in this file".into(),
    }
    ok()
}

fn matching_brace(s: &mut Session, _: &Value) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let caret = e.selections.primary().head;
    let target = codecraft_syntax::matching_bracket(d.language, &d.doc.text(), caret);
    if let Some(t) = target {
        s.select_active(|_, _| SelectionSet::caret(t));
    }
    ok()
}

fn step_method(s: &mut Session, forward: bool) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let caret_line = d.doc.char_to_line(e.selections.primary().head);
    let items = codecraft_syntax::outline(d.language, &d.doc.text());
    let lines: Vec<(usize, usize)> = items.iter().map(|i| (i.line, i.column)).collect();
    let target = if forward {
        lines.iter().find(|(l, _)| *l > caret_line)
    } else {
        lines.iter().rev().find(|(l, _)| *l < caret_line)
    };
    if let Some((l, c)) = target.copied() {
        s.select_active(|d, _| SelectionSet::caret(d.position_to_offset(Position::new(l, c))));
    }
    ok()
}

fn select_in(s: &mut Session, _: &Value) -> CmdResult {
    let Some(path) = s.ws.active().and_then(|(_, d)| d.path.clone()) else { return ok() };
    if let Some(p) = &mut s.ws.project {
        p.reveal(&path);
    }
    s.ws.layout.show(codecraft_workspace::ToolWindowId::Project);
    ok()
}

fn file_structure(s: &mut Session, _: &Value) -> CmdResult {
    let Some((_, d)) = s.ws.active() else { return ok() };
    let items: Vec<StructureItem> = codecraft_syntax::outline(d.language, &d.doc.text())
        .into_iter()
        .map(|i| StructureItem {
            name: i.name,
            kind: format!("{:?}", i.kind).to_lowercase(),
            detail: None,
            depth: i.depth,
            line: i.line,
            column: i.column,
        })
        .collect();
    let rust = d.language == codecraft_syntax::Language::Rust;
    s.popup = Some(Popup::FileStructure { query: String::new(), selected: 0, items, pending: false });
    if rust && s.lsp_ready().is_ok() {
        if let Some(Popup::FileStructure { pending, .. }) = &mut s.popup {
            *pending = true;
        }
        s.lsp_document_symbols()?;
    }
    ok()
}

fn open_location(s: &mut Session, p: &Value) -> CmdResult {
    let path = req_path(p, "path")?;
    let line = opt_usize(p, "line")?.unwrap_or(0);
    let column = opt_usize(p, "column")?.unwrap_or(0);
    s.popup = None;
    s.navigate_to_position(&path, line, column)?;
    ok()
}

impl Session {
    /// Go to File results: `(absolute path, relative path, match indices)` best first.
    pub fn goto_file_results(&self, query: &str, limit: usize) -> Vec<(PathBuf, String, Vec<usize>)> {
        let Some(project) = &self.ws.project else { return Vec::new() };
        let files: Vec<(PathBuf, String)> =
            project.tree.files().map(|n| (n.path.clone(), project.relative(&n.path).into_owned())).collect();
        if query.trim().is_empty() {
            // Recent files first, like RustRover's empty Go to File list.
            let mut out: Vec<(PathBuf, String, Vec<usize>)> = self
                .ws
                .recent_files
                .iter()
                .filter(|p| p.starts_with(&project.root))
                .take(limit)
                .map(|p| (p.clone(), project.relative(p).into_owned(), Vec::new()))
                .collect();
            if out.len() < limit {
                out.extend(
                    files
                        .iter()
                        .filter(|(p, _)| !self.ws.recent_files.contains(p))
                        .take(limit - out.len())
                        .map(|(p, r)| (p.clone(), r.clone(), Vec::new())),
                );
            }
            return out;
        }
        let q = query.trim();
        // Allow `name:line` like RustRover.
        let q = q.rsplit_once(':').filter(|(_, l)| l.chars().all(|c| c.is_ascii_digit())).map(|(n, _)| n).unwrap_or(q);
        codecraft_fs::rank(q, files.iter().map(|(_, r)| r.as_str()), limit)
            .into_iter()
            .filter_map(|(i, m)| files.get(i).map(|(p, r)| (p.clone(), r.clone(), m.indices)))
            .collect()
    }

    /// Lexical Go to Symbol over open documents (fallback without a language server).
    pub(crate) fn lexical_symbols(&self, query: &str) -> Vec<SymbolItem> {
        let mut out = Vec::new();
        for d in self.ws.docs() {
            let Some(path) = &d.path else { continue };
            for item in codecraft_syntax::outline(d.language, &d.doc.text()) {
                if codecraft_fs::fuzzy_match(query, &item.name).is_some() {
                    out.push(SymbolItem {
                        name: item.name,
                        kind: format!("{:?}", item.kind).to_lowercase(),
                        container: None,
                        path: path.clone(),
                        line: item.line,
                        column: item.column,
                    });
                }
            }
        }
        out
    }

    /// Record the current location and jump (used by popups' accept).
    pub fn jump_to(&mut self, loc: &Location) -> Result<(), CommandError> {
        self.navigate_to(&loc.path, loc.offset)
    }
}
