//! Code › Update Copyright…: put the copyright notice of the profile at the top of files, in each
//! language's comment style, replacing an existing notice (a leading comment that mentions
//! "copyright").
//!
//! The profile (`$CODECRAFT_HOME/copyright.json`) is a notice text with variables: `$year` /
//! `$today.year` (the current year), `$file.name` and `$project.name`. A shebang line (and a
//! Python encoding line) stays first. Each changed file is one undoable edit.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_text::{Change, ChangeSet, EditGroup, SelectionSet};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::inspections::ScopeKind;
use crate::params::{bool_or, opt_path, opt_str};
use crate::user_store::UserHome;
use crate::{CommandError, Session};

pub const FILE: &str = "copyright.json";
pub const DEFAULT_NOTICE: &str = "Copyright (c) $year $project.name contributors.\nLicensed under the terms in the LICENSE file.";
const MAX_FILES: usize = 20_000;
const MAX_NOTICE_CHARS: usize = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub notice: String,
}

impl Default for Profile {
    fn default() -> Self {
        Self { notice: DEFAULT_NOTICE.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyrightDialog {
    pub notice: String,
    /// `file`, `directory`, `project` or `uncommitted`.
    pub scope: String,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct CopyrightState {
    pub home: UserHome,
    pub dialog: Option<CopyrightDialog>,
    profile: Option<Profile>,
}

impl CopyrightState {
    pub fn set_home(&mut self, dir: PathBuf) {
        self.home.dir_override = Some(dir);
        self.profile = None;
    }

    pub fn profile(&mut self) -> Profile {
        if self.profile.is_none() {
            let loaded = self.home.file(FILE).and_then(|p| crate::user_store::load_json::<Profile>(&p).ok().flatten());
            self.profile = Some(loaded.unwrap_or_default());
        }
        self.profile.clone().unwrap_or_default()
    }

    fn save(&mut self, profile: Profile) -> Result<(), String> {
        let path = self.home.require(FILE)?;
        crate::user_store::save_json(&path, &profile, false)?;
        self.profile = Some(profile);
        Ok(())
    }
}

/// The notice with its variables filled in.
pub fn render(notice: &str, file_name: &str, project: &str) -> String {
    let year = crate::user_store::current_year().to_string();
    notice
        .replace("$today.year", &year)
        .replace("$year", &year)
        .replace("$file.name", file_name)
        .replace("$project.name", project)
        .trim_end()
        .to_string()
}

enum Style {
    Block(&'static str, &'static str, &'static str),
    Line(&'static str),
}

fn style(l: Language) -> Option<Style> {
    match l {
        Language::Rust | Language::C | Language::Cpp | Language::TypeScript | Language::JavaScript => {
            Some(Style::Block("/*", " * ", " */"))
        }
        Language::Sql => Some(Style::Line("--")),
        Language::Zig => Some(Style::Line("//")),
        Language::Python | Language::Shell | Language::Toml | Language::Yaml | Language::Dockerfile => {
            Some(Style::Line("#"))
        }
        _ => None,
    }
}

/// The header comment for a language (ending with a newline); `None` for languages without a
/// comment syntax CodeCraft writes notices in.
pub fn header(l: Language, text: &str) -> Option<String> {
    let mut out = String::new();
    match style(l)? {
        Style::Block(open, mid, close) => {
            out.push_str(open);
            out.push('\n');
            for line in text.lines() {
                out.push_str(if line.is_empty() { mid.trim_end() } else { mid });
                out.push_str(line);
                out.push('\n');
            }
            out.push_str(close);
            out.push('\n');
        }
        Style::Line(prefix) => {
            for line in text.lines() {
                out.push_str(prefix);
                if !line.is_empty() {
                    out.push(' ');
                    out.push_str(line);
                }
                out.push('\n');
            }
        }
    }
    Some(out)
}

/// Byte offset after lines that must stay first (shebang, Python encoding declaration).
fn kept_prefix(text: &str, l: Language) -> usize {
    let mut at = 0usize;
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let keep = (i == 0 && line.starts_with("#!"))
            || (l == Language::Python && i < 2 && line.starts_with('#') && line.contains("coding"));
        if !keep {
            break;
        }
        at += line.len();
    }
    at
}

/// Byte range of an existing notice at `from` (after blank lines): a leading comment that
/// mentions "copyright" (doc comments are never notices).
fn existing_notice(text: &str, from: usize, l: Language) -> Option<(usize, usize)> {
    let rest = text.get(from..)?;
    let skip = rest.len() - rest.trim_start_matches(['\n', '\r', ' ', '\t']).len();
    let start = from + skip;
    let body = text.get(start..)?;
    let end = match style(l)? {
        Style::Block(open, _, _) if body.starts_with(open) && !body.starts_with("/**") && !body.starts_with("/*!") => {
            let close = body.find("*/")? + 2;
            let after = body.get(close..).map_or(0, |r| if r.starts_with('\n') { 1 } else if r.starts_with("\r\n") { 2 } else { 0 });
            start + close + after
        }
        _ => {
            let prefix = l.line_comment()?;
            let mut len = 0usize;
            for line in body.split_inclusive('\n') {
                let t = line.trim_start();
                if !t.starts_with(prefix) || t.starts_with("///") || t.starts_with("//!") || t.starts_with("#!") {
                    break;
                }
                len += line.len();
            }
            if len == 0 {
                return None;
            }
            start + len
        }
    };
    text.get(start..end).filter(|c| c.to_lowercase().contains("copyright")).map(|_| (start, end))
}

/// The file's text with the notice in place, or `None` when nothing changes (or the language
/// has no supported comment style).
pub fn update(text: &str, l: Language, notice: &str) -> Option<String> {
    let header = header(l, notice)?;
    let keep = kept_prefix(text, l);
    let (start, end, sep) = match existing_notice(text, keep, l) {
        Some((s, e)) => (s, e, ""),
        None => {
            let rest = text.get(keep..).unwrap_or("");
            (keep, keep, if rest.is_empty() { "" } else { "\n" })
        }
    };
    let mut out = String::with_capacity(text.len() + header.len() + 1);
    out.push_str(text.get(..start)?);
    out.push_str(&header);
    out.push_str(sep);
    out.push_str(text.get(end..)?);
    (out != text).then_some(out)
}

/// Replace a file's text as one undoable edit (only the changed middle), loading the file as a
/// document when it is not open. Saved by the caller.
pub(crate) fn replace_file_text(s: &mut Session, path: &Path, new_text: &str) -> Result<bool, String> {
    let doc_id = match s.ws.doc_by_path(path) {
        Some(d) => d,
        None => {
            let text = codecraft_fs::read_text_file(path).map_err(|e| e.to_string())?;
            let id = s.ws.add_document(Some(path.to_path_buf()), &text);
            if let Some(od) = s.ws.doc_mut(id) {
                od.language = Language::from_path(path);
            }
            s.lsp_did_open(id);
            id
        }
    };
    let od = s.ws.doc(doc_id).ok_or("no document")?;
    if od.read_only {
        return Err(format!("{} is read-only", path.display()));
    }
    let old: Vec<char> = od.doc.text().chars().collect();
    let new: Vec<char> = new_text.chars().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let max_suffix = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(max_suffix).take_while(|(a, b)| a == b).count();
    if prefix == old.len() && prefix == new.len() {
        return Ok(false);
    }
    let insert: String = new.get(prefix..new.len() - suffix).map(|c| c.iter().collect()).unwrap_or_default();
    let set = ChangeSet::new(vec![Change::new(prefix, old.len() - suffix, insert)]).map_err(|e| e.to_string())?;
    let editor = s.ws.editor_for_doc(doc_id);
    let before = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_else(|| SelectionSet::caret(0));
    let after = before.map(&set);
    let edit = codecraft_text::Edit { changes: set, selections: after.clone() };
    let od = s.ws.doc_mut(doc_id).ok_or("no document")?;
    od.doc.apply(&edit, &before, EditGroup::Standalone).map_err(|e| e.to_string())?;
    if let Some(e) = editor {
        s.ws.set_selections(e, after);
    }
    Ok(true)
}

fn files_in_scope(s: &Session, scope: &str, path: Option<PathBuf>) -> Result<Vec<PathBuf>, CommandError> {
    if matches!(scope, "openFiles" | "open") {
        return Ok(s.ws.docs().filter_map(|d| d.path.clone()).collect());
    }
    let kind = ScopeKind::parse(scope).ok_or_else(|| CommandError::BadParams(format!("unknown scope `{scope}`")))?;
    Ok(crate::inspections::resolve_scope(s, kind, path)?.files)
}

fn update_copyright(s: &mut Session, p: &Value) -> CmdResult {
    let Some(scope) = opt_str(p, "scope").map(str::to_string) else {
        let notice = s.copyright.profile().notice;
        let scope = if s.ws.active().and_then(|(_, d)| d.path.as_ref()).is_some() { "file" } else { "project" };
        s.copyright.dialog = Some(CopyrightDialog { notice, scope: scope.into(), error: None });
        return Ok(json!({ "dialog": true }));
    };
    let notice = match opt_str(p, "notice") {
        Some(n) => n.to_string(),
        None => s.copyright.profile().notice,
    };
    if notice.trim().is_empty() || notice.chars().count() > MAX_NOTICE_CHARS {
        return Err(CommandError::BadParams("the notice is empty or too long".into()));
    }
    if bool_or(p, "saveProfile", false) {
        s.copyright.save(Profile { notice: notice.clone() }).map_err(CommandError::Failed)?;
    }
    let files = files_in_scope(s, &scope, opt_path(p, "path"))?;
    let project = s.ws.project.as_ref().map(|p| p.name.clone()).unwrap_or_default();
    let mut changed = Vec::new();
    let mut errors = Vec::new();
    for path in files.into_iter().take(MAX_FILES) {
        let language = s.ws.doc_by_path(&path).and_then(|d| s.ws.doc(d)).map(|d| d.language).unwrap_or_else(|| Language::from_path(&path));
        if style(language).is_none() {
            continue;
        }
        let Some(text) = s.current_text(&path) else { continue };
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Some(new_text) = update(&text, language, &render(&notice, &name, &project)) else { continue };
        match replace_file_text(s, &path, &new_text) {
            Ok(true) => changed.push(path),
            Ok(false) => {}
            Err(e) => errors.push(e),
        }
    }
    let _ = s.save_all();
    s.copyright.dialog = None;
    s.status = format!("Update Copyright: {} file(s) updated", changed.len());
    if !errors.is_empty() {
        s.notify(codecraft_workspace::NotificationLevel::Warning, "Update Copyright", errors.join("\n"));
    }
    Ok(json!({ "updated": changed, "errors": errors }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.updateCopyright",
            "Update Copyright…",
            None,
            "{scope?: \"file\"|\"directory\"|\"project\"|\"uncommitted\"|\"openFiles\", path?: string, notice?: string ($year, $file.name, $project.name), saveProfile?: bool} — put the copyright notice at the top of the scope's files in each language's comment style, replacing an existing one; without `scope` opens the dialog",
            Context::Global,
            |s| if s.ws.project.is_some() || s.ws.active().is_some() { Ok(()) } else { Err("open a project or a file first".into()) },
            update_copyright,
        ),
        spec("copyright.dialog.ok", "Update", None, "Run the dialog (and keep its notice as the profile)", Context::Global, |s| s.copyright.dialog.as_ref().map(|_| ()).ok_or_else(|| "the dialog is not open".into()), |s, _| {
            let d = s.copyright.dialog.clone().ok_or_else(|| CommandError::Failed("no dialog".into()))?;
            let r = update_copyright(s, &json!({ "scope": d.scope, "notice": d.notice, "saveProfile": true }));
            if let Err(e) = &r
                && let Some(d) = &mut s.copyright.dialog
            {
                d.error = Some(e.to_string());
            }
            r
        }),
        spec("copyright.dialog.cancel", "Cancel", None, "", Context::Global, always, |s, _| {
            s.copyright.dialog = None;
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn headers_per_language() {
        assert_eq!(header(Language::Rust, "A\n\nB").unwrap(), "/*\n * A\n *\n * B\n */\n");
        assert_eq!(header(Language::Python, "A\n\nB").unwrap(), "# A\n#\n# B\n");
        assert_eq!(header(Language::Zig, "A").unwrap(), "// A\n");
        assert!(header(Language::Json, "A").is_none());
        assert!(header(Language::Markdown, "A").is_none());
    }

    #[test]
    fn inserts_replaces_and_keeps_shebangs() {
        let n = "Copyright (c) 2026 Me";
        assert_eq!(update("fn main() {}\n", Language::Rust, n).unwrap(), "/*\n * Copyright (c) 2026 Me\n */\n\nfn main() {}\n");
        // Replaces an old notice; idempotent.
        let old = "/*\n * Copyright (c) 2020 Old\n */\n\nfn main() {}\n";
        let new = update(old, Language::Rust, n).unwrap();
        assert_eq!(new, "/*\n * Copyright (c) 2026 Me\n */\n\nfn main() {}\n");
        assert!(update(&new, Language::Rust, n).is_none());
        // A doc comment is not a notice.
        assert_eq!(update("//! Crate docs\n", Language::Rust, n).unwrap(), "/*\n * Copyright (c) 2026 Me\n */\n\n//! Crate docs\n");
        // Shebang and encoding lines stay first; line comments are replaced.
        let py = "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n# copyright 1999 X\nprint(1)\n";
        assert_eq!(update(py, Language::Python, n).unwrap(), "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n# Copyright (c) 2026 Me\nprint(1)\n");
        let sh = "#!/bin/sh\necho hi\n";
        assert_eq!(update(sh, Language::Shell, n).unwrap(), "#!/bin/sh\n# Copyright (c) 2026 Me\n\necho hi\n");
        assert_eq!(update("", Language::Rust, n).unwrap(), "/*\n * Copyright (c) 2026 Me\n */\n");
        assert!(update("{}", Language::Json, n).is_none());
        // Hostile: an unterminated block comment and odd text never panic.
        for t in ["/* copyright", "/*", "#", "\u{0}😀", "\r\n\r\n/* Copyright */"] {
            let _ = update(t, Language::Rust, n);
            let _ = update(t, Language::Python, n);
        }
        let r = render("© $year $file.name $project.name", "a.rs", "p");
        assert!(r.starts_with("© 20") && r.ends_with(" a.rs p"));
    }

    #[test]
    fn updates_a_project_as_one_edit_per_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("b.py"), "print(1)\n").unwrap();
        std::fs::write(root.join("c.json"), "{}\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&root.join(".home"));
        s.execute("file.open", &json!({ "path": root })).unwrap();
        let start = std::time::Instant::now();
        while s.ws.project.as_ref().is_none_or(|p| p.tree.is_empty()) && start.elapsed().as_secs() < 30 {
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        s.execute("file.open", &json!({ "path": root.join("a.rs") })).unwrap();
        assert_eq!(s.execute("code.updateCopyright", &json!({})).unwrap()["dialog"], true);
        s.copyright.dialog.as_mut().unwrap().notice = "Copyright $year ACME ($file.name)".into();
        s.copyright.dialog.as_mut().unwrap().scope = "openFiles".into();
        s.execute("copyright.dialog.ok", &json!({})).unwrap();
        let a = std::fs::read_to_string(root.join("a.rs")).unwrap();
        assert!(a.starts_with("/*\n * Copyright 20") && a.contains("ACME (a.rs)"), "{a}");
        s.copyright.set_home(root.join(".home"));
        assert!(s.copyright.profile().notice.contains("ACME"));
        let d = s.ws.doc_by_path(&root.join("a.rs")).unwrap();
        s.ws.doc_mut(d).unwrap().doc.undo();
        assert_eq!(s.ws.doc(d).unwrap().doc.text(), "fn a() {}\n");
        let v = s.execute("code.updateCopyright", &json!({ "scope": "directory", "path": root, "notice": "Copyright X" })).unwrap();
        assert_eq!(v["updated"].as_array().unwrap().len(), 2, "{v}");
        assert!(std::fs::read_to_string(root.join("b.py")).unwrap().starts_with("# Copyright X\n\nprint(1)"));
        assert_eq!(std::fs::read_to_string(root.join("c.json")).unwrap(), "{}\n");
    }
}
