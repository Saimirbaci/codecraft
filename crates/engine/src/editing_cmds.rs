//! Editor depth (M3b, track E): live templates, surround with, column selection, soft wrap,
//! navigation bar and smaller editing actions.

use std::path::Path;

use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_text::column::{BoxPoint, box_point_of, box_selection};
use codecraft_text::movement::Motion;
use codecraft_text::wrap::{WrapConfig, move_visual};
use codecraft_text::{Change, ChangeSet, Edit, EditGroup, Selection, SelectionSet, ops};
use codecraft_workspace::EditorId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, ok, spec};
use crate::editing::{
    Chooser, ChooserItem, ChooserKind, ColumnBlock, ExpandWord, NavBar, NavItem, NavKind, NavSegment,
    SaveTemplateDraft, TemplateSession,
};
use crate::params::{bool_or, opt_str, opt_usize, req_usize};
use crate::structure::{self, Code};
use crate::templates::{self, ExpandContext, Template};
use crate::{CommandError, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ---------------------------------------------------------------- live templates
        spec(
            "editor.tab",
            "Tab",
            Some("Tab"),
            "Next live template variable, else expand the abbreviation before the caret, else indent",
            Context::Editor,
            has_editor,
            tab,
        ),
        spec(
            "editor.shiftTab",
            "Shift+Tab",
            Some("Shift+Tab"),
            "Previous live template variable, else unindent",
            Context::Editor,
            has_editor,
            shift_tab,
        ),
        spec(
            "editor.expandLiveTemplate",
            "Expand Live Template",
            None,
            "{abbreviation?: string} — expand the given template (or the abbreviation before the caret)",
            Context::Editor,
            has_editor,
            expand_live_template,
        ),
        spec(
            "code.insertLiveTemplate",
            "Insert Live Template…",
            None,
            "{abbreviation?: string} — insert directly; without it, open the template list",
            Context::Editor,
            has_editor,
            insert_live_template,
        ),
        spec(
            "code.surroundWith",
            "Surround With…",
            None,
            "{abbreviation?: string} — surround the selection (or line); without it, open the list",
            Context::Editor,
            has_editor,
            surround_with,
        ),
        spec(
            "code.saveAsLiveTemplate",
            "Save as Live Template…",
            None,
            "{abbreviation?: string, description?: string, text?: string, language?: string} — save; without an abbreviation, open the dialog for the selection",
            Context::Editor,
            has_editor,
            save_as_live_template,
        ),
        spec(
            "navigate.navigateInFile.nextLiveTemplateParameter",
            "Next Live Template Parameter",
            None,
            "",
            Context::Editor,
            has_editor,
            |s, _| step_variable(s, true).map(|_| Value::Null),
        ),
        spec(
            "navigate.navigateInFile.previousLiveTemplateParameter",
            "Previous Live Template Parameter",
            None,
            "",
            Context::Editor,
            has_editor,
            |s, _| step_variable(s, false).map(|_| Value::Null),
        ),
        spec(
            "code.unwrapRemove",
            "Unwrap/Remove…",
            Some("Shift+Cmd+Delete"),
            "{index?: int} — apply the given choice; without it, open the list (the result lists the choices)",
            Context::Editor,
            has_editor,
            unwrap_remove,
        ),
        // ---------------------------------------------------------------- choosers
        spec(
            "editor.chooser.move",
            "Move Chooser Selection",
            None,
            "{delta: int}",
            Context::Global,
            has_chooser,
            chooser_move,
        ),
        spec(
            "editor.chooser.setQuery",
            "Filter Chooser",
            None,
            "{query: string}",
            Context::Global,
            has_chooser,
            chooser_query,
        ),
        spec(
            "editor.chooser.accept",
            "Accept Chooser Item",
            None,
            "{index?: int} — index into the visible items (default: the selected one)",
            Context::Global,
            has_chooser,
            chooser_accept,
        ),
        spec("editor.chooser.close", "Close Chooser", None, "", Context::Global, always, |s, _| {
            s.editing.chooser = None;
            s.editing.save_dialog = None;
            ok()
        }),
        // ---------------------------------------------------------------- selection and wrap
        spec(
            "edit.columnSelectionMode",
            "Column Selection Mode",
            None,
            "{on?: bool} — toggle (or set) column selection mode for the active editor",
            Context::Global,
            has_editor,
            column_mode,
        ),
        spec(
            "editor.boxSelect",
            "Box Selection",
            None,
            "{anchor: {line, column}, head: {line, column}} — zero-based lines, display columns (tabs expanded)",
            Context::Editor,
            has_editor,
            box_select,
        ),
        // ---------------------------------------------------------------- navigation
        spec(
            "navigate.nextEditLocation",
            "Next Edit Location",
            None,
            "",
            Context::Global,
            always,
            |s, _| next_edit_location(s),
        ),
        spec(
            "navigate.jumpToNavigationBar",
            "Jump to Navigation Bar",
            None,
            "",
            Context::Global,
            has_editor,
            jump_to_navigation_bar,
        ),
        spec(
            "navbar.select",
            "Select Navigation Bar Element",
            None,
            "{index?: int, delta?: int}",
            Context::Global,
            has_navbar,
            navbar_select,
        ),
        spec(
            "navbar.moveItem",
            "Move Navigation Bar Popup Selection",
            None,
            "{delta: int}",
            Context::Global,
            has_navbar,
            navbar_move_item,
        ),
        spec(
            "navbar.accept",
            "Open Navigation Bar Item",
            None,
            "{index?: int} — open the item (file/symbol) or descend into a directory",
            Context::Global,
            has_navbar,
            navbar_accept,
        ),
        spec("navbar.close", "Close Navigation Bar", None, "", Context::Global, always, |s, _| {
            s.editing.navbar = None;
            ok()
        }),
        // ---------------------------------------------------------------- smaller actions
        spec(
            "code.codeCompletion.completeCurrentStatement",
            "Complete Current Statement",
            None,
            "",
            Context::Editor,
            has_editor,
            complete_current_statement,
        ),
        spec("code.autoIndentLines", "Auto-Indent Lines", None, "", Context::Editor, has_editor, auto_indent_lines),
        spec("code.optimizeImports", "Optimize Imports", None, "", Context::Global, has_editor, optimize_imports),
        spec("code.moveElementLeft", "Move Element Left", None, "", Context::Editor, has_editor, |s, _| {
            move_element(s, false)
        }),
        spec("code.moveElementRight", "Move Element Right", None, "", Context::Editor, has_editor, |s, _| {
            move_element(s, true)
        }),
        spec(
            "edit.find.scrollToResultsDuringTyping",
            "Scroll to Results During Typing",
            None,
            "{on?: bool}",
            Context::Global,
            always,
            |s, p| {
                let on = bool_or(p, "on", !s.ws.settings.find_scroll_to_results);
                s.ws.settings.find_scroll_to_results = on;
                s.status = format!("Scroll to results during typing: {}", if on { "on" } else { "off" });
                Ok(json!({ "on": on }))
            },
        ),
        CommandSpec {
            alt_shortcuts: &["Shift+Alt+/"],
            ..spec(
                "code.codeCompletion.cyclicExpandWord",
                "Cyclic Expand Word",
                None,
                "",
                Context::Editor,
                has_editor,
                |s, _| cyclic_expand(s, false),
            )
        },
        CommandSpec {
            alt_shortcuts: &["Shift+Alt+?"],
            ..spec(
                "code.codeCompletion.cyclicExpandWordBackward",
                "Cyclic Expand Word (Backward)",
                None,
                "",
                Context::Editor,
                has_editor,
                |s, _| cyclic_expand(s, true),
            )
        },
    ]
}

fn has_chooser(s: &Session) -> Result<(), String> {
    s.editing.chooser.as_ref().map(|_| ()).ok_or_else(|| "no chooser is open".into())
}

fn has_navbar(s: &Session) -> Result<(), String> {
    s.editing.navbar.as_ref().map(|_| ()).ok_or_else(|| "the navigation bar is not active".into())
}

fn active_language(s: &Session) -> Language {
    s.ws.active().map(|(_, d)| d.language).unwrap_or_default()
}

fn active_revision(s: &Session) -> Option<(EditorId, u64)> {
    let (e, d) = s.ws.active()?;
    Some((e.id, d.doc.revision()))
}

/// The identifier before the caret (single caret, no selection): `(start, word)`.
fn word_before_caret(s: &Session) -> Option<(usize, String)> {
    let (e, d) = s.ws.active()?;
    let sel = e.selections.primary();
    if !sel.is_empty() || e.selections.len() != 1 {
        return None;
    }
    let caret = sel.head;
    let mut start = caret;
    while start > 0 && d.doc.char_at(start - 1).is_some_and(|c| c.is_alphanumeric() || c == '_') {
        start -= 1;
    }
    if d.doc.char_at(caret).is_some_and(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((start, d.doc.slice(start..caret)))
}

// ------------------------------------------------------------------------------- expansion

/// Replace `range` with an expanded template and start a live template session.
fn apply_template(s: &mut Session, t: &Template, range: std::ops::Range<usize>, selection: Option<String>) -> CmdResult {
    let Some((eid, did)) = s.active_ids() else { return ok() };
    let indent_unit = s.ops_config(did).indent_unit;
    let indent = s
        .ws
        .doc(did)
        .map(|d| {
            let line = d.doc.char_to_line(range.start.min(d.doc.len_chars()));
            d.doc.indent_of_line(line)
        })
        .unwrap_or_default();
    let exp = templates::expand(t, &ExpandContext { indent, indent_unit, selection });
    let start = range.start;
    let len = exp.text.chars().count();
    let vars: Vec<Vec<std::ops::Range<usize>>> =
        exp.vars.iter().map(|occ| occ.iter().map(|r| start + r.start..start + r.end).collect()).collect();
    let end = start + exp.end.unwrap_or(len);
    let first_sel = selection_for(vars.first().map(Vec::as_slice), end);
    let text = exp.text.clone();
    s.edit_active(EditGroup::Standalone, |d, _, _| {
        let r = range.start.min(d.len_chars())..range.end.min(d.len_chars());
        Edit { changes: ChangeSet::single(r.start, r.end, text), selections: first_sel }
    })?;
    let revision = s.ws.doc(did).map(|d| d.doc.revision()).unwrap_or(0);
    s.editing.session =
        (!vars.is_empty()).then_some(TemplateSession { editor: eid, revision, vars, current: 0, end: Some(end) });
    s.editing.chooser = None;
    Ok(json!({ "expanded": t.abbreviation, "text": exp.text }))
}

/// Selections covering every occurrence of a variable (typing edits them all), or a caret.
fn selection_for(ranges: Option<&[std::ops::Range<usize>]>, fallback: usize) -> SelectionSet {
    match ranges {
        Some(rs) if !rs.is_empty() => {
            SelectionSet::from_vec(rs.iter().map(|r| Selection::new(r.start, r.end)).collect(), 0)
        }
        _ => SelectionSet::caret(fallback),
    }
}

/// Move to the next/previous variable of the live template session. Returns false when no
/// session applies (the caller falls back to plain Tab behaviour).
fn step_variable(s: &mut Session, forward: bool) -> Result<bool, CommandError> {
    let Some((eid, revision)) = active_revision(s) else { return Ok(false) };
    let caret = s.ws.active().map(|(e, _)| e.selections.primary().head).unwrap_or(0);
    let Some(t) = s.editing.session.clone() else { return Ok(false) };
    let inside = t.vars.iter().flatten().any(|r| r.start <= caret && caret <= r.end) || t.end == Some(caret);
    if t.editor != eid || t.revision != revision || !inside {
        s.editing.session = None;
        return Ok(false);
    }
    let mut t = t;
    if forward {
        if t.current + 1 < t.vars.len() {
            t.current += 1;
        } else {
            let end = t.end.unwrap_or(caret);
            s.editing.session = None;
            s.select_active(|_, _| SelectionSet::caret(end));
            return Ok(true);
        }
    } else {
        t.current = t.current.saturating_sub(1);
    }
    let sels = selection_for(Some(t.current_ranges()), caret);
    s.editing.session = Some(t);
    s.select_active(|_, _| sels);
    Ok(true)
}

fn tab(s: &mut Session, _: &Value) -> CmdResult {
    if step_variable(s, true)? {
        return ok();
    }
    if let Some((start, word)) = word_before_caret(s).filter(|(_, w)| !w.is_empty()) {
        let lang = active_language(s);
        if let Some(t) = s.editing.templates.find(lang, &word).filter(|t| !t.is_surround()) {
            let caret = start + word.chars().count();
            return apply_template(s, &t, start..caret, None);
        }
    }
    s.edit_active(EditGroup::Standalone, ops::indent)?;
    ok()
}

fn shift_tab(s: &mut Session, _: &Value) -> CmdResult {
    if step_variable(s, false)? {
        return ok();
    }
    s.edit_active(EditGroup::Standalone, ops::unindent)?;
    ok()
}

fn expand_live_template(s: &mut Session, p: &Value) -> CmdResult {
    let lang = active_language(s);
    let before = word_before_caret(s);
    let (abbr, range) = match opt_str(p, "abbreviation") {
        Some(a) => {
            // Replace a matching prefix before the caret, else insert at the caret.
            let caret = s.ws.active().map(|(e, _)| e.selections.primary().head).unwrap_or(0);
            let range = before
                .filter(|(_, w)| !w.is_empty() && a.starts_with(w.as_str()))
                .map(|(st, _)| st..caret)
                .unwrap_or(caret..caret);
            (a.to_string(), range)
        }
        None => {
            let (st, w) = before.filter(|(_, w)| !w.is_empty()).ok_or_else(|| {
                CommandError::Failed("no template abbreviation before the caret".into())
            })?;
            let end = st + w.chars().count();
            (w, st..end)
        }
    };
    let t = s
        .editing
        .templates
        .find(lang, &abbr)
        .ok_or_else(|| CommandError::Failed(format!("no live template `{abbr}` for {}", lang.name())))?;
    let sel_range = s.ws.active().map(|(e, _)| e.selections.primary().range()).unwrap_or(0..0);
    let range = if sel_range.is_empty() { range } else { sel_range };
    apply_template(s, &t, range, None)
}

fn chooser_items(list: &[Template]) -> Vec<ChooserItem> {
    list.iter()
        .map(|t| ChooserItem { label: t.abbreviation.clone(), detail: t.description.clone(), key: t.abbreviation.clone() })
        .collect()
}

fn insert_live_template(s: &mut Session, p: &Value) -> CmdResult {
    if opt_str(p, "abbreviation").is_some() {
        return expand_live_template(s, p);
    }
    let Some((eid, _)) = s.active_ids() else { return ok() };
    let lang = active_language(s);
    let prefix = word_before_caret(s).map(|(_, w)| w).unwrap_or_default();
    let all: Vec<Template> = s.editing.templates.for_language(lang).into_iter().filter(|t| !t.is_surround()).collect();
    if all.is_empty() {
        s.status = format!("No live templates for {}", lang.name());
        return ok();
    }
    let matching: Vec<Template> = all.iter().filter(|t| t.abbreviation.starts_with(&prefix)).cloned().collect();
    let list = if prefix.is_empty() || matching.is_empty() { all } else { matching };
    let items = chooser_items(&list);
    let result = json!({ "items": items.iter().map(|i| &i.label).collect::<Vec<_>>() });
    s.editing.chooser = Some(Chooser {
        kind: ChooserKind::InsertTemplate,
        editor: eid,
        title: "Insert Live Template".into(),
        items,
        query: String::new(),
        selected: 0,
    });
    Ok(result)
}

/// The range Surround With replaces and the selection text for `$SELECTION$`: whole lines
/// (dedented) when the selection is empty, spans lines or covers a line's text; else the
/// selection itself.
fn surround_target(s: &Session) -> Option<(std::ops::Range<usize>, String)> {
    let (e, d) = s.ws.active()?;
    let doc = &d.doc;
    let sel = e.selections.primary().clamp(doc.len_chars());
    let (l0, mut l1) = (doc.char_to_line(sel.start()), doc.char_to_line(sel.end()));
    if l1 > l0 && sel.end() == doc.line_start(l1) {
        l1 -= 1;
    }
    let first_text = doc.line_start(l0) + doc.indent_of_line(l0).chars().count();
    let line_like = sel.is_empty() || l1 > l0 || (sel.start() <= first_text && sel.end() >= doc.line_end(l1));
    if !line_like {
        return Some((sel.range(), doc.slice(sel.range())));
    }
    let end = doc.line_end(l1);
    let text = templates::dedent(&doc.slice(doc.line_start(l0)..end));
    Some((first_text.min(end)..end, text))
}

fn surround_with(s: &mut Session, p: &Value) -> CmdResult {
    let lang = active_language(s);
    if let Some(abbr) = opt_str(p, "abbreviation") {
        let t = s
            .editing
            .templates
            .find(lang, abbr)
            .filter(Template::is_surround)
            .ok_or_else(|| CommandError::Failed(format!("no surround template `{abbr}` for {}", lang.name())))?;
        let Some((range, text)) = surround_target(s) else { return ok() };
        return apply_template(s, &t, range, Some(text));
    }
    let Some((eid, _)) = s.active_ids() else { return ok() };
    let list: Vec<Template> = s.editing.templates.for_language(lang).into_iter().filter(Template::is_surround).collect();
    if list.is_empty() {
        s.status = format!("Nothing to surround with in {}", lang.name());
        return ok();
    }
    let items: Vec<ChooserItem> = list
        .iter()
        .map(|t| ChooserItem { label: t.description.clone(), detail: t.abbreviation.clone(), key: t.abbreviation.clone() })
        .collect();
    let result = json!({ "items": items.iter().map(|i| &i.key).collect::<Vec<_>>() });
    s.editing.chooser = Some(Chooser {
        kind: ChooserKind::SurroundWith,
        editor: eid,
        title: "Surround With".into(),
        items,
        query: String::new(),
        selected: 0,
    });
    Ok(result)
}

fn save_as_live_template(s: &mut Session, p: &Value) -> CmdResult {
    let lang = active_language(s);
    let selected = s
        .ws
        .active()
        .map(|(e, d)| d.doc.slice(e.selections.primary().range()))
        .filter(|t| !t.trim().is_empty())
        .map(|t| templates::dedent(&t).replace('$', "$$"));
    let Some(abbr) = opt_str(p, "abbreviation").map(str::trim) else {
        let text = selected.ok_or_else(|| CommandError::Failed("select the code to save as a live template".into()))?;
        s.editing.save_dialog =
            Some(SaveTemplateDraft { text, language: lang.lsp_id().to_string(), ..SaveTemplateDraft::default() });
        return ok();
    };
    let fail = |s: &mut Session, msg: String| {
        if let Some(d) = &mut s.editing.save_dialog {
            d.error = Some(msg.clone());
        }
        Err(CommandError::Failed(msg))
    };
    if abbr.is_empty() || !abbr.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return fail(s, "the abbreviation must be a word (letters, digits, `_`)".into());
    }
    let text = match opt_str(p, "text").map(str::to_string).or(selected) {
        Some(t) if !t.trim().is_empty() => t,
        _ => return fail(s, "the template text is empty".into()),
    };
    let language = opt_str(p, "language").filter(|l| !l.is_empty()).unwrap_or(lang.lsp_id()).to_string();
    let mut t = Template::new(abbr, opt_str(p, "description").unwrap_or(""), &text, &[]);
    t.languages = vec![language];
    match s.editing.templates.add_user(t) {
        Ok(path) => {
            s.editing.save_dialog = None;
            s.status = format!("Saved live template `{abbr}` to {}", path.display());
            Ok(json!({ "path": path }))
        }
        Err(e) => fail(s, e),
    }
}

fn unwrap_remove(s: &mut Session, p: &Value) -> CmdResult {
    let Some((eid, _)) = s.active_ids() else { return ok() };
    let Some((caret, lang, text)) =
        s.ws.active().map(|(e, d)| (e.selections.primary().head, d.language, d.doc.text()))
    else {
        return ok();
    };
    let code = Code::new(lang, &text);
    let options = structure::unwrap_options(lang, &code, caret);
    let labels: Vec<String> = options.iter().map(|o| o.label.clone()).collect();
    if options.is_empty() {
        s.status = "Nothing to unwrap or remove here".into();
        return Ok(json!({ "options": labels }));
    }
    if let Some(i) = opt_usize(p, "index")? {
        let o = options.get(i).cloned().ok_or_else(|| CommandError::BadParams(format!("no choice {i}")))?;
        s.edit_active(EditGroup::Standalone, |d, _, _| {
            let r = o.range.start.min(d.len_chars())..o.range.end.min(d.len_chars());
            Edit { changes: ChangeSet::single(r.start, r.end, o.text.clone()), selections: SelectionSet::caret(r.start) }
        })?;
        s.editing.chooser = None;
        return Ok(json!({ "applied": o.label }));
    }
    let items = options
        .iter()
        .enumerate()
        .map(|(i, o)| ChooserItem { label: o.label.clone(), detail: String::new(), key: i.to_string() })
        .collect();
    s.editing.chooser = Some(Chooser {
        kind: ChooserKind::UnwrapRemove,
        editor: eid,
        title: "Unwrap/Remove".into(),
        items,
        query: String::new(),
        selected: 0,
    });
    Ok(json!({ "options": labels }))
}

fn chooser_move(s: &mut Session, p: &Value) -> CmdResult {
    let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1);
    if let Some(c) = &mut s.editing.chooser {
        let n = c.visible().len();
        if n > 0 {
            c.selected = usize::try_from((c.selected as i64 + delta).rem_euclid(n as i64)).unwrap_or(0);
        }
    }
    ok()
}

fn chooser_query(s: &mut Session, p: &Value) -> CmdResult {
    let q = opt_str(p, "query").unwrap_or("").to_string();
    if let Some(c) = &mut s.editing.chooser {
        c.query = q;
        c.selected = 0;
    }
    ok()
}

fn chooser_accept(s: &mut Session, p: &Value) -> CmdResult {
    let Some(mut c) = s.editing.chooser.take() else { return ok() };
    if let Some(i) = opt_usize(p, "index")? {
        c.selected = i;
    }
    let Some(item) = c.selected_item().cloned() else { return ok() };
    if s.ws.active_editor() != Some(c.editor) {
        return Err(CommandError::Failed("the editor changed".into()));
    }
    match c.kind {
        ChooserKind::InsertTemplate => expand_live_template(s, &json!({ "abbreviation": item.key })),
        ChooserKind::SurroundWith => surround_with(s, &json!({ "abbreviation": item.key })),
        ChooserKind::UnwrapRemove => {
            let index = item.key.parse::<usize>().unwrap_or(0);
            unwrap_remove(s, &json!({ "index": index }))
        }
    }
}

// ------------------------------------------------------------------------------- column / wrap

fn column_mode(s: &mut Session, p: &Value) -> CmdResult {
    let Some((eid, _)) = s.active_ids() else { return ok() };
    let on = bool_or(p, "on", !s.editing.column_mode.contains(&eid));
    if on {
        s.editing.column_mode.insert(eid);
    } else {
        s.editing.column_mode.remove(&eid);
        s.editing.block = None;
    }
    s.status = format!("Column selection mode {}", if on { "on" } else { "off" });
    Ok(json!({ "on": on }))
}

fn point(p: &Value, key: &str) -> Result<BoxPoint, CommandError> {
    let v = p.get(key).ok_or_else(|| CommandError::BadParams(format!("missing `{key}`")))?;
    Ok(BoxPoint::new(req_usize(v, "line")?, req_usize(v, "column")?))
}

fn set_block(s: &mut Session, anchor: BoxPoint, head: BoxPoint) {
    let tab = s.ws.settings.tab_size.max(1);
    s.select_active(|d, _| box_selection(d, anchor, head, tab));
    if let Some((eid, revision)) = active_revision(s) {
        s.editing.block = Some(ColumnBlock { editor: eid, anchor, head, revision });
    }
}

fn box_select(s: &mut Session, p: &Value) -> CmdResult {
    let anchor = point(p, "anchor")?;
    let head = point(p, "head")?;
    s.popup = None;
    set_block(s, anchor, head);
    let n = s.ws.active().map(|(e, _)| e.selections.len()).unwrap_or(0);
    Ok(json!({ "carets": n }))
}

/// Hook from the editor's caret motion: column-mode Shift+arrows grow a box; with soft wrap,
/// Up/Down move by visual row. `None` = not handled.
pub(crate) fn motion_hook(s: &mut Session, motion: Motion, extend: bool) -> Option<CmdResult> {
    let (eid, revision) = active_revision(s)?;
    let tab = s.ws.settings.tab_size.max(1);
    if extend
        && s.editing.column_mode.contains(&eid)
        && matches!(motion, Motion::Left | Motion::Right | Motion::Up | Motion::Down)
    {
        let (block, sels, last_line) = {
            let (e, d) = s.ws.active()?;
            (s.editing.block, e.selections.clone(), d.doc.len_lines().saturating_sub(1))
        };
        let current = block.filter(|b| {
            b.editor == eid
                && b.revision == revision
                && s.ws.active().is_some_and(|(_, d)| box_selection(&d.doc, b.anchor, b.head, tab) == sels)
        });
        let (anchor, mut head) = match current {
            Some(b) => (b.anchor, b.head),
            None => {
                let (_, d) = s.ws.active()?;
                let p = sels.primary();
                (box_point_of(&d.doc, p.anchor, tab), box_point_of(&d.doc, p.head, tab))
            }
        };
        match motion {
            Motion::Up => head.line = head.line.saturating_sub(1),
            Motion::Down => head.line = (head.line + 1).min(last_line),
            Motion::Left => head.column = head.column.saturating_sub(1),
            _ => head.column = head.column.saturating_add(1).min(1_000_000),
        }
        set_block(s, anchor, head);
        return Some(ok());
    }
    if s.ws.settings.soft_wrap
        && matches!(motion, Motion::Up | Motion::Down)
        && let Some(width) = s.editing.wrap_widths.get(&eid).copied()
    {
        let cfg = WrapConfig::new(width, tab);
        s.select_active(|d, sel| move_visual(d, sel, motion == Motion::Down, cfg, extend));
        return Some(ok());
    }
    None
}

// ------------------------------------------------------------------------------- edit locations

/// Navigate › Last Edit Location: step back through the places text was edited.
pub(crate) fn last_edit_location(s: &mut Session) -> CmdResult {
    let len = s.editing.history.places.len();
    if len == 0 {
        s.status = "No edit locations".into();
        return ok();
    }
    let here = s.ws.active().and_then(|(e, d)| Some((d.path.clone()?, d.doc.char_to_line(e.selections.primary().head))));
    let target = match s.editing.history.cursor {
        None => {
            let last = len - 1;
            let at_last = s
                .editing
                .history
                .places
                .get(last)
                .zip(here.as_ref())
                .is_some_and(|((loc, line), (p, l))| loc.path == *p && line.abs_diff(*l) <= 1);
            if at_last {
                if last == 0 {
                    s.status = "No previous edit location".into();
                    return ok();
                }
                last - 1
            } else {
                last
            }
        }
        Some(0) => {
            s.status = "No previous edit location".into();
            return ok();
        }
        Some(i) => i - 1,
    };
    go_to_edit(s, target)
}

fn next_edit_location(s: &mut Session) -> CmdResult {
    let len = s.editing.history.places.len();
    match s.editing.history.cursor {
        Some(i) if i + 1 < len => go_to_edit(s, i + 1),
        _ => {
            s.status = "No next edit location".into();
            ok()
        }
    }
}

fn go_to_edit(s: &mut Session, index: usize) -> CmdResult {
    let Some((loc, _)) = s.editing.history.places.get(index).cloned() else { return ok() };
    s.navigate_to(&loc.path, loc.offset)?;
    s.editing.history.cursor = Some(index);
    Ok(json!({ "path": loc.path, "offset": loc.offset }))
}

// ------------------------------------------------------------------------------- navigation bar

fn nav_children_of_dir(s: &Session, dir: &Path) -> Vec<NavItem> {
    let mut items: Vec<NavItem> = Vec::new();
    let from_tree = s.ws.project.as_ref().and_then(|p| {
        let tree = &p.tree;
        let id = tree.find(dir)?;
        let node = tree.node(id)?;
        Some(
            node.children
                .iter()
                .filter_map(|c| tree.node(*c))
                .map(|n| NavItem {
                    label: n.name.clone(),
                    kind: if n.is_dir { NavKind::Directory } else { NavKind::File },
                    path: n.path.clone(),
                    offset: None,
                })
                .collect::<Vec<_>>(),
        )
    });
    match from_tree {
        Some(v) => items = v,
        None => {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten().take(1000) {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.starts_with('.') {
                        continue;
                    }
                    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    items.push(NavItem {
                        label: name,
                        kind: if is_dir { NavKind::Directory } else { NavKind::File },
                        path: entry.path(),
                        offset: None,
                    });
                }
            }
        }
    }
    items.sort_by_key(|a| (a.kind != NavKind::Directory, a.label.to_lowercase()));
    items
}

/// The contents shown for a navigation bar element.
fn nav_items(s: &Session, bar: &NavBar, index: usize) -> Vec<NavItem> {
    let Some(seg) = bar.segments.get(index) else { return Vec::new() };
    match seg.kind {
        NavKind::Directory => nav_children_of_dir(s, &seg.path),
        NavKind::File | NavKind::Symbol => {
            let Some((lang, text)) = s
                .ws
                .editor(bar.editor)
                .and_then(|e| s.ws.doc(e.doc))
                .filter(|d| d.path.as_deref() == Some(seg.path.as_path()))
                .map(|d| (d.language, d.doc.text()))
            else {
                return Vec::new();
            };
            let ex = structure::extents(lang, &text);
            let item = |e: &structure::Extent| NavItem {
                label: e.label(),
                kind: NavKind::Symbol,
                path: seg.path.clone(),
                offset: Some(e.offset),
            };
            match (seg.offset, seg.end) {
                (Some(off), Some(end)) => {
                    let depth = ex.iter().find(|e| e.offset == off).map(|e| e.depth).unwrap_or(0);
                    let children: Vec<NavItem> = ex
                        .iter()
                        .filter(|e| e.depth == depth + 1 && e.offset > off && e.offset <= end)
                        .map(item)
                        .collect();
                    if !children.is_empty() {
                        return children;
                    }
                    // A leaf: its siblings.
                    let parent = structure::crumbs_at(&ex, off).into_iter().rev().find(|e| e.depth < depth);
                    ex.iter()
                        .filter(|e| {
                            e.depth == depth && parent.as_ref().is_none_or(|p| e.offset > p.offset && e.offset <= p.end)
                        })
                        .map(item)
                        .collect()
                }
                _ => ex.iter().filter(|e| e.depth == 0).map(item).collect(),
            }
        }
    }
}

fn nav_segments(s: &Session) -> Option<(EditorId, Vec<NavSegment>)> {
    let (e, d) = s.ws.active()?;
    let path = d.path.clone()?;
    let root = s
        .ws
        .project
        .as_ref()
        .map(|p| p.root.clone())
        .filter(|r| path.starts_with(r))
        .or_else(|| path.parent().map(Path::to_path_buf))?;
    let mut segments = Vec::new();
    let root_label = s
        .ws
        .project
        .as_ref()
        .filter(|p| p.root == root)
        .map(|p| p.name.clone())
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| root.display().to_string());
    segments.push(NavSegment { label: root_label, kind: NavKind::Directory, path: root.clone(), offset: None, end: None });
    let mut dir = root.clone();
    if let Some(parent) = path.parent()
        && let Ok(rel) = parent.strip_prefix(&root)
    {
        for c in rel.components() {
            dir = dir.join(c);
            segments.push(NavSegment {
                label: c.as_os_str().to_string_lossy().into_owned(),
                kind: NavKind::Directory,
                path: dir.clone(),
                offset: None,
                end: None,
            });
        }
    }
    segments.push(NavSegment { label: d.title(), kind: NavKind::File, path: path.clone(), offset: None, end: None });
    let caret = e.selections.primary().head;
    let ex = structure::extents(d.language, &d.doc.text());
    for c in structure::crumbs_at(&ex, caret) {
        segments.push(NavSegment {
            label: c.label(),
            kind: NavKind::Symbol,
            path: path.clone(),
            offset: Some(c.offset),
            end: Some(c.end),
        });
    }
    Some((e.id, segments))
}

fn jump_to_navigation_bar(s: &mut Session, _: &Value) -> CmdResult {
    let (editor, segments) = nav_segments(s)
        .ok_or_else(|| CommandError::Failed("the navigation bar needs a file saved on disk".into()))?;
    // RustRover selects the file element (the last path element before symbols).
    let selected = segments.iter().rposition(|x| x.kind == NavKind::File).unwrap_or(0);
    let mut bar = NavBar { editor, segments, selected, items: Vec::new(), item_selected: 0 };
    bar.items = nav_items(s, &bar, selected);
    let labels: Vec<String> = bar.segments.iter().map(|x| x.label.clone()).collect();
    s.editing.navbar = Some(bar);
    Ok(json!({ "segments": labels }))
}

fn navbar_select(s: &mut Session, p: &Value) -> CmdResult {
    let Some(mut bar) = s.editing.navbar.take() else { return ok() };
    let n = bar.segments.len().max(1);
    let index = match opt_usize(p, "index")? {
        Some(i) => i.min(n - 1),
        None => {
            let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1);
            usize::try_from((bar.selected as i64 + delta).clamp(0, n as i64 - 1)).unwrap_or(0)
        }
    };
    bar.selected = index;
    bar.items = nav_items(s, &bar, index);
    bar.item_selected = 0;
    let items: Vec<String> = bar.items.iter().map(|i| i.label.clone()).collect();
    s.editing.navbar = Some(bar);
    Ok(json!({ "items": items }))
}

fn navbar_move_item(s: &mut Session, p: &Value) -> CmdResult {
    let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1);
    if let Some(bar) = &mut s.editing.navbar {
        let n = bar.items.len();
        if n > 0 {
            bar.item_selected = usize::try_from((bar.item_selected as i64 + delta).rem_euclid(n as i64)).unwrap_or(0);
        }
    }
    ok()
}

fn navbar_accept(s: &mut Session, p: &Value) -> CmdResult {
    let Some(mut bar) = s.editing.navbar.take() else { return ok() };
    let index = opt_usize(p, "index")?.unwrap_or(bar.item_selected);
    let Some(item) = bar.items.get(index).cloned() else {
        s.editing.navbar = Some(bar);
        return ok();
    };
    match item.kind {
        NavKind::Directory => {
            bar.segments.truncate(bar.selected + 1);
            bar.segments.push(NavSegment {
                label: item.label.clone(),
                kind: NavKind::Directory,
                path: item.path.clone(),
                offset: None,
                end: None,
            });
            bar.selected = bar.segments.len() - 1;
            bar.items = nav_items(s, &bar, bar.selected);
            bar.item_selected = 0;
            s.editing.navbar = Some(bar);
            Ok(json!({ "opened": item.path }))
        }
        NavKind::File => {
            s.open_file(&item.path)?;
            Ok(json!({ "opened": item.path }))
        }
        NavKind::Symbol => {
            s.navigate_to(&item.path, item.offset.unwrap_or(0))?;
            Ok(json!({ "opened": item.path, "offset": item.offset }))
        }
    }
}

// ------------------------------------------------------------------------------- smaller actions

fn complete_current_statement(s: &mut Session, _: &Value) -> CmdResult {
    let Some((_, did)) = s.active_ids() else { return ok() };
    let unit = s.ops_config(did).indent_unit;
    let lang = active_language(s);
    s.popup = None;
    s.edit_active(EditGroup::Standalone, |d, sel, _| {
        let caret = sel.primary().head.min(d.len_chars());
        let code = Code::new(lang, &d.text());
        let line = code.line_of(caret);
        let (text, rel) = structure::complete_statement(lang, &code, line, &unit);
        let start = code.line_start(line);
        let end = code.line_end(line);
        Edit { changes: ChangeSet::single(start, end, text), selections: SelectionSet::caret(start + rel) }
    })?;
    ok()
}

fn auto_indent_lines(s: &mut Session, _: &Value) -> CmdResult {
    let lang = active_language(s);
    if !structure::is_brace_language(lang) {
        s.status = format!("Auto-Indent Lines keeps {} indentation as is", lang.name());
        return ok();
    }
    let Some((_, did)) = s.active_ids() else { return ok() };
    let unit = s.ops_config(did).indent_unit;
    s.edit_active(EditGroup::Standalone, |d, sels, _| {
        let code = Code::new(lang, &d.text());
        let mut changes = Vec::new();
        let mut lines: Vec<usize> = Vec::new();
        for r in sels.ranges() {
            let (l0, l1) = (code.line_of(r.start()), code.line_of(r.end()));
            lines.extend(l0..=l1);
        }
        lines.sort_unstable();
        lines.dedup();
        let (Some(first), Some(last)) = (lines.first().copied(), lines.last().copied()) else {
            return Edit::none(sels.clone());
        };
        for (line, indent) in structure::auto_indent(&code, first..last + 1, &unit) {
            if lines.binary_search(&line).is_err() {
                continue;
            }
            let start = code.line_start(line);
            let old = code.indent_of(line).chars().count();
            changes.push(Change::new(start, start + old, indent));
        }
        match ChangeSet::new(changes) {
            Ok(cs) => {
                let selections = sels.map(&cs);
                Edit { changes: cs, selections }
            }
            Err(_) => Edit::none(sels.clone()),
        }
    })?;
    ok()
}

fn move_element(s: &mut Session, right: bool) -> CmdResult {
    let lang = active_language(s);
    let mut moved = false;
    s.edit_active(EditGroup::Standalone, |d, sels, _| {
        let code = Code::new(lang, &d.text());
        match structure::move_element(&code, sels.primary().head, right) {
            Some((range, text, caret)) => {
                moved = true;
                Edit { changes: ChangeSet::single(range.start, range.end, text), selections: SelectionSet::caret(caret) }
            }
            None => Edit::none(sels.clone()),
        }
    })?;
    if !moved {
        s.status = "Nothing to move here".into();
    }
    Ok(json!({ "moved": moved }))
}

fn cyclic_expand(s: &mut Session, backward: bool) -> CmdResult {
    let Some((eid, revision)) = active_revision(s) else { return ok() };
    let caret = s.ws.active().map(|(e, _)| e.selections.primary().head).unwrap_or(0);
    let state = s.editing.expand.clone().filter(|x| x.editor == eid && x.revision == revision && x.caret == caret);
    let mut state = match state {
        Some(mut st) => {
            // Cycle; the original prefix is the last stop.
            let n = st.candidates.len() + 1;
            st.index = if backward { (st.index + n - 1) % n } else { (st.index + 1) % n };
            st
        }
        None => {
            let Some((start, prefix)) = word_before_caret(s).filter(|(_, w)| !w.is_empty()) else {
                s.status = "No word to expand".into();
                return ok();
            };
            let chars: Vec<char> = s.ws.active().map(|(_, d)| d.doc.text().chars().collect()).unwrap_or_default();
            let candidates = structure::expand_candidates(&chars, start, &prefix, backward);
            if candidates.is_empty() {
                s.status = format!("No completion for `{prefix}`");
                return ok();
            }
            ExpandWord { editor: eid, start, prefix, candidates, index: 0, revision, caret }
        }
    };
    let word = state.candidates.get(state.index).cloned().unwrap_or_else(|| state.prefix.clone());
    let (start, end) = (state.start, caret);
    s.editing.own_edit = true;
    let r = s.edit_active(EditGroup::Standalone, |d, _, _| {
        let n = word.chars().count();
        Edit {
            changes: ChangeSet::single(start.min(d.len_chars()), end.min(d.len_chars()), word.clone()),
            selections: SelectionSet::caret(start + n),
        }
    });
    s.editing.own_edit = false;
    r?;
    if let Some((_, rev)) = active_revision(s) {
        state.revision = rev;
    }
    state.caret = state.start + word.chars().count();
    s.editing.expand = Some(state);
    Ok(json!({ "word": word }))
}

/// Hook from the find bar: with Scroll to Results During Typing on, select the current match
/// so the editor scrolls to it while the query is typed.
pub(crate) fn scroll_to_find_result(s: &mut Session) {
    if !s.ws.settings.find_scroll_to_results {
        return;
    }
    let Some(m) = s.find_bar.as_ref().and_then(|b| b.current.and_then(|i| b.matches.get(i)).cloned()) else {
        return;
    };
    s.select_active(|_, _| SelectionSet::single(Selection::new(m.start, m.end)));
}

// ------------------------------------------------------------------------------- optimize imports

fn optimize_imports(s: &mut Session, _: &Value) -> CmdResult {
    use codecraft_lsp::lsp_types as lt;
    // Rust: sort the `use` groups here (rust-analyzer has no `source.organizeImports`), then ask
    // the server for its "Remove all unused imports" assist over the whole file.
    let is_rust = s.ws.active().is_some_and(|(_, d)| d.language == Language::Rust);
    let mut sorted = false;
    if is_rust
        && let Some(new_text) = s.ws.active().and_then(|(_, d)| sort_rust_imports(&d.doc.text()))
    {
        sorted = s.edit_active(EditGroup::Standalone, |doc, sels, _| crate::reformat::diff_edit(doc, &new_text, sels))?;
    }
    if let Err(reason) = s.lsp_ready() {
        if sorted {
            s.status = format!("Imports sorted; removing unused imports needs a language server ({reason})");
            return ok();
        }
        s.status = format!("Optimize Imports needs a language server ({reason})");
        return ok();
    }
    let Some((_, did)) = s.active_ids() else { return ok() };
    s.lsp_sync(did);
    let Some((path, range, revision)) = s.ws.doc(did).and_then(|d| {
        let path = d.path.clone()?;
        Some((path, crate::convert::lsp_range(&d.doc, 0..d.doc.len_chars()), d.doc.revision()))
    }) else {
        return Err(CommandError::Failed("the file is not saved on disk".into()));
    };
    let uri = codecraft_lsp::path_to_uri(&path).map_err(|e| CommandError::Failed(e.to_string()))?;
    let params = lt::CodeActionParams {
        text_document: lt::TextDocumentIdentifier { uri },
        range,
        context: lt::CodeActionContext {
            diagnostics: Vec::new(),
            only: if is_rust { None } else { Some(vec![lt::CodeActionKind::SOURCE_ORGANIZE_IMPORTS]) },
            trigger_kind: Some(lt::CodeActionTriggerKind::INVOKED),
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    s.lsp_send(
        |c| c.request::<lt::request::CodeActionRequest>(params),
        crate::lsp::Pending::OrganizeImports { doc: did, revision },
    )?;
    s.status = "Optimizing imports…".into();
    ok()
}

/// The language server's answer to Optimize Imports.
pub(crate) fn on_organize_imports(s: &mut Session, doc: codecraft_workspace::DocId, revision: u64, value: Value) {
    if s.ws.doc(doc).map(|d| d.doc.revision()) != Some(revision) {
        s.status = "Optimize Imports skipped: the file changed".into();
        return;
    }
    let actions: Vec<Value> = match value {
        Value::Array(v) => v,
        _ => Vec::new(),
    };
    let action = actions
        .iter()
        .find(|a| a.get("kind").and_then(Value::as_str).is_some_and(|k| k.starts_with("source.organizeImports")))
        .or_else(|| actions.iter().find(|a| is_remove_unused_imports(a)))
        .or_else(|| {
            // Servers that send bare actions without a kind (only organizeImports was asked for).
            actions.iter().find(|a| a.get("kind").is_none() && a.get("data").is_none() && a.get("edit").is_some())
        })
        .cloned();
    match action {
        Some(a) => match s.apply_code_action(&a) {
            Ok(()) => s.status = "Imports optimized".into(),
            Err(e) => s.status = e.to_string(),
        },
        None => s.status = "Optimize Imports: the language server has no import changes for this file".into(),
    }
}

/// rust-analyzer's `remove_unused_imports` assist ("Remove all unused imports"), recognized by its
/// resolve data id or its title.
fn is_remove_unused_imports(action: &Value) -> bool {
    let id = action.get("data").and_then(|d| d.get("id")).and_then(Value::as_str).unwrap_or("");
    let title = action.get("title").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
    id.starts_with("remove_unused_imports") || title.starts_with("remove all unused imports")
        || title == "remove unused imports"
}

/// Rank of a `use` path's first segment, then the path (rustfmt puts `self`, `super` and `crate`
/// before external crates).
fn use_sort_key(line: &str) -> (u8, String) {
    let t = line.trim();
    let after = t.split_once("use ").map(|(_, rest)| rest).unwrap_or(t).trim_start_matches("::");
    let first = after.split([':', ';', ' ', '{']).next().unwrap_or("");
    let rank = match first {
        "self" => 0,
        "super" => 1,
        "crate" => 2,
        _ => 3,
    };
    (rank, after.to_string())
}

/// A single-line `use` item (`use a::b;`, `pub(crate) use c::{d, e};`).
fn is_single_line_use(line: &str) -> bool {
    let t = line.trim();
    let body = t.strip_prefix("pub ").or_else(|| t.split_once(") ").filter(|(v, _)| v.starts_with("pub(")).map(|(_, r)| r));
    let body = body.unwrap_or(t);
    body.starts_with("use ") && t.ends_with(';') && t.matches('{').count() == t.matches('}').count()
        && !t.contains("//") && !t.contains("/*")
}

fn indent_of(line: &str) -> &str {
    line.get(..line.len() - line.trim_start().len()).unwrap_or("")
}

/// Sort each run of consecutive single-line `use` items (same indentation; blank lines, comments
/// and attributes end a run, as with rustfmt) and drop exact duplicates. `None` when nothing
/// changes.
pub(crate) fn sort_rust_imports(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while let Some(line) = lines.get(i) {
        if !is_single_line_use(line) {
            out.push((*line).to_string());
            i += 1;
            continue;
        }
        let indent = indent_of(line);
        let mut run: Vec<&str> = Vec::new();
        while let Some(l) = lines.get(i)
            && is_single_line_use(l)
            && indent_of(l) == indent
        {
            run.push(l);
            i += 1;
        }
        run.sort_by_cached_key(|l| use_sort_key(l));
        run.dedup_by(|a, b| a.trim() == b.trim());
        out.extend(run.into_iter().map(str::to_string));
    }
    let new = out.join("\n");
    (new != text).then_some(new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_workspace::DocId;
    use std::path::PathBuf;

    fn session(text: &str, lang: Language) -> (Session, DocId, tempfile::TempDir) {
        let mut s = Session::new();
        s.lsp_enabled = false;
        let dir = tempfile::tempdir().unwrap();
        s.editing.templates.set_path(dir.path().join("templates.json"));
        let did = s.ws.add_document(None, text);
        if let Some(d) = s.ws.doc_mut(did) {
            d.language = lang;
        }
        s.ws.open_editor(did).unwrap();
        (s, did, dir)
    }

    fn text(s: &Session) -> String {
        s.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
    }

    fn caret(s: &Session) -> usize {
        s.ws.active().map(|(e, _)| e.selections.primary().head).unwrap()
    }

    fn set_caret(s: &mut Session, at: usize) {
        s.execute("editor.setSelection", &json!({ "anchor": at })).unwrap();
    }

    fn typ(s: &mut Session, t: &str) {
        s.execute("editor.type", &json!({ "text": t })).unwrap();
    }

    #[test]
    fn tab_expands_and_walks_variables() {
        let (mut s, _, _d) = session("    \n", Language::Rust);
        set_caret(&mut s, 4);
        typ(&mut s, "for");
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(text(&s), "    for item in  {\n        \n    }\n");
        // The first variable is selected.
        let sel = s.ws.active().unwrap().0.selections.primary();
        assert_eq!((sel.start(), sel.end()), (8, 12));
        typ(&mut s, "x");
        s.execute("editor.tab", &Value::Null).unwrap();
        typ(&mut s, "v");
        assert_eq!(text(&s), "    for x in v {\n        \n    }\n");
        // Shift+Tab goes back to the first variable (its whole text selected).
        s.execute("editor.shiftTab", &Value::Null).unwrap();
        let sel = s.ws.active().unwrap().0.selections.primary();
        assert_eq!(s.ws.active().unwrap().1.doc.slice(sel.range()), "x");
        s.execute("navigate.navigateInFile.nextLiveTemplateParameter", &Value::Null).unwrap();
        s.execute("editor.tab", &Value::Null).unwrap();
        // After the last variable: the caret is at $END$ and the session is over.
        assert_eq!(caret(&s), "    for x in v {\n        ".chars().count());
        assert!(s.editing.session.is_none());
        // Tab now indents.
        s.execute("editor.tab", &Value::Null).unwrap();
        assert!(text(&s).contains("{\n            \n"), "{}", text(&s));
    }

    #[test]
    fn mirrored_variables_are_edited_together_and_undo_restores() {
        let (mut s, _, _d) = session("", Language::Rust);
        typ(&mut s, "pv");
        s.execute("editor.tab", &Value::Null).unwrap();
        typ(&mut s, "n");
        assert_eq!(text(&s), "println!(\"n = {:?}\", n);");
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(caret(&s), text(&s).chars().count());
        s.execute("edit.undo", &Value::Null).unwrap();
        s.execute("edit.undo", &Value::Null).unwrap();
        assert!(text(&s).len() <= 2, "{}", text(&s));
    }

    #[test]
    fn tab_without_template_indents_and_unknown_words_stay() {
        let (mut s, _, _d) = session("zzz", Language::Rust);
        set_caret(&mut s, 3);
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(text(&s), "zzz ");
        // Shift+Tab unindents the line.
        let (mut s, _, _d) = session("    a", Language::Rust);
        set_caret(&mut s, 5);
        s.execute("editor.shiftTab", &Value::Null).unwrap();
        assert_eq!(text(&s), "a");
    }

    #[test]
    fn insert_live_template_lists_and_inserts() {
        let (mut s, _, _d) = session("", Language::Rust);
        typ(&mut s, "ma");
        let r = s.execute("code.insertLiveTemplate", &Value::Null).unwrap();
        let items: Vec<String> = serde_json::from_value(r["items"].clone()).unwrap();
        assert!(items.contains(&"main".to_string()) && items.contains(&"matchop".to_string()), "{items:?}");
        assert!(items.iter().all(|i| i.starts_with("ma")));
        let idx = s.editing.chooser.as_ref().unwrap().visible().iter().position(|i| {
            s.editing.chooser.as_ref().unwrap().items[*i].key == "main"
        });
        s.execute("editor.chooser.accept", &json!({ "index": idx.unwrap() })).unwrap();
        assert_eq!(text(&s), "fn main() {\n    \n}");
        assert!(s.editing.chooser.is_none());
        // Direct insertion by abbreviation, Python templates by language.
        let (mut s, _, _d) = session("", Language::Python);
        s.execute("code.insertLiveTemplate", &json!({ "abbreviation": "def" })).unwrap();
        assert_eq!(text(&s), "def ():\n    pass");
        assert!(s.execute("code.insertLiveTemplate", &json!({ "abbreviation": "nope" })).is_err());
    }

    #[test]
    fn chooser_query_move_and_close() {
        let (mut s, _, _d) = session("", Language::TypeScript);
        s.execute("code.insertLiveTemplate", &Value::Null).unwrap();
        s.execute("editor.chooser.setQuery", &json!({ "query": "console" })).unwrap();
        assert_eq!(s.editing.chooser.as_ref().unwrap().visible().len(), 1);
        s.execute("editor.chooser.move", &json!({ "delta": 5 })).unwrap();
        s.execute("editor.chooser.accept", &Value::Null).unwrap();
        assert_eq!(text(&s), "console.log();");
        s.execute("code.insertLiveTemplate", &Value::Null).unwrap();
        s.execute("editor.chooser.close", &Value::Null).unwrap();
        assert!(s.editing.chooser.is_none());
        assert!(s.execute("editor.chooser.accept", &Value::Null).is_err(), "disabled without a chooser");
    }

    #[test]
    fn surround_with_lines_and_expression() {
        let (mut s, _, _d) = session("fn f() {\n    a();\n    b();\n}\n", Language::Rust);
        s.execute("editor.setSelection", &json!({ "anchor": 9, "head": 27 })).unwrap();
        let r = s.execute("code.surroundWith", &Value::Null).unwrap();
        assert!(r["items"].as_array().unwrap().iter().any(|i| i == "sif"));
        let idx = {
            let c = s.editing.chooser.as_ref().unwrap();
            c.visible().iter().position(|i| c.items[*i].key == "sif").unwrap()
        };
        s.execute("editor.chooser.accept", &json!({ "index": idx })).unwrap();
        assert_eq!(text(&s), "fn f() {\n    if  {\n        a();\n        b();\n    }\n}\n");
        typ(&mut s, "ok");
        assert!(text(&s).contains("if ok {"));
        // An expression inside a line.
        let (mut s, _, _d) = session("let v = compute();", Language::Rust);
        s.execute("editor.setSelection", &json!({ "anchor": 8, "head": 17 })).unwrap();
        s.execute("code.surroundWith", &json!({ "abbreviation": "ssome" })).unwrap();
        assert_eq!(text(&s), "let v = Some(compute());");
        // No selection: the current line.
        let (mut s, _, _d) = session("x();", Language::Rust);
        s.execute("code.surroundWith", &json!({ "abbreviation": "sloop" })).unwrap();
        assert_eq!(text(&s), "loop {\n    x();\n}");
        assert!(s.execute("code.surroundWith", &json!({ "abbreviation": "fn" })).is_err());
    }

    #[test]
    fn save_as_live_template_persists_to_the_given_file() {
        let (mut s, _, dir) = session("let a = 1;", Language::Rust);
        assert!(s.execute("code.saveAsLiveTemplate", &Value::Null).is_err(), "needs a selection");
        s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 10 })).unwrap();
        s.execute("code.saveAsLiveTemplate", &Value::Null).unwrap();
        assert_eq!(s.editing.save_dialog.as_ref().unwrap().text, "let a = 1;");
        assert!(s.execute("code.saveAsLiveTemplate", &json!({ "abbreviation": "bad word" })).is_err());
        assert!(s.editing.save_dialog.as_ref().unwrap().error.is_some());
        let r = s
            .execute(
                "code.saveAsLiveTemplate",
                &json!({ "abbreviation": "leta", "description": "let a", "text": "let $N$ = $END$;" }),
            )
            .unwrap();
        assert!(s.editing.save_dialog.is_none());
        let path = PathBuf::from(r["path"].as_str().unwrap());
        assert!(path.starts_with(dir.path()));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("leta"));
        // The new template expands with Tab.
        s.execute("editor.setSelection", &json!({ "anchor": 10 })).unwrap();
        typ(&mut s, "\nleta");
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(text(&s), "let a = 1;\nlet  = ;");
    }

    #[test]
    fn unwrap_remove_applies_choices() {
        let src = "fn f() {\n    if x {\n        a();\n    }\n}\n";
        let (mut s, _, _d) = session(src, Language::Rust);
        set_caret(&mut s, src.find("a()").unwrap());
        let r = s.execute("code.unwrapRemove", &Value::Null).unwrap();
        assert_eq!(r["options"][0], "Unwrap 'if...'");
        s.execute("editor.chooser.accept", &Value::Null).unwrap();
        assert_eq!(text(&s), "fn f() {\n    a();\n}\n");
        set_caret(&mut s, 0);
        let r = s.execute("code.unwrapRemove", &Value::Null).unwrap();
        assert_eq!(r["options"].as_array().unwrap().len(), 0);
        assert!(s.execute("code.unwrapRemove", &json!({ "index": 3 })).is_ok());
    }

    #[test]
    fn column_mode_and_box_select() {
        let (mut s, _, _d) = session("abcdef\nghijkl\nmnopqr\n", Language::PlainText);
        let r = s
            .execute("editor.boxSelect", &json!({ "anchor": { "line": 0, "column": 1 }, "head": { "line": 2, "column": 3 } }))
            .unwrap();
        assert_eq!(r["carets"], 3);
        typ(&mut s, "X");
        assert_eq!(text(&s), "aXdef\ngXjkl\nmXpqr\n");
        // Column mode: Shift+Down/Right grow a box from the caret.
        s.execute("edit.columnSelectionMode", &Value::Null).unwrap();
        set_caret(&mut s, 0);
        s.execute("editor.downWithSelection", &Value::Null).unwrap();
        s.execute("editor.downWithSelection", &Value::Null).unwrap();
        s.execute("editor.rightWithSelection", &Value::Null).unwrap();
        let sels = s.ws.active().unwrap().0.selections.clone();
        assert_eq!(sels.len(), 3);
        assert!(sels.ranges().iter().all(|r| r.end() - r.start() == 1));
        let r = s.execute("edit.columnSelectionMode", &Value::Null).unwrap();
        assert_eq!(r["on"], false);
        assert!(s.execute("editor.boxSelect", &json!({ "anchor": 1 })).is_err());
    }

    #[test]
    fn soft_wrap_moves_by_visual_row() {
        let (mut s, _, _d) = session("hello world again\nx", Language::PlainText);
        let eid = s.ws.active_editor().unwrap();
        s.ws.settings.soft_wrap = true;
        s.editing.set_wrap_width(eid, 12);
        set_caret(&mut s, 2);
        s.execute("editor.down", &Value::Null).unwrap();
        assert_eq!(caret(&s), 14);
        // Without wrap: plain line motion.
        s.ws.settings.soft_wrap = false;
        s.execute("editor.down", &Value::Null).unwrap();
        assert_eq!(caret(&s), 19);
    }

    #[test]
    fn edit_locations_back_and_forth() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "fn a() {}\n\n\n\n\nfn a2() {}\n").unwrap();
        std::fs::write(&b, "fn b() {}\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.open_file(&a).unwrap();
        typ(&mut s, "x");
        set_caret(&mut s, 15);
        typ(&mut s, "y");
        s.open_file(&b).unwrap();
        typ(&mut s, "z");
        s.execute("navigate.lastEditLocation", &Value::Null).unwrap();
        assert!(s.ws.active().unwrap().1.path.as_ref().unwrap().ends_with("a.rs"));
        assert_eq!(caret(&s), 16);
        s.execute("navigate.lastEditLocation", &Value::Null).unwrap();
        assert_eq!(caret(&s), 1);
        s.execute("navigate.nextEditLocation", &Value::Null).unwrap();
        assert_eq!(caret(&s), 16);
        s.execute("navigate.nextEditLocation", &Value::Null).unwrap();
        assert!(s.ws.active().unwrap().1.path.as_ref().unwrap().ends_with("b.rs"));
        s.execute("navigate.nextEditLocation", &Value::Null).unwrap();
        assert_eq!(s.status, "No next edit location");
    }

    #[test]
    fn navigation_bar_segments_and_navigation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let main = dir.path().join("src/main.rs");
        std::fs::write(&main, "mod m {\n    fn inner() {\n        x();\n    }\n    fn other() {}\n}\nfn main() {}\n")
            .unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.open_file(&main).unwrap();
        let text = std::fs::read_to_string(&main).unwrap();
        set_caret(&mut s, text.find("x()").unwrap());
        let r = s.execute("navigate.jumpToNavigationBar", &Value::Null).unwrap();
        let segs: Vec<String> = serde_json::from_value(r["segments"].clone()).unwrap();
        assert_eq!(&segs[segs.len() - 4..], &["src", "main.rs", "m", "inner()"]);
        // The file element lists its top-level items.
        let bar = s.editing.navbar.as_ref().unwrap();
        assert_eq!(bar.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), vec!["m", "main()"]);
        // The `src` directory lists its files.
        let r = s.execute("navbar.select", &json!({ "delta": -1 })).unwrap();
        assert_eq!(r["items"], json!(["lib.rs", "main.rs"]));
        // The leaf symbol shows its siblings; accepting one navigates there.
        let last = segs.len() - 1;
        s.execute("navbar.select", &json!({ "index": last })).unwrap();
        let items: Vec<String> =
            s.editing.navbar.as_ref().unwrap().items.iter().map(|i| i.label.clone()).collect();
        assert_eq!(items, vec!["inner()", "other()"]);
        s.execute("navbar.moveItem", &json!({ "delta": 1 })).unwrap();
        s.execute("navbar.accept", &Value::Null).unwrap();
        assert_eq!(caret(&s), text.find("other").unwrap());
        s.execute("navbar.close", &Value::Null).unwrap();
        assert!(s.editing.navbar.is_none());
    }

    #[test]
    fn complete_statement_auto_indent_and_move_element() {
        let (mut s, _, _d) = session("fn f() {\n    let x = foo(1\n}\n", Language::Rust);
        set_caret(&mut s, 24);
        s.execute("code.codeCompletion.completeCurrentStatement", &Value::Null).unwrap();
        assert_eq!(text(&s), "fn f() {\n    let x = foo(1);\n}\n");
        assert_eq!(caret(&s), 28);

        let (mut s, _, _d) = session("fn f() {\nlet a = 1;\n      if x {\ny();\n}\n}\n", Language::Rust);
        s.execute("edit.selectAll", &Value::Null).unwrap();
        s.execute("code.autoIndentLines", &Value::Null).unwrap();
        assert_eq!(text(&s), "fn f() {\n    let a = 1;\n    if x {\n        y();\n    }\n}\n");

        let (mut s, _, _d) = session("call(a, bb, c)", Language::Rust);
        set_caret(&mut s, 8);
        s.execute("code.moveElementRight", &Value::Null).unwrap();
        assert_eq!(text(&s), "call(a, c, bb)");
        assert_eq!(caret(&s), 11);
        s.execute("code.moveElementLeft", &Value::Null).unwrap();
        s.execute("code.moveElementLeft", &Value::Null).unwrap();
        assert_eq!(text(&s), "call(bb, a, c)");
        let r = s.execute("code.moveElementLeft", &Value::Null).unwrap();
        assert_eq!(r["moved"], false);
    }

    #[test]
    fn cyclic_expand_word_cycles_and_returns_to_prefix() {
        let (mut s, _, _d) = session("alpha alphabet\nal", Language::PlainText);
        set_caret(&mut s, 17);
        let r = s.execute("code.codeCompletion.cyclicExpandWord", &Value::Null).unwrap();
        assert_eq!(r["word"], "alphabet");
        let r = s.execute("code.codeCompletion.cyclicExpandWord", &Value::Null).unwrap();
        assert_eq!(r["word"], "alpha");
        let r = s.execute("code.codeCompletion.cyclicExpandWord", &Value::Null).unwrap();
        assert_eq!(r["word"], "al");
        assert_eq!(text(&s), "alpha alphabet\nal");
        let r = s.execute("code.codeCompletion.cyclicExpandWordBackward", &Value::Null).unwrap();
        assert_eq!(r["word"], "alpha");
        // Typing ends the cycle; a fresh expansion starts from the new prefix.
        typ(&mut s, "b");
        let r = s.execute("code.codeCompletion.cyclicExpandWord", &Value::Null).unwrap();
        assert_eq!(r["word"], "alphabet");
    }

    #[test]
    fn find_scroll_toggle_selects_the_current_match() {
        let (mut s, _, _d) = session("one two three two", Language::PlainText);
        assert!(s.ws.settings.find_scroll_to_results);
        s.execute("edit.find.setQuery", &json!({ "pattern": "two" })).unwrap();
        let sel = s.ws.active().unwrap().0.selections.primary();
        assert_eq!((sel.start(), sel.end()), (4, 7));
        let r = s.execute("edit.find.scrollToResultsDuringTyping", &Value::Null).unwrap();
        assert_eq!(r["on"], false);
        set_caret(&mut s, 0);
        s.execute("edit.find.setQuery", &json!({ "pattern": "three" })).unwrap();
        assert_eq!(caret(&s), 0);
    }

    #[test]
    fn optimize_imports_without_a_server_explains() {
        let (mut s, _, _d) = session("use a;", Language::Rust);
        s.execute("code.optimizeImports", &Value::Null).unwrap();
        assert!(s.status.contains("language server"), "{}", s.status);
    }

    #[test]
    fn rust_imports_sort_per_group_and_dedupe() {
        let text = "use std::fmt;\nuse crate::b;\nuse anyhow::Result;\nuse std::fmt;\n\nuse zed::Z;\nuse self::a;\n#[cfg(test)]\nuse x::y;\nfn main() {}\n";
        let sorted = sort_rust_imports(text).unwrap();
        assert_eq!(
            sorted,
            "use crate::b;\nuse anyhow::Result;\nuse std::fmt;\n\nuse self::a;\nuse zed::Z;\n#[cfg(test)]\nuse x::y;\nfn main() {}\n"
        );
        assert_eq!(sort_rust_imports(&sorted), None);
        // Multi-line trees, comments and different indentation are left alone.
        let keep = "use b::{\n    c,\n};\nuse a::x; // why\n    use z::q;\nuse y::w;\n";
        assert_eq!(sort_rust_imports(keep), None);
        let vis = "pub use b::B;\npub(crate) use a::{A, AA};\n";
        assert_eq!(sort_rust_imports(vis).unwrap(), "pub(crate) use a::{A, AA};\npub use b::B;\n");
        assert_eq!(sort_rust_imports(""), None);
        assert_eq!(sort_rust_imports("use é::ü;\nuse ä;"), Some("use ä;\nuse é::ü;".into()));
    }

    #[test]
    fn optimize_imports_sorts_rust_without_a_server() {
        let (mut s, _, _d) = session("use b::B;\nuse a::A;\n", Language::Rust);
        s.execute("code.optimizeImports", &Value::Null).unwrap();
        assert_eq!(text(&s), "use a::A;\nuse b::B;\n");
        assert!(s.status.contains("sorted"), "{}", s.status);
        s.execute("edit.undo", &Value::Null).unwrap();
        assert_eq!(text(&s), "use b::B;\nuse a::A;\n");
    }

    #[test]
    fn remove_unused_imports_action_is_recognized() {
        assert!(is_remove_unused_imports(&json!({ "title": "Remove all unused imports", "data": { "id": "remove_unused_imports:QuickFix:0" } })));
        assert!(is_remove_unused_imports(&json!({ "title": "Remove unused imports" })));
        assert!(!is_remove_unused_imports(&json!({ "title": "Remove unused import", "data": { "id": "remove_unused_param:QuickFix:0" } })));
    }

    #[test]
    fn hostile_params_never_panic() {
        let (mut s, _, _d) = session("x", Language::Rust);
        for (id, p) in [
            ("editor.boxSelect", json!({ "anchor": { "line": 99999999, "column": 99999999 }, "head": { "line": 0, "column": 0 } })),
            ("code.unwrapRemove", json!({ "index": 99 })),
            ("editor.chooser.move", json!({ "delta": -99 })),
            ("navbar.select", json!({ "index": 99 })),
            ("code.insertLiveTemplate", json!({ "abbreviation": "" })),
            ("code.surroundWith", json!({ "abbreviation": 5 })),
            ("navigate.jumpToNavigationBar", json!({})),
            ("navigate.nextEditLocation", json!({})),
            ("navigate.lastEditLocation", json!({})),
        ] {
            let _ = s.execute(id, &p);
        }
        assert_eq!(text(&s), "x");
    }
}
