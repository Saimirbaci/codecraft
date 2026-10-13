//! Edit › Bookmarks commands (RustRover fills that submenu lazily, so the ids are ours) and the
//! Bookmarks tool window's actions.

use std::path::PathBuf;

use serde_json::{Value, json};

use codecraft_workspace::ToolWindowId;

use crate::bookmarks::{self, DescriptionDialog, MnemonicDialog};
use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, has_editor, ok, spec};
use crate::params::{opt_path, opt_str, opt_usize, req_usize};
use crate::Session;

const TARGET: &str = "{path?, line?} (default: the caret line of the active editor)";

/// `{path, line}` from params, else the active editor's file and caret line.
fn target(s: &Session, p: &Value) -> Result<(PathBuf, usize), CommandError> {
    let line = opt_usize(p, "line")?;
    if let Some(path) = opt_path(p, "path") {
        return Ok((crate::normalize_path(&path), line.unwrap_or(0)));
    }
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Failed("no editor is open".into()))?;
    let path = d.path.clone().ok_or_else(|| CommandError::Failed("the file is not saved yet".into()))?;
    Ok((path, line.unwrap_or_else(|| d.doc.char_to_line(e.selections.primary().head))))
}

fn mnemonic_param(p: &Value) -> Result<Option<char>, CommandError> {
    let Some(m) = opt_str(p, "mnemonic") else { return Ok(None) };
    let mut chars = m.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => bookmarks::normalize_mnemonic(c)
            .map(Some)
            .ok_or_else(|| CommandError::BadParams(format!("'{m}' is not a mnemonic (use 0-9 or A-Z)"))),
        _ => Err(CommandError::BadParams(format!("'{m}' is not a mnemonic (use 0-9 or A-Z)"))),
    }
}

fn failed(e: String) -> CommandError {
    CommandError::Failed(e)
}

fn list_json(s: &Session) -> Value {
    let items: Vec<Value> = bookmarks::sorted(&s.bookmarks.items)
        .into_iter()
        .filter_map(|i| {
            let b = s.bookmarks.items.get(i)?;
            Some(json!({
                "index": i,
                "path": b.path,
                "line": b.line,
                "mnemonic": b.mnemonic.map(String::from),
                "description": b.description,
                "preview": b.preview,
            }))
        })
        .collect();
    json!({ "bookmarks": items, "error": s.bookmarks.error })
}

fn open(s: &mut Session, index: usize) -> CmdResult {
    let b = s.bookmarks.items.get(index).cloned().ok_or_else(|| CommandError::Failed(format!("no bookmark {index}")))?;
    s.navigate_to_position(&b.path, b.line, 0)?;
    s.bookmarks.popup = false;
    ok()
}

fn goto_mnemonic(s: &mut Session, m: char) -> CmdResult {
    bookmarks::ensure_loaded(s);
    bookmarks::sync(s);
    match s.bookmarks.by_mnemonic(m) {
        Some(i) => open(s, i),
        None => {
            s.status = format!("No bookmark {m}");
            ok()
        }
    }
}

fn step(s: &mut Session, forward: bool) -> CmdResult {
    bookmarks::ensure_loaded(s);
    bookmarks::sync(s);
    let (path, line) = target(s, &Value::Null)?;
    let lines: Vec<usize> = s.bookmarks.lines_in(&path).into_iter().map(|(l, _)| l).collect();
    match bookmarks::next_line(&lines, line, forward) {
        Some(l) => {
            s.navigate_to_position(&path, l, 0)?;
            Ok(json!({ "line": l }))
        }
        None => {
            s.status = "No bookmarks in this file".into();
            ok()
        }
    }
}

macro_rules! goto_digit {
    ($id:literal, $label:literal, $key:literal, $m:literal) => {
        spec($id, $label, Some($key), "", Context::Global, always, |s, _| goto_mnemonic(s, $m))
    };
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec("edit.bookmarks.toggle", "Toggle Bookmark", Some("F3"), TARGET, Context::Editor, has_editor, |s, p| {
            let (path, line) = target(s, p)?;
            let on = bookmarks::toggle(s, path, line).map_err(failed)?;
            Ok(json!({ "bookmarked": on }))
        }),
        spec(
            "edit.bookmarks.toggleWithMnemonic",
            "Toggle Bookmark with Mnemonic",
            Some("Alt+F3"),
            "{mnemonic?: \"0\"-\"9\" | \"A\"-\"Z\", path?, line?}: without a mnemonic, opens the chooser",
            Context::Editor,
            has_editor,
            |s, p| {
                let (path, line) = target(s, p)?;
                bookmarks::ensure_loaded(s);
                bookmarks::sync(s);
                match mnemonic_param(p)? {
                    Some(m) => {
                        s.bookmarks.mnemonic_dialog = None;
                        let on = bookmarks::set_mnemonic(s, path, line, m).map_err(failed)?;
                        Ok(json!({ "bookmarked": on }))
                    }
                    None => {
                        let existing = s.bookmarks.find(&path, line).and_then(|i| s.bookmarks.items.get(i));
                        s.bookmarks.mnemonic_dialog = Some(MnemonicDialog {
                            current: existing.and_then(|b| b.mnemonic),
                            description: existing.map(|b| b.description.clone()).unwrap_or_default(),
                            path,
                            line,
                        });
                        ok()
                    }
                }
            },
        ),
        spec(
            "edit.bookmarks.closeMnemonicDialog",
            "Close Bookmark Mnemonic Chooser",
            None,
            "",
            Context::Global,
            always,
            |s, _| {
                s.bookmarks.mnemonic_dialog = None;
                ok()
            },
        ),
        spec(
            "edit.bookmarks.show",
            "Show Bookmarks",
            Some("Cmd+F3"),
            "Opens the bookmarks popup; returns the list",
            Context::Global,
            always,
            |s, _| {
                bookmarks::ensure_loaded(s);
                bookmarks::sync(s);
                s.bookmarks.popup = true;
                Ok(list_json(s))
            },
        ),
        spec("edit.bookmarks.closePopup", "Close Bookmarks Popup", None, "", Context::Global, always, |s, _| {
            s.bookmarks.popup = false;
            ok()
        }),
        spec("edit.bookmarks.list", "List Bookmarks", None, "Returns {bookmarks: [{index, path, line, mnemonic, description}]}", Context::Global, always, |s, _| {
            bookmarks::ensure_loaded(s);
            bookmarks::sync(s);
            Ok(list_json(s))
        }),
        spec("edit.bookmarks.open", "Go to Bookmark", None, "{index}", Context::Global, always, |s, p| {
            let i = req_usize(p, "index")?;
            open(s, i)
        }),
        spec(
            "edit.bookmarks.gotoMnemonic",
            "Go to Bookmark with Mnemonic",
            None,
            "{mnemonic}",
            Context::Global,
            always,
            |s, p| {
                let m = mnemonic_param(p)?.ok_or_else(|| CommandError::BadParams("missing mnemonic".into()))?;
                goto_mnemonic(s, m)
            },
        ),
        goto_digit!("edit.bookmarks.goto0", "Go to Bookmark 0", "Ctrl+0", '0'),
        goto_digit!("edit.bookmarks.goto1", "Go to Bookmark 1", "Ctrl+1", '1'),
        goto_digit!("edit.bookmarks.goto2", "Go to Bookmark 2", "Ctrl+2", '2'),
        goto_digit!("edit.bookmarks.goto3", "Go to Bookmark 3", "Ctrl+3", '3'),
        goto_digit!("edit.bookmarks.goto4", "Go to Bookmark 4", "Ctrl+4", '4'),
        goto_digit!("edit.bookmarks.goto5", "Go to Bookmark 5", "Ctrl+5", '5'),
        goto_digit!("edit.bookmarks.goto6", "Go to Bookmark 6", "Ctrl+6", '6'),
        goto_digit!("edit.bookmarks.goto7", "Go to Bookmark 7", "Ctrl+7", '7'),
        goto_digit!("edit.bookmarks.goto8", "Go to Bookmark 8", "Ctrl+8", '8'),
        goto_digit!("edit.bookmarks.goto9", "Go to Bookmark 9", "Ctrl+9", '9'),
        spec("edit.bookmarks.next", "Next Line Bookmark", None, "", Context::Global, has_editor, |s, _| step(s, true)),
        spec("edit.bookmarks.previous", "Previous Line Bookmark", None, "", Context::Global, has_editor, |s, _| {
            step(s, false)
        }),
        spec(
            "edit.bookmarks.editDescription",
            "Edit Bookmark Description…",
            None,
            "{index?, description?}: without a description, opens the dialog (default: the caret line's bookmark)",
            Context::Global,
            always,
            |s, p| {
                bookmarks::ensure_loaded(s);
                bookmarks::sync(s);
                let index = match opt_usize(p, "index")? {
                    Some(i) => i,
                    None => {
                        let (path, line) = target(s, p)?;
                        s.bookmarks.find(&path, line).ok_or_else(|| failed("no bookmark on this line".into()))?
                    }
                };
                match opt_str(p, "description") {
                    Some(d) => {
                        s.bookmarks.description_dialog = None;
                        bookmarks::set_description(s, index, d).map_err(failed)?;
                    }
                    None => {
                        let b = s.bookmarks.items.get(index).ok_or_else(|| failed(format!("no bookmark {index}")))?;
                        s.bookmarks.description_dialog =
                            Some(DescriptionDialog { index, description: b.description.clone() });
                    }
                }
                ok()
            },
        ),
        spec("edit.bookmarks.closeDescriptionDialog", "Close Bookmark Description", None, "", Context::Global, always, |s, _| {
            s.bookmarks.description_dialog = None;
            ok()
        }),
        spec("edit.bookmarks.remove", "Delete Bookmark", None, "{index}", Context::Global, always, |s, p| {
            let i = req_usize(p, "index")?;
            bookmarks::ensure_loaded(s);
            if i >= s.bookmarks.items.len() {
                return Err(failed(format!("no bookmark {i}")));
            }
            s.bookmarks.items.remove(i);
            bookmarks::persist(s);
            ok()
        }),
        spec("edit.bookmarks.removeAll", "Delete All Bookmarks", None, "", Context::Global, always, |s, _| {
            bookmarks::ensure_loaded(s);
            s.bookmarks.items.clear();
            bookmarks::persist(s);
            ok()
        }),
        spec(
            "edit.bookmarks.showToolWindow",
            "Bookmarks Tool Window",
            None,
            "",
            Context::Global,
            always,
            |s, _| {
                s.ws.layout.show(ToolWindowId::Bookmarks);
                s.bookmarks.popup = false;
                ok()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    fn project() -> (tempfile::TempDir, std::path::PathBuf, Session) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.rs"), "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        s.execute("file.open", &json!({ "path": root.join("a.rs") })).unwrap();
        (dir, root, s)
    }

    fn lines(s: &mut Session) -> Vec<(usize, Option<String>)> {
        let v = s.execute("edit.bookmarks.list", &json!({})).unwrap();
        v["bookmarks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| (b["line"].as_u64().unwrap() as usize, b["mnemonic"].as_str().map(str::to_string)))
            .collect()
    }

    #[test]
    fn toggle_persist_and_follow_edits() {
        let (_d, root, mut s) = project();
        s.execute("editor.setSelection", &json!({ "anchor": 10, "head": 10 })).unwrap();
        assert_eq!(s.execute("edit.bookmarks.toggle", &json!({})).unwrap()["bookmarked"], true);
        s.execute("edit.bookmarks.toggleWithMnemonic", &json!({ "line": 3, "mnemonic": "7" })).unwrap();
        assert_eq!(lines(&mut s), vec![(1, None), (3, Some("7".into()))]);
        assert!(root.join(".codecraft/bookmarks.json").exists());

        // Insert two lines at the top: both bookmarks move down.
        s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
        s.execute("editor.type", &json!({ "text": "// x\n// y\n" })).unwrap();
        s.pump();
        assert_eq!(lines(&mut s), vec![(3, None), (5, Some("7".into()))]);

        // A new session reads them back (persisted after the move).
        let mut s2 = Session::new();
        s2.lsp_enabled = false;
        s2.execute("file.open", &json!({ "path": root })).unwrap();
        assert_eq!(lines(&mut s2), vec![(3, None), (5, Some("7".into()))]);

        // Go to the mnemonic, then next/previous wrap within the file.
        s.execute("edit.bookmarks.goto7", &json!({})).unwrap();
        let caret_line = |s: &Session| {
            let (e, d) = s.ws.active().unwrap();
            d.doc.char_to_line(e.selections.primary().head)
        };
        assert_eq!(caret_line(&s), 5);
        s.execute("edit.bookmarks.next", &json!({})).unwrap();
        assert_eq!(caret_line(&s), 3);
        s.execute("edit.bookmarks.previous", &json!({})).unwrap();
        assert_eq!(caret_line(&s), 5);

        // Same mnemonic on the same line removes it; toggling removes the plain one.
        s.execute("edit.bookmarks.toggleWithMnemonic", &json!({ "line": 5, "mnemonic": "7" })).unwrap();
        s.execute("edit.bookmarks.toggle", &json!({ "line": 3 })).unwrap();
        assert!(lines(&mut s).is_empty());
    }

    #[test]
    fn deleted_line_merges_and_description() {
        let (_d, _root, mut s) = project();
        s.execute("edit.bookmarks.toggle", &json!({ "line": 1 })).unwrap();
        s.execute("edit.bookmarks.toggle", &json!({ "line": 2 })).unwrap();
        // Delete line 1 ("fn b() {}\n"): both bookmarks end up on line 1; one survives.
        s.execute("editor.setSelection", &json!({ "anchor": 10, "head": 20 })).unwrap();
        s.execute("editor.type", &json!({ "text": "" })).ok();
        s.execute("edit.deleteLine", &json!({})).ok();
        s.pump();
        let ls = lines(&mut s);
        assert!(ls.len() <= 2 && !ls.is_empty(), "{ls:?}");
        let idx = s.execute("edit.bookmarks.list", &json!({})).unwrap()["bookmarks"][0]["index"].as_u64().unwrap();
        s.execute("edit.bookmarks.editDescription", &json!({ "index": idx, "description": "  here " })).unwrap();
        let v = s.execute("edit.bookmarks.list", &json!({})).unwrap();
        assert_eq!(v["bookmarks"][0]["description"], "here");
        // Without a description the dialog opens.
        s.execute("edit.bookmarks.editDescription", &json!({ "index": idx })).unwrap();
        assert!(s.bookmarks.description_dialog.is_some());
    }

    #[test]
    fn hostile_params_are_errors() {
        let (_d, _root, mut s) = project();
        for p in [
            json!({ "mnemonic": "ab" }),
            json!({ "mnemonic": "!" }),
            json!({ "mnemonic": 5 }),
            json!({ "line": -1 }),
            json!({ "line": "x" }),
        ] {
            let r = s.execute("edit.bookmarks.toggleWithMnemonic", &p);
            assert!(r.is_err() || s.bookmarks.mnemonic_dialog.is_some(), "{p}: {r:?}");
        }
        assert!(s.execute("edit.bookmarks.open", &json!({ "index": 99 })).is_err());
        assert!(s.execute("edit.bookmarks.remove", &json!({ "index": usize::MAX })).is_err());
        // A huge line clamps to the last line on sync.
        s.execute("edit.bookmarks.toggle", &json!({ "line": 1_000_000 })).unwrap();
        s.pump();
        assert_eq!(lines(&mut s), vec![(4, None)]);
        // Without a mnemonic, the chooser opens.
        s.execute("edit.bookmarks.toggleWithMnemonic", &json!({})).unwrap();
        assert!(s.bookmarks.mnemonic_dialog.is_some());
    }
}
