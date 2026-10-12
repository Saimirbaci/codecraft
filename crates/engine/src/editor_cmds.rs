//! Editor actions without a menu entry: typing, caret motion, mouse, completion and popups.
//! Shortcuts follow RustRover's macOS keymap.

use serde_json::{Value, json};

use codecraft_text::movement::{self, Motion};
use codecraft_text::{Change, ChangeSet, Edit, EditGroup, Selection, SelectionSet, ops};

use crate::commands::{CmdResult, CommandSpec, Context, has_editor, ok, spec};
use crate::params::{bool_or, opt_usize, req_str, req_usize};
use crate::{CommandError, Popup, Session};

const fn motion(
    id: &'static str,
    label: &'static str,
    shortcut: &'static str,
    run: crate::commands::Run,
) -> CommandSpec {
    spec(id, label, Some(shortcut), "", Context::Editor, has_editor, run)
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let mut v = vec![
        spec("editor.type", "Type", None, "{text: string}", Context::Editor, has_editor, type_text),
        spec("editor.enter", "Enter", Some("Enter"), "", Context::Editor, has_editor, enter),
        spec("editor.startNewLine", "Start New Line", Some("Shift+Enter"), "", Context::Editor, has_editor, |s, _| {
            s.edit_active(EditGroup::Standalone, |d, sel, _| ops::start_new_line(d, sel))?;
            ok()
        }),
        spec("editor.backspace", "Backspace", Some("Backspace"), "", Context::Editor, has_editor, |s, _| {
            s.edit_active(EditGroup::Typing, ops::backspace)?;
            ok()
        }),
        spec("editor.deleteForward", "Delete", Some("Delete"), "", Context::Editor, has_editor, |s, _| {
            s.edit_active(EditGroup::Typing, |d, sel, _| ops::delete_forward(d, sel))?;
            ok()
        }),
        spec(
            "editor.deleteWordBackward",
            "Delete to Word Start",
            Some("Alt+Backspace"),
            "",
            Context::Editor,
            has_editor,
            |s, _| {
                s.edit_active(EditGroup::Standalone, |d, sel, _| ops::delete_word_backward(d, sel))?;
                ok()
            },
        ),
        spec(
            "editor.deleteWordForward",
            "Delete to Word End",
            Some("Alt+Delete"),
            "",
            Context::Editor,
            has_editor,
            |s, _| {
                s.edit_active(EditGroup::Standalone, |d, sel, _| ops::delete_word_forward(d, sel))?;
                ok()
            },
        ),
        spec("editor.deleteLine", "Delete Line", Some("Cmd+Backspace"), "", Context::Editor, has_editor, |s, _| {
            s.edit_active(EditGroup::Standalone, |d, sel, _| ops::delete_line(d, sel))?;
            ok()
        }),
        spec("editor.escape", "Escape", Some("Escape"), "", Context::Editor, has_editor, escape),
        spec(
            "editor.click",
            "Place Caret",
            None,
            "{offset: int, extend?: bool, addCaret?: bool, clicks?: 1|2|3}",
            Context::Editor,
            has_editor,
            click,
        ),
        spec("editor.drag", "Drag Selection", None, "{offset: int}", Context::Editor, has_editor, drag),
        spec(
            "editor.setSelection",
            "Set Selection",
            None,
            "{anchor: int, head?: int} — char offsets",
            Context::Editor,
            has_editor,
            set_selection,
        ),
        spec("editor.cloneCaretAbove", "Clone Caret Above", None, "", Context::Editor, has_editor, |s, _| {
            s.select_active(|d, sel| movement::add_caret_vertical(d, sel, false));
            ok()
        }),
        spec("editor.cloneCaretBelow", "Clone Caret Below", None, "", Context::Editor, has_editor, |s, _| {
            s.select_active(|d, sel| movement::add_caret_vertical(d, sel, true));
            ok()
        }),
        spec(
            "editor.completion.accept",
            "Choose Lookup Item",
            None,
            "{index?: int} — accept the selected (or given) completion",
            Context::Editor,
            has_editor,
            accept_completion,
        ),
        spec(
            "editor.completion.select",
            "Move Lookup Selection",
            None,
            "{delta: int}",
            Context::Editor,
            has_editor,
            select_completion,
        ),
        spec(
            "editor.move",
            "Move Caret",
            None,
            "{motion: left|right|up|down|wordLeft|wordRight|lineStart|lineEnd|docStart|docEnd|pageUp|pageDown, extend?: bool, lines?: int}",
            Context::Editor,
            has_editor,
            move_generic,
        ),
    ];
    v.extend(motion_specs());
    v
}

fn motion_specs() -> Vec<CommandSpec> {
    vec![
        motion("editor.left", "Left", "Left", |s, _| mv(s, Motion::Left, false)),
        motion("editor.right", "Right", "Right", |s, _| mv(s, Motion::Right, false)),
        motion("editor.up", "Up", "Up", |s, _| mv(s, Motion::Up, false)),
        motion("editor.down", "Down", "Down", |s, _| mv(s, Motion::Down, false)),
        motion("editor.leftWithSelection", "Left with Selection", "Shift+Left", |s, _| mv(s, Motion::Left, true)),
        motion("editor.rightWithSelection", "Right with Selection", "Shift+Right", |s, _| mv(s, Motion::Right, true)),
        motion("editor.upWithSelection", "Up with Selection", "Shift+Up", |s, _| mv(s, Motion::Up, true)),
        motion("editor.downWithSelection", "Down with Selection", "Shift+Down", |s, _| mv(s, Motion::Down, true)),
        motion("editor.previousWord", "Move Caret to Previous Word", "Alt+Left", |s, _| mv(s, Motion::WordLeft, false)),
        motion("editor.nextWord", "Move Caret to Next Word", "Alt+Right", |s, _| mv(s, Motion::WordRight, false)),
        motion("editor.previousWordWithSelection", "Previous Word with Selection", "Shift+Alt+Left", |s, _| {
            mv(s, Motion::WordLeft, true)
        }),
        motion("editor.nextWordWithSelection", "Next Word with Selection", "Shift+Alt+Right", |s, _| {
            mv(s, Motion::WordRight, true)
        }),
        motion("editor.lineStart", "Move Caret to Line Start", "Cmd+Left", |s, _| mv(s, Motion::LineStart, false)),
        motion("editor.lineEnd", "Move Caret to Line End", "Cmd+Right", |s, _| mv(s, Motion::LineEnd, false)),
        motion("editor.lineStartWithSelection", "Line Start with Selection", "Shift+Cmd+Left", |s, _| {
            mv(s, Motion::LineStart, true)
        }),
        motion("editor.lineEndWithSelection", "Line End with Selection", "Shift+Cmd+Right", |s, _| {
            mv(s, Motion::LineEnd, true)
        }),
        motion("editor.home", "Home", "Home", |s, _| mv(s, Motion::LineStart, false)),
        motion("editor.end", "End", "End", |s, _| mv(s, Motion::LineEnd, false)),
        motion("editor.homeWithSelection", "Home with Selection", "Shift+Home", |s, _| mv(s, Motion::LineStart, true)),
        motion("editor.endWithSelection", "End with Selection", "Shift+End", |s, _| mv(s, Motion::LineEnd, true)),
        motion("editor.textStart", "Move Caret to Text Start", "Cmd+Home", |s, _| mv(s, Motion::DocStart, false)),
        motion("editor.textEnd", "Move Caret to Text End", "Cmd+End", |s, _| mv(s, Motion::DocEnd, false)),
        motion("editor.textStartWithSelection", "Text Start with Selection", "Shift+Cmd+Home", |s, _| {
            mv(s, Motion::DocStart, true)
        }),
        motion("editor.textEndWithSelection", "Text End with Selection", "Shift+Cmd+End", |s, _| {
            mv(s, Motion::DocEnd, true)
        }),
        motion("editor.pageUp", "Page Up", "PageUp", |s, _| mv(s, Motion::PageUp(page(s)), false)),
        motion("editor.pageDown", "Page Down", "PageDown", |s, _| mv(s, Motion::PageDown(page(s)), false)),
        motion("editor.pageUpWithSelection", "Page Up with Selection", "Shift+PageUp", |s, _| {
            let n = page(s);
            mv(s, Motion::PageUp(n), true)
        }),
        motion("editor.pageDownWithSelection", "Page Down with Selection", "Shift+PageDown", |s, _| {
            let n = page(s);
            mv(s, Motion::PageDown(n), true)
        }),
    ]
}

/// Lines per page (the UI reports its visible line count through settings-independent state;
/// a reasonable default for headless use).
fn page(s: &Session) -> usize {
    s.page_lines.max(1)
}

fn mv(s: &mut Session, motion: Motion, extend: bool) -> CmdResult {
    // Up/Down move the completion selection while the lookup is open.
    if let Some(Popup::Completion(c)) = &mut s.popup
        && !extend
        && matches!(motion, Motion::Up | Motion::Down | Motion::PageUp(_) | Motion::PageDown(_))
        && !c.visible.is_empty()
    {
        let n = c.visible.len();
        c.selected = match motion {
            Motion::Up => (c.selected + n - 1) % n,
            Motion::Down => (c.selected + 1) % n,
            Motion::PageUp(k) => c.selected.saturating_sub(k),
            _ => (c.selected + 10).min(n - 1),
        };
        return ok();
    }
    s.select_active(|d, sel| movement::move_selections(d, sel, motion, extend));
    s.refresh_completion_filter();
    ok()
}

fn move_generic(s: &mut Session, p: &Value) -> CmdResult {
    let lines = opt_usize(p, "lines")?.unwrap_or(s.page_lines.max(1));
    let m = match req_str(p, "motion")? {
        "left" => Motion::Left,
        "right" => Motion::Right,
        "up" => Motion::Up,
        "down" => Motion::Down,
        "wordLeft" => Motion::WordLeft,
        "wordRight" => Motion::WordRight,
        "lineStart" => Motion::LineStart,
        "lineEnd" => Motion::LineEnd,
        "docStart" => Motion::DocStart,
        "docEnd" => Motion::DocEnd,
        "pageUp" => Motion::PageUp(lines),
        "pageDown" => Motion::PageDown(lines),
        other => return Err(CommandError::BadParams(format!("unknown motion `{other}`"))),
    };
    mv(s, m, bool_or(p, "extend", false))
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn type_text(s: &mut Session, p: &Value) -> CmdResult {
    let text = req_str(p, "text")?.to_string();
    if text.is_empty() {
        return ok();
    }
    let mut chars = text.chars();
    let single = match (chars.next(), chars.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    };
    // Completion lookup: typing a non-identifier char closes it (RustRover inserts the selected
    // item only on Enter/Tab, or on `.`/`(` for some items; we keep the simple behaviour).
    s.edit_active(EditGroup::Typing, |d, sel, cfg| match single {
        Some(c) => ops::type_char(d, sel, c, cfg),
        None => ops::insert_text(d, sel, &text),
    })?;
    if let Some(c) = single {
        auto_popup(s, c);
    }
    ok()
}

/// Show completion automatically while typing an identifier or after `.` / `::`.
fn auto_popup(s: &mut Session, typed: char) {
    if s.lsp_ready().is_err() {
        return;
    }
    if matches!(s.popup, Some(Popup::Completion(_))) {
        return;
    }
    let Some((e, d)) = s.ws.active() else { return };
    if e.selections.len() != 1 || d.language != codecraft_syntax::Language::Rust {
        return;
    }
    let caret = e.selections.primary().head;
    let trigger = if is_ident_char(typed) {
        // Start of a new identifier only (not in the middle of numbers, strings are not checked).
        let before = caret.checked_sub(2).and_then(|i| d.doc.char_at(i));
        !typed.is_ascii_digit() && !before.is_some_and(is_ident_char)
    } else if typed == '.' {
        caret
            .checked_sub(2)
            .and_then(|i| d.doc.char_at(i))
            .is_some_and(|c| is_ident_char(c) || c == ')' || c == ']' || c == '?')
    } else if typed == ':' {
        caret.checked_sub(2).and_then(|i| d.doc.char_at(i)) == Some(':')
    } else {
        false
    };
    if trigger {
        let anchor = if is_ident_char(typed) { caret.saturating_sub(1) } else { caret };
        let _ = s.lsp_completion(anchor);
    }
}

fn enter(s: &mut Session, _: &Value) -> CmdResult {
    if let Some(Popup::Completion(c)) = &s.popup
        && !c.visible.is_empty()
    {
        return accept_completion(s, &Value::Null);
    }
    s.edit_active(EditGroup::Standalone, ops::newline)?;
    ok()
}

fn escape(s: &mut Session, _: &Value) -> CmdResult {
    if s.popup.is_some() {
        s.popup = None;
        return ok();
    }
    if s.find_bar.is_some() {
        s.find_bar = None;
        return ok();
    }
    s.select_active(|_, sel| {
        if sel.len() > 1 {
            sel.collapse_to_primary()
        } else {
            let p = sel.primary();
            SelectionSet::caret(p.head)
        }
    });
    ok()
}

fn click(s: &mut Session, p: &Value) -> CmdResult {
    let offset = req_usize(p, "offset")?;
    let extend = bool_or(p, "extend", false);
    let add = bool_or(p, "addCaret", false);
    let clicks = opt_usize(p, "clicks")?.unwrap_or(1);
    if !matches!(s.popup, Some(Popup::Rename { .. }) | Some(Popup::FindInFiles { .. })) {
        s.popup = None;
    }
    s.select_active(|d, sel| {
        let offset = offset.min(d.len_chars());
        match clicks {
            2 => SelectionSet::single(movement::select_word_at(d, offset)),
            n if n >= 3 => SelectionSet::single(movement::select_line(d, d.char_to_line(offset))),
            _ if extend => {
                let p = sel.primary();
                SelectionSet::single(Selection::new(p.anchor, offset))
            }
            _ if add => {
                let mut sel = sel.clone();
                sel.push(Selection::caret(offset));
                sel
            }
            _ => SelectionSet::caret(offset),
        }
    });
    ok()
}

fn drag(s: &mut Session, p: &Value) -> CmdResult {
    let offset = req_usize(p, "offset")?;
    s.select_active(|d, sel| {
        let p = sel.primary();
        SelectionSet::single(Selection::new(p.anchor, offset.min(d.len_chars())))
    });
    ok()
}

fn set_selection(s: &mut Session, p: &Value) -> CmdResult {
    let anchor = req_usize(p, "anchor")?;
    let head = opt_usize(p, "head")?.unwrap_or(anchor);
    s.select_active(|_, _| SelectionSet::single(Selection::new(anchor, head)));
    ok()
}

fn select_completion(s: &mut Session, p: &Value) -> CmdResult {
    let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1);
    if let Some(Popup::Completion(c)) = &mut s.popup
        && !c.visible.is_empty()
    {
        let n = c.visible.len() as i64;
        c.selected = usize::try_from((c.selected as i64 + delta).rem_euclid(n)).unwrap_or(0);
    }
    ok()
}

fn accept_completion(s: &mut Session, p: &Value) -> CmdResult {
    let Some(Popup::Completion(c)) = s.popup.take() else { return ok() };
    let idx = opt_usize(p, "index")?.unwrap_or(c.selected);
    let Some(item) = c.visible.get(idx).and_then(|i| c.items.get(*i)).cloned() else { return ok() };
    let anchor = item.replace_start.unwrap_or(c.anchor);
    s.edit_active(EditGroup::Standalone, |d, sel, _| {
        let caret = sel.primary().head;
        let start = anchor.min(caret);
        // Replace the rest of the identifier after the caret too (RustRover's Tab behaviour).
        let mut end = caret;
        while d.char_at(end).is_some_and(is_ident_char) {
            end += 1;
        }
        let mut changes = vec![Change::new(start, end, item.insert_text.clone())];
        for (r, text) in &item.additional_edits {
            if r.end <= start || r.start >= end {
                changes.push(Change::new(r.start, r.end, text.clone()));
            }
        }
        match ChangeSet::new(changes) {
            Ok(cs) => {
                let inserted_end = cs.map_pos(end, codecraft_text::Assoc::After);
                // The snippet's final cursor (`name($0)` puts the caret inside the parentheses).
                let len = item.insert_text.chars().count();
                let caret = match item.cursor {
                    Some(c) if c <= len => inserted_end.saturating_sub(len - c),
                    _ => inserted_end,
                };
                Edit { changes: cs, selections: SelectionSet::caret(caret) }
            }
            Err(_) => Edit::none(sel.clone()),
        }
    })?;
    Ok(json!({ "inserted": item.label }))
}
