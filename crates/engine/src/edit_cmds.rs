//! Edit menu commands: undo/redo, clipboard, find/replace, multi-caret, line operations.

use serde_json::{Value, json};

use codecraft_text::search::{self, SearchQuery};
use codecraft_text::{EditGroup, Selection, SelectionSet, movement, ops};

use crate::commands::{CmdResult, CommandSpec, Context, editor, has_editor, has_project, ok, simple, spec};
use crate::params::{bool_or, opt_str};
use crate::popup::FindBar;
use crate::{CommandError, Effect, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        editor("edit.undo", "Undo", undo),
        editor("edit.redo", "Redo", redo),
        editor("edit.cut", "Cut", cut),
        editor("edit.copy", "Copy", copy),
        editor("edit.copyAsPlainText", "Copy as Plain Text", copy),
        spec(
            "edit.copyPathReference",
            "Copy Path/Reference…",
            Some("Shift+Alt+Cmd+C"),
            "{kind?: \"absolute\"|\"relative\"|\"name\"} (default absolute)",
            Context::Global,
            has_editor,
            copy_path,
        ),
        spec(
            "edit.paste",
            "Paste",
            Some("Cmd+V"),
            "{text?: string, wholeLine?: bool} — pastes the given text, else the internal clipboard",
            Context::Editor,
            has_editor,
            paste,
        ),
        editor("edit.delete", "Delete", |s, _| {
            s.edit_active(EditGroup::Standalone, |d, sel, _| ops::delete_forward(d, sel))?;
            ok()
        }),
        editor("edit.selectAll", "Select All", |s, _| {
            s.select_active(|d, _| movement::select_all(d));
            ok()
        }),
        // Find / replace bar
        editor("edit.find.find", "Find…", |s, _| open_find_bar(s, false)),
        editor("edit.find.replace", "Replace…", |s, _| open_find_bar(s, true)),
        editor("edit.searchInSelection", "Search In Selection", |s, _| open_find_bar(s, false)),
        editor("edit.find.searchInSelection", "Search In Selection", |s, _| open_find_bar(s, false)),
        spec(
            "edit.find.setQuery",
            "Set Find Query",
            None,
            "{pattern: string, caseSensitive?: bool, wholeWord?: bool, regex?: bool, replace?: string}",
            Context::Editor,
            has_editor,
            set_find_query,
        ),
        editor("edit.find.findNextOccurrence", "Find Next Occurrence", |s, _| find_step(s, true)),
        editor("edit.find.findPreviousOccurrence", "Find Previous Occurrence", |s, _| find_step(s, false)),
        editor("edit.find.replaceCurrent", "Replace", replace_current),
        editor("edit.find.replaceAll", "Replace All", replace_all),
        editor("edit.find.nextOccurrenceOfTheWordAtCaret", "Next Occurrence of the Word at Caret", |s, _| {
            word_step(s, true)
        }),
        editor("edit.find.previousOccurrenceOfTheWordAtCaret", "Previous Occurrence of the Word at Caret", |s, _| {
            word_step(s, false)
        }),
        editor("edit.find.selectAllOccurrences", "Select All Occurrences", |s, _| {
            s.select_active(movement::select_all_occurrences);
            ok()
        }),
        editor("edit.find.addSelectionForNextOccurrence", "Add Selection for Next Occurrence", |s, _| {
            s.select_active(movement::add_next_occurrence);
            ok()
        }),
        editor("edit.find.unselectOccurrence", "Unselect Occurrence", |s, _| {
            s.select_active(movement::unselect_occurrence);
            ok()
        }),
        editor("edit.addCaretsToEndsOfSelectedLines", "Add Carets to Ends of Selected Lines", |s, _| {
            s.select_active(movement::add_carets_to_line_ends);
            ok()
        }),
        editor("edit.extendSelection", "Extend Selection", extend_selection),
        editor("edit.shrinkSelection", "Shrink Selection", shrink_selection),
        editor("edit.toggleCase", "Toggle Case", |s, _| run_op(s, |d, sel, _| ops::toggle_case(d, sel))),
        editor("edit.joinLines", "Join Lines", |s, _| run_op(s, |d, sel, _| ops::join_lines(d, sel))),
        editor("edit.duplicateSelection", "Duplicate Line or Selection", |s, _| {
            run_op(s, |d, sel, _| ops::duplicate(d, sel))
        }),
        editor("edit.sortLines", "Sort Lines", |s, _| run_op(s, |d, sel, _| ops::sort_lines(d, sel))),
        editor("edit.reverseLines", "Reverse Lines", |s, _| run_op(s, |d, sel, _| ops::reverse_lines(d, sel))),
        editor("edit.transpose", "Transpose", |s, _| run_op(s, |d, sel, _| ops::transpose(d, sel))),
        editor("edit.indentSelection", "Indent Selection", |s, _| run_op(s, ops::indent)),
        editor("edit.unindentLineOrSelection", "Unindent Line or Selection", |s, _| run_op(s, ops::unindent)),
        editor("edit.convertIndents.toSpaces", "To Spaces", |s, _| convert_indents(s, true)),
        editor("edit.convertIndents.toTabs", "To Tabs", |s, _| convert_indents(s, false)),
        simple("edit.find.findInFiles", "Find in Files…", has_project, |s, _| find_in_files_popup(s, false)),
        simple("edit.find.replaceInFiles", "Replace in Files…", has_project, |s, _| find_in_files_popup(s, true)),
        spec(
            "edit.find.findInFilesRun",
            "Find in Files (run)",
            None,
            "{pattern: string, caseSensitive?: bool, wholeWord?: bool, regex?: bool, mask?: string}",
            Context::Global,
            has_project,
            find_in_files_run,
        ),
        spec(
            "edit.find.replaceInFilesRun",
            "Replace in Files (run)",
            None,
            "{pattern: string, replace: string, caseSensitive?: bool, wholeWord?: bool, regex?: bool, mask?: string}",
            Context::Global,
            has_project,
            replace_in_files_run,
        ),
    ]
}

pub(crate) fn run_op(
    s: &mut Session,
    f: impl FnOnce(&codecraft_text::Document, &SelectionSet, &ops::OpsConfig) -> codecraft_text::Edit,
) -> CmdResult {
    s.edit_active(EditGroup::Standalone, f)?;
    ok()
}

fn undo(s: &mut Session, _: &Value) -> CmdResult {
    let Some((editor, od)) = s.ws.active_mut() else { return ok() };
    match od.doc.undo() {
        Some(sels) => {
            editor.selections = sels.clamp(od.doc.len_chars());
            editor.reveal_caret = true;
            let id = editor.id;
            s.after_text_change(id);
            ok()
        }
        None => {
            s.status = "Nothing to undo".into();
            ok()
        }
    }
}

fn redo(s: &mut Session, _: &Value) -> CmdResult {
    let Some((editor, od)) = s.ws.active_mut() else { return ok() };
    match od.doc.redo() {
        Some(sels) => {
            editor.selections = sels.clamp(od.doc.len_chars());
            editor.reveal_caret = true;
            let id = editor.id;
            s.after_text_change(id);
            ok()
        }
        None => {
            s.status = "Nothing to redo".into();
            ok()
        }
    }
}

fn copy(s: &mut Session, _: &Value) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let (text, whole_line) = ops::copy_text(&d.doc, &e.selections);
    s.effects.push(Effect::CopyToClipboard(text.clone()));
    s.clipboard = Some((text.clone(), whole_line));
    Ok(json!({ "text": text }))
}

fn cut(s: &mut Session, _: &Value) -> CmdResult {
    let mut clip = None;
    s.edit_active(EditGroup::Standalone, |d, sel, _| {
        let (text, whole, edit) = ops::cut(d, sel);
        clip = Some((text, whole));
        edit
    })?;
    if let Some((text, whole)) = clip {
        s.effects.push(Effect::CopyToClipboard(text.clone()));
        s.clipboard = Some((text, whole));
    }
    ok()
}

fn paste(s: &mut Session, p: &Value) -> CmdResult {
    let (text, whole) = match opt_str(p, "text") {
        Some(t) => {
            // Text from the system clipboard is whole-line only if we copied it that way.
            let whole = match &s.clipboard {
                Some((c, w)) if c == t => *w,
                _ => bool_or(p, "wholeLine", false),
            };
            (t.to_string(), whole)
        }
        None => s.clipboard.clone().unwrap_or_default(),
    };
    if text.is_empty() {
        return ok();
    }
    s.edit_active(EditGroup::Standalone, |d, sel, _| ops::paste(d, sel, &text, whole))?;
    ok()
}

fn copy_path(s: &mut Session, p: &Value) -> CmdResult {
    let Some((_, d)) = s.ws.active() else { return ok() };
    let Some(path) = d.path.clone() else { return Err(CommandError::Failed("the file has no path".into())) };
    let text = match opt_str(p, "kind").unwrap_or("absolute") {
        "relative" => {
            s.ws.project
                .as_ref()
                .map(|pr| pr.relative(&path).into_owned())
                .unwrap_or_else(|| path.display().to_string())
        }
        "name" => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        _ => path.display().to_string(),
    };
    s.effects.push(Effect::CopyToClipboard(text.clone()));
    s.status = format!("Copied {text}");
    Ok(json!({ "text": text }))
}

// ---------------------------------------------------------------- find bar

fn open_find_bar(s: &mut Session, replace: bool) -> CmdResult {
    let selected = s.ws.active().map(|(e, d)| {
        let sel = e.selections.primary();
        if sel.is_empty() || d.doc.char_to_line(sel.start()) != d.doc.char_to_line(sel.end()) {
            String::new()
        } else {
            d.doc.slice(sel.range())
        }
    });
    let bar = s.find_bar.get_or_insert_with(FindBar::default);
    if let Some(text) = selected.filter(|t| !t.is_empty()) {
        bar.query.pattern = text;
    }
    if replace && bar.replace.is_none() {
        bar.replace = Some(String::new());
    } else if !replace {
        bar.replace = None;
    }
    bar.invalidate();
    s.refresh_find_matches();
    ok()
}

fn set_find_query(s: &mut Session, p: &Value) -> CmdResult {
    let bar = s.find_bar.get_or_insert_with(FindBar::default);
    if let Some(pat) = opt_str(p, "pattern") {
        bar.query.pattern = pat.to_string();
    }
    bar.query.case_sensitive = bool_or(p, "caseSensitive", bar.query.case_sensitive);
    bar.query.whole_word = bool_or(p, "wholeWord", bar.query.whole_word);
    bar.query.regex = bool_or(p, "regex", bar.query.regex);
    if let Some(r) = opt_str(p, "replace") {
        bar.replace = Some(r.to_string());
    }
    bar.invalidate();
    s.refresh_find_matches();
    if opt_str(p, "pattern").is_some() {
        crate::editing_cmds::scroll_to_find_result(s);
    }
    let n = s.find_bar.as_ref().map(|b| b.matches.len()).unwrap_or(0);
    Ok(json!({ "matches": n }))
}

impl Session {
    /// Recompute find-bar matches if the document or query changed; selects the match at or
    /// after the caret as current.
    pub fn refresh_find_matches(&mut self) {
        let Some((e, d)) = self.ws.active() else { return };
        let Some(bar) = &mut self.find_bar else { return };
        let key = (d.doc.revision(), bar.query.clone());
        if bar.computed_for.as_ref() == Some(&key) {
            return;
        }
        bar.error = None;
        bar.matches = if bar.query.pattern.is_empty() {
            Vec::new()
        } else {
            match search::find_all(&d.doc, &bar.query, 100_000) {
                Ok(m) => m,
                Err(err) => {
                    bar.error = Some(err.to_string());
                    Vec::new()
                }
            }
        };
        let caret = e.selections.primary().start();
        bar.current =
            bar.matches.iter().position(|r| r.start >= caret).or(if bar.matches.is_empty() { None } else { Some(0) });
        bar.computed_for = Some(key);
    }
}

fn find_step(s: &mut Session, forward: bool) -> CmdResult {
    if s.find_bar.is_none() {
        // Cmd+G without a find bar searches for the selection / word at caret.
        let word = s.ws.active().map(|(e, d)| {
            let sel = e.selections.primary();
            if sel.is_empty() {
                d.doc.slice(movement::word_range_at(&d.doc, sel.head))
            } else {
                d.doc.slice(sel.range())
            }
        });
        let Some(word) = word.filter(|w| !w.is_empty()) else { return ok() };
        s.find_bar = Some(FindBar {
            query: SearchQuery { case_sensitive: true, ..SearchQuery::plain(word) },
            ..FindBar::default()
        });
    }
    s.refresh_find_matches();
    let Some((e, d)) = s.ws.active() else { return ok() };
    let Some(bar) = &s.find_bar else { return ok() };
    let sel = e.selections.primary();
    let from = if forward { sel.end() } else { sel.start() };
    let found =
        search::find_next(&d.doc, &bar.query, from, forward, true).map_err(|e| CommandError::Failed(e.to_string()))?;
    match found {
        Some(r) => {
            let idx = bar.matches.iter().position(|m| *m == r);
            s.select_active(|_, _| SelectionSet::single(Selection::new(r.start, r.end)));
            if let Some(bar) = &mut s.find_bar {
                bar.current = idx;
            }
            ok()
        }
        None => {
            s.status = format!("'{}' not found", bar.query.pattern);
            ok()
        }
    }
}

fn word_step(s: &mut Session, forward: bool) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let sel = e.selections.primary();
    let range = movement::word_range_at(&d.doc, sel.head);
    let word = d.doc.slice(range.clone());
    if word.is_empty() {
        return ok();
    }
    let q = SearchQuery { pattern: word, case_sensitive: true, whole_word: true, regex: false };
    let from = if forward { range.end } else { range.start };
    if let Some(r) =
        search::find_next(&d.doc, &q, from, forward, true).map_err(|e| CommandError::Failed(e.to_string()))?
    {
        s.select_active(|_, _| SelectionSet::single(Selection::new(r.start, r.end)));
    }
    ok()
}

fn replace_current(s: &mut Session, _: &Value) -> CmdResult {
    s.refresh_find_matches();
    let Some(bar) = s.find_bar.clone() else { return ok() };
    let Some(replacement) = bar.replace.clone() else { return ok() };
    let Some(range) = bar.current.and_then(|i| bar.matches.get(i).cloned()) else { return ok() };
    let text = if bar.query.regex {
        let doc_text = s.ws.active().map(|(_, d)| d.doc.slice(range.clone())).unwrap_or_default();
        bar.query.to_regex().map(|re| re.replace(&doc_text, replacement.as_str()).into_owned()).unwrap_or(replacement)
    } else {
        replacement
    };
    s.edit_active(EditGroup::Standalone, |_, _, _| {
        let changes = codecraft_text::ChangeSet::single(range.start, range.end, text.clone());
        let end = range.start + text.chars().count();
        codecraft_text::Edit { changes, selections: SelectionSet::caret(end) }
    })?;
    find_step(s, true)
}

fn replace_all(s: &mut Session, _: &Value) -> CmdResult {
    let Some(bar) = s.find_bar.clone() else { return ok() };
    let Some(replacement) = bar.replace.clone() else { return ok() };
    let count = bar.matches.len();
    let mut err = None;
    s.edit_active(EditGroup::Standalone, |d, sel, _| match search::replace_all(d, &bar.query, &replacement, sel) {
        Ok(e) => e,
        Err(e) => {
            err = Some(e.to_string());
            codecraft_text::Edit::none(sel.clone())
        }
    })?;
    if let Some(e) = err {
        return Err(CommandError::Failed(e));
    }
    s.status = format!("Replaced {count} occurrence(s)");
    Ok(json!({ "replaced": count }))
}

// ---------------------------------------------------------------- selection

fn extend_selection(s: &mut Session, _: &Value) -> CmdResult {
    let Some((e, _)) = s.ws.active() else { return ok() };
    let (id, before) = (e.id, e.selections.clone());
    s.select_active(movement::extend_selection);
    let changed = s.ws.editor(id).is_some_and(|e| e.selections != before);
    if changed {
        s.selection_stack.push((id, before));
        if s.selection_stack.len() > 100 {
            s.selection_stack.remove(0);
        }
    }
    ok()
}

fn shrink_selection(s: &mut Session, _: &Value) -> CmdResult {
    let Some((e, _)) = s.ws.active() else { return ok() };
    let id = e.id;
    if let Some(pos) = s.selection_stack.iter().rposition(|(eid, _)| *eid == id) {
        let (_, sels) = s.selection_stack.remove(pos);
        s.select_active(|_, _| sels);
    }
    ok()
}

fn convert_indents(s: &mut Session, to_spaces: bool) -> CmdResult {
    let tab = s.ws.settings.tab_size.max(1);
    s.edit_active(EditGroup::Standalone, |d, sel, _| {
        let mut changes = Vec::new();
        for line in 0..d.len_lines() {
            let indent = d.indent_of_line(line);
            if indent.is_empty() {
                continue;
            }
            let width: usize = indent.chars().map(|c| if c == '\t' { tab } else { 1 }).sum();
            let new = if to_spaces {
                " ".repeat(width)
            } else {
                format!("{}{}", "\t".repeat(width / tab), " ".repeat(width % tab))
            };
            if new != indent {
                let start = d.line_start(line);
                changes.push(codecraft_text::Change::new(start, start + indent.chars().count(), new));
            }
        }
        match codecraft_text::ChangeSet::new(changes) {
            Ok(cs) => {
                let sels = sel.map(&cs);
                codecraft_text::Edit { changes: cs, selections: sels }
            }
            Err(_) => codecraft_text::Edit::none(sel.clone()),
        }
    })?;
    ok()
}

// ---------------------------------------------------------------- find in files

fn query_from(p: &Value) -> Result<SearchQuery, CommandError> {
    Ok(SearchQuery {
        pattern: crate::params::req_str(p, "pattern")?.to_string(),
        case_sensitive: bool_or(p, "caseSensitive", false),
        whole_word: bool_or(p, "wholeWord", false),
        regex: bool_or(p, "regex", false),
    })
}

fn find_in_files_popup(s: &mut Session, replace: bool) -> CmdResult {
    let pattern =
        s.ws.active()
            .map(|(e, d)| {
                let sel = e.selections.primary();
                if sel.is_empty() || d.doc.char_to_line(sel.start()) != d.doc.char_to_line(sel.end()) {
                    String::new()
                } else {
                    d.doc.slice(sel.range())
                }
            })
            .unwrap_or_default();
    s.popup = Some(crate::Popup::FindInFiles {
        query: SearchQuery { pattern, ..SearchQuery::default() },
        mask: String::new(),
        replace: replace.then(String::new),
    });
    ok()
}

fn find_in_files_run(s: &mut Session, p: &Value) -> CmdResult {
    let query = query_from(p)?;
    if query.pattern.is_empty() {
        return Err(CommandError::BadParams("the search text is empty".into()));
    }
    let mask = opt_str(p, "mask").map(str::to_string);
    s.start_find_in_files(query, mask).map_err(CommandError::Failed)?;
    ok()
}

/// Replace every match in the project's files (in open documents, undoable; others are loaded
/// as documents and saved by Save All).
fn replace_in_files_run(s: &mut Session, p: &Value) -> CmdResult {
    let query = query_from(p)?;
    let replacement = crate::params::req_str(p, "replace")?.to_string();
    if query.pattern.is_empty() {
        return Err(CommandError::BadParams("the search text is empty".into()));
    }
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    let regex = query.to_regex().map_err(|e| CommandError::BadParams(e.to_string()))?;
    let opts = codecraft_fs::FindOptions {
        file_mask: opt_str(p, "mask").filter(|m| !m.trim().is_empty()).map(str::to_string),
        ..codecraft_fs::FindOptions::default()
    };
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let matches =
        codecraft_fs::find_in_files(&root, &regex, &opts, &cancel).map_err(|e| CommandError::Failed(e.to_string()))?;
    let mut files: Vec<std::path::PathBuf> = matches.iter().map(|m| m.path.clone()).collect();
    files.dedup();
    let mut replaced = 0usize;
    for path in &files {
        let doc_id = match s.ws.doc_by_path(path) {
            Some(d) => d,
            None => {
                let text = codecraft_fs::read_text_file(path).map_err(|e| CommandError::Failed(e.to_string()))?;
                let id = s.ws.add_document(Some(path.clone()), &text);
                s.lsp_did_open(id);
                id
            }
        };
        let editor = s.ws.editor_for_doc(doc_id);
        let before = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
        let Some(od) = s.ws.doc_mut(doc_id) else { continue };
        let n = search::find_all(&od.doc, &query, usize::MAX).map(|v| v.len()).unwrap_or(0);
        let edit = search::replace_all(&od.doc, &query, &replacement, &before)
            .map_err(|e| CommandError::Failed(e.to_string()))?;
        od.doc.apply(&edit, &before, EditGroup::Standalone).map_err(|e| CommandError::Failed(e.to_string()))?;
        replaced += n;
        if let Some(e) = editor {
            s.ws.set_selections(e, edit.selections);
        }
    }
    let _ = s.save_all();
    s.status = format!("Replaced {replaced} occurrence(s) in {} file(s)", files.len());
    Ok(json!({ "replaced": replaced, "files": files.len() }))
}
