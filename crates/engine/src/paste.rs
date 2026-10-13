//! Edit › Paste: Paste from History… (a ring of the texts copied, cut or pasted from outside,
//! most recent first, offered in a chooser at the caret) and Paste as Plain Text. Edit › Paste ›
//! Paste itself is `edit.paste.paste` in `edit_cmds.rs`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_text::{EditGroup, ops};

use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, has_editor, ok, spec};
use crate::editing::{Chooser, ChooserItem, ChooserKind};
use crate::params::{opt_str, opt_usize};
use crate::{Effect, Session};

/// Entries kept (RustRover's default is 100).
pub const HISTORY_LIMIT: usize = 100;
/// Texts longer than this (chars) are not remembered.
pub const ENTRY_LIMIT_CHARS: usize = 1_000_000;
/// Chars of an entry shown in the chooser.
const PREVIEW_CHARS: usize = 80;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipEntry {
    pub text: String,
    /// Copied as whole lines (a copy without a selection): pasting inserts above the caret line.
    pub whole_line: bool,
}

/// Clipboard history, most recent first.
#[derive(Clone, Debug, Default)]
pub struct ClipHistory {
    pub entries: Vec<ClipEntry>,
}

impl ClipHistory {
    /// Remember `text` as the most recent entry (an equal entry moves to the front).
    pub fn push(&mut self, text: &str, whole_line: bool) {
        if text.is_empty() || text.chars().nth(ENTRY_LIMIT_CHARS).is_some() {
            return;
        }
        self.entries.retain(|e| e.text != text);
        self.entries.insert(0, ClipEntry { text: text.to_string(), whole_line });
        self.entries.truncate(HISTORY_LIMIT);
    }
}

/// The chooser line for an entry: its first non-blank line, shortened.
pub fn preview(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let mut out: String = line.chars().take(PREVIEW_CHARS).collect();
    if line.chars().nth(PREVIEW_CHARS).is_some() {
        out.push('…');
    }
    out
}

fn detail(text: &str) -> String {
    match text.lines().count() {
        0 | 1 => String::new(),
        n => format!("{n} lines"),
    }
}

/// Record a copy, cut or outside paste.
pub(crate) fn record(s: &mut Session, text: &str, whole_line: bool) {
    s.clip_history.push(text, whole_line);
}

fn history_json(s: &Session) -> Value {
    let items: Vec<Value> = s
        .clip_history
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| json!({ "index": i, "preview": preview(&e.text), "lines": e.text.lines().count().max(1), "text": e.text }))
        .collect();
    json!({ "entries": items })
}

/// Paste history entry `index` at the carets and make it the clipboard's content.
pub(crate) fn paste_entry(s: &mut Session, index: usize) -> CmdResult {
    let entry = s
        .clip_history
        .entries
        .get(index)
        .cloned()
        .ok_or_else(|| CommandError::BadParams(format!("no clipboard history entry {index}")))?;
    s.edit_active(EditGroup::Standalone, |d, sel, _| ops::paste(d, sel, &entry.text, entry.whole_line))?;
    s.clip_history.push(&entry.text, entry.whole_line);
    s.clipboard = Some((entry.text.clone(), entry.whole_line));
    s.effects.push(Effect::CopyToClipboard(entry.text));
    ok()
}

fn paste_from_history(s: &mut Session, p: &Value) -> CmdResult {
    if let Some(i) = opt_usize(p, "index")? {
        return paste_entry(s, i);
    }
    let Some(editor) = s.ws.active_editor() else { return ok() };
    if s.clip_history.entries.is_empty() {
        s.status = "The clipboard history is empty".into();
        return Ok(history_json(s));
    }
    let items = s
        .clip_history
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| ChooserItem { label: preview(&e.text), detail: detail(&e.text), key: i.to_string() })
        .collect();
    s.editing.chooser = Some(Chooser {
        kind: ChooserKind::PasteFromHistory,
        editor,
        title: "Choose Content to Paste".into(),
        items,
        query: String::new(),
        selected: 0,
    });
    Ok(history_json(s))
}

/// Paste without whole-line semantics: the text goes in at the carets as it is.
fn paste_plain(s: &mut Session, p: &Value) -> CmdResult {
    let text = match opt_str(p, "text") {
        Some(t) => {
            record(s, t, false);
            t.to_string()
        }
        None => s.clipboard.as_ref().map(|(t, _)| t.clone()).unwrap_or_default(),
    };
    if text.is_empty() {
        return ok();
    }
    s.edit_active(EditGroup::Standalone, |d, sel, _| ops::paste(d, sel, &text, false))?;
    ok()
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "edit.paste.pasteFromHistory",
            "Paste from History…",
            None,
            "{index?: int} — paste that clipboard history entry (0 = the latest); without it, open the chooser → {entries: [{index, preview, lines, text}]}",
            Context::Editor,
            has_editor,
            paste_from_history,
        ),
        spec(
            "edit.paste.pasteAsPlainText",
            "Paste as Plain Text",
            None,
            "{text?: string} — paste the given text (else the clipboard) as typed, never as whole lines",
            Context::Editor,
            has_editor,
            paste_plain,
        ),
        spec(
            "edit.paste.history",
            "Clipboard History",
            None,
            "→ {entries: [{index, preview, lines, text}]}, most recent first",
            Context::Global,
            always,
            |s, _| Ok(history_json(s)),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(text: &str) -> Session {
        let mut s = Session::new();
        let d = s.ws.add_document(None, text);
        s.ws.open_editor(d);
        s
    }

    fn text(s: &Session) -> String {
        s.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
    }

    fn select(s: &mut Session, a: usize, b: usize) {
        s.execute("editor.setSelection", &json!({ "anchor": a, "head": b })).unwrap();
    }

    #[test]
    fn copies_and_cuts_fill_the_history_most_recent_first() {
        let mut s = session("alpha beta gamma");
        select(&mut s, 0, 5);
        s.execute("edit.copy", &Value::Null).unwrap();
        select(&mut s, 6, 10);
        s.execute("edit.copy", &Value::Null).unwrap();
        select(&mut s, 0, 5);
        s.execute("edit.copy", &Value::Null).unwrap();
        let texts: Vec<&str> = s.clip_history.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["alpha", "beta"]);
        select(&mut s, 11, 16);
        s.execute("edit.cut", &Value::Null).unwrap();
        assert_eq!(s.clip_history.entries[0].text, "gamma");
        assert_eq!(text(&s), "alpha beta ");
        // Text pasted from another application joins the history too.
        s.execute("edit.paste.paste", &json!({ "text": "outside" })).unwrap();
        assert_eq!(s.clip_history.entries[0].text, "outside");
        let v = s.execute("edit.paste.history", &Value::Null).unwrap();
        assert_eq!(v["entries"].as_array().map(Vec::len), Some(4));
    }

    #[test]
    fn paste_from_history_opens_a_chooser_and_pastes_the_choice() {
        let mut s = session("one two\n");
        select(&mut s, 0, 3);
        s.execute("edit.copy", &Value::Null).unwrap();
        select(&mut s, 4, 7);
        s.execute("edit.copy", &Value::Null).unwrap();
        s.execute("editor.setSelection", &json!({ "anchor": 8 })).unwrap();
        let v = s.execute("edit.paste.pasteFromHistory", &Value::Null).unwrap();
        assert_eq!(v["entries"][0]["preview"], "two");
        let c = s.editing.chooser.clone().unwrap();
        assert_eq!(c.kind, ChooserKind::PasteFromHistory);
        assert_eq!(c.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["two", "one"]);
        // Choosing "one" pastes it and makes it the latest entry and the clipboard.
        s.execute("editor.chooser.accept", &json!({ "index": 1 })).unwrap();
        assert_eq!(text(&s), "one two\none");
        assert!(s.editing.chooser.is_none());
        assert_eq!(s.clip_history.entries[0].text, "one");
        assert_eq!(s.clipboard, Some(("one".to_string(), false)));
        assert_eq!(s.effects.last(), Some(&Effect::CopyToClipboard("one".into())));
        // Directly by index; a bad index is an error, not a panic.
        s.execute("edit.paste.pasteFromHistory", &json!({ "index": 1 })).unwrap();
        assert_eq!(text(&s), "one two\nonetwo");
        assert!(s.execute("edit.paste.pasteFromHistory", &json!({ "index": 99 })).is_err());
    }

    #[test]
    fn paste_from_history_with_an_empty_history_says_so() {
        let mut s = session("x");
        s.execute("edit.paste.pasteFromHistory", &Value::Null).unwrap();
        assert!(s.editing.chooser.is_none());
        assert!(s.status.contains("empty"));
    }

    #[test]
    fn paste_as_plain_text_never_pastes_whole_lines() {
        let mut s = session("abc\ndef\n");
        // A copy without a selection copies the whole line; Paste inserts it above the caret line.
        s.execute("editor.setSelection", &json!({ "anchor": 1 })).unwrap();
        s.execute("edit.copy", &Value::Null).unwrap();
        s.execute("editor.setSelection", &json!({ "anchor": 5 })).unwrap();
        s.execute("edit.paste.pasteAsPlainText", &Value::Null).unwrap();
        assert_eq!(text(&s), "abc\ndabc\nef\n");
        s.execute("edit.paste.pasteAsPlainText", &json!({ "text": "X" })).unwrap();
        assert_eq!(text(&s), "abc\ndabc\nXef\n");
    }

    #[test]
    fn the_ring_is_bounded_and_skips_huge_or_empty_texts() {
        let mut h = ClipHistory::default();
        for i in 0..(HISTORY_LIMIT + 20) {
            h.push(&format!("t{i}"), false);
        }
        assert_eq!(h.entries.len(), HISTORY_LIMIT);
        assert_eq!(h.entries[0].text, format!("t{}", HISTORY_LIMIT + 19));
        h.push("", false);
        h.push(&"x".repeat(ENTRY_LIMIT_CHARS + 1), false);
        assert_eq!(h.entries.len(), HISTORY_LIMIT);
        assert_eq!(preview("\n\n   fn main() {\n}"), "fn main() {");
        assert!(preview(&"y".repeat(200)).ends_with('…'));
    }
}
