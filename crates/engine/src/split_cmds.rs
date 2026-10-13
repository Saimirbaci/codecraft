//! Split editors and editor tab commands (Window › Editor Tabs, track S).
//!
//! Groups live in the workspace ([`codecraft_workspace::SplitNode`]); these commands drive
//! them. Not registered: Keep Tab Open (CodeCraft has no preview tab), Show Hidden Tabs (the tab
//! bar scrolls instead of hiding tabs) and Split with Chooser Navigation (a submenu).

use serde_json::{Value, json};

use codecraft_workspace::{EditorId, GroupId, SplitAxis, SplitNode, Stretch};

use crate::commands::{CmdResult, CommandSpec, Context, has_editor, has_tabs, ok, simple, spec};
use crate::params::{opt_usize, req_usize};
use crate::{Effect, Session};

/// Share of the splitter one Stretch Editor step moves.
const STRETCH_STEP: f32 = 0.1;

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("window.editorTabs.splitRight", "Split Right", has_editor, |s, _| {
            split(s, SplitAxis::Horizontal, false)
        }),
        simple("window.editorTabs.splitAndMoveRight", "Split and Move Right", has_editor, |s, _| {
            split(s, SplitAxis::Horizontal, true)
        }),
        simple("window.editorTabs.splitDown", "Split Down", has_editor, |s, _| split(s, SplitAxis::Vertical, false)),
        simple("window.editorTabs.splitAndMoveDown", "Split and Move Down", has_editor, |s, _| {
            split(s, SplitAxis::Vertical, true)
        }),
        simple("window.editorTabs.unsplit", "Unsplit", is_split, |s, _| {
            s.ws.unsplit();
            focus(s)
        }),
        simple("window.editorTabs.unsplitAll", "Unsplit All", is_split, |s, _| {
            s.ws.unsplit_all();
            focus(s)
        }),
        simple("window.editorTabs.goToNextSplitter", "Go to Next Splitter", is_split, |s, _| {
            s.ws.cycle_group(1);
            focus(s)
        }),
        simple("window.editorTabs.goToPreviousSplitter", "Go to Previous Splitter", is_split, |s, _| {
            s.ws.cycle_group(-1);
            focus(s)
        }),
        simple("window.editorTabs.changeSplitterOrientation", "Change Splitter Orientation", is_split, |s, _| {
            s.ws.flip_split();
            ok()
        }),
        simple(
            "window.editorTabs.maximizeEditorNormalizeSplits",
            "Maximize Editor/Normalize Splits",
            is_split,
            |s, _| {
                let maximized = s.ws.toggle_maximize();
                Ok(json!({ "maximized": maximized }))
            },
        ),
        simple("window.editorTabs.stretchEditorToTop", "Stretch Editor to Top", is_split, |s, _| {
            stretch(s, Stretch::Top)
        }),
        simple("window.editorTabs.stretchEditorToLeft", "Stretch Editor to Left", is_split, |s, _| {
            stretch(s, Stretch::Left)
        }),
        simple("window.editorTabs.stretchEditorToBottom", "Stretch Editor to Bottom", is_split, |s, _| {
            stretch(s, Stretch::Bottom)
        }),
        simple("window.editorTabs.stretchEditorToRight", "Stretch Editor to Right", is_split, |s, _| {
            stretch(s, Stretch::Right)
        }),
        simple("window.editorTabs.closeAllButPinned", "Close All but Pinned", has_tabs, |s, _| {
            crate::window_cmds::close_where(s, |_, _| true)
        }),
        simple("window.editorTabs.closeAllReadOnly", "Close All Read-Only", has_tabs, |s, _| {
            crate::window_cmds::close_where(s, |s, id| {
                s.ws.editor(id).and_then(|e| s.ws.doc(e.doc)).is_some_and(|d| d.read_only)
            })
        }),
        spec(
            "window.editorTabs.focusGroup",
            "Focus Editor Group",
            None,
            "{group: int}",
            Context::Global,
            has_tabs,
            |s, p| {
                let g = GroupId(req_usize(p, "group")? as u64);
                if !s.ws.focus_group(g) {
                    return Err(crate::CommandError::BadParams(format!("no editor group {}", g.0)));
                }
                focus(s)
            },
        ),
        spec(
            "window.editorTabs.moveTab",
            "Move Tab to Group",
            None,
            "{editor: int, group: int, index?: int} — drag a tab to another group's tab bar (or reorder within one)",
            Context::Global,
            has_tabs,
            |s, p| {
                let id = EditorId(req_usize(p, "editor")? as u64);
                let g = GroupId(req_usize(p, "group")? as u64);
                let index = opt_usize(p, "index")?.unwrap_or(usize::MAX);
                if !s.ws.move_tab(id, g, index) {
                    return Err(crate::CommandError::BadParams("no such tab or group".into()));
                }
                focus(s)
            },
        ),
        simple("window.editorTabs.layout", "Editor Split Layout", crate::commands::always, |s, _| Ok(layout(s))),
    ]
}

fn is_split(s: &Session) -> Result<(), String> {
    if s.ws.is_split() { Ok(()) } else { Err("the editor is not split".into()) }
}

fn split(s: &mut Session, axis: SplitAxis, move_tab: bool) -> CmdResult {
    let Some(g) = s.ws.split_editor(axis, move_tab) else {
        return Err(crate::CommandError::Failed("nothing to split".into()));
    };
    s.effects.push(Effect::FocusEditor);
    Ok(json!({ "group": g.0 }))
}

fn focus(s: &mut Session) -> CmdResult {
    s.effects.push(Effect::FocusEditor);
    ok()
}

fn stretch(s: &mut Session, side: Stretch) -> CmdResult {
    let moved = s.ws.stretch_editor(side, STRETCH_STEP);
    Ok(json!({ "moved": moved }))
}

/// The split tree with each group's tabs, for agents and the CLI.
fn layout(s: &Session) -> Value {
    fn node(s: &Session, n: &SplitNode) -> Value {
        match n {
            SplitNode::Group(g) => {
                let group = s.ws.group(*g);
                let tabs: Vec<Value> = group
                    .map(|gr| gr.tabs.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .map(|id| {
                        let title = s.ws.editor(*id).and_then(|e| s.ws.doc(e.doc)).map(|d| d.title());
                        json!({ "editor": id.0, "title": title })
                    })
                    .collect();
                json!({
                    "group": g.0,
                    "focused": s.ws.focused_group() == *g,
                    "active": group.and_then(|gr| gr.active).map(|e| e.0),
                    "tabs": tabs,
                })
            }
            SplitNode::Split { axis, ratio, first, second } => json!({
                "axis": match axis { SplitAxis::Horizontal => "horizontal", SplitAxis::Vertical => "vertical" },
                "ratio": ratio,
                "first": node(s, first),
                "second": node(s, second),
            }),
        }
    }
    node(s, s.ws.split_layout())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_with_files(names: &[&str]) -> (tempfile::TempDir, Session) {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        for n in names {
            let p = dir.path().join(n);
            std::fs::write(&p, format!("// {n}\nfn main() {{}}\n")).unwrap();
            s.open_file(&p).unwrap();
        }
        (dir, s)
    }

    fn run(s: &mut Session, id: &str) -> Value {
        s.execute(id, &Value::Null).unwrap()
    }

    #[test]
    fn split_commands_and_their_enablement() {
        let (_d, mut s) = session_with_files(&["a.rs", "b.rs"]);
        for id in
            ["window.editorTabs.unsplit", "window.editorTabs.goToNextSplitter", "window.editorTabs.stretchEditorToLeft"]
        {
            assert!(s.enabled(id).is_err(), "{id}");
        }
        let g = run(&mut s, "window.editorTabs.splitRight")["group"].as_u64().unwrap();
        assert_eq!(s.ws.focused_group(), GroupId(g));
        assert_eq!(s.ws.group_ids().len(), 2);
        run(&mut s, "window.editorTabs.splitAndMoveDown");
        assert_eq!(s.ws.group_ids().len(), 3);
        let l = run(&mut s, "window.editorTabs.layout");
        assert_eq!(l["axis"], "horizontal");
        assert_eq!(l["second"]["axis"], "vertical");
        run(&mut s, "window.editorTabs.goToNextSplitter");
        assert_eq!(s.ws.focused_group(), s.ws.group_ids()[0]);
        run(&mut s, "window.editorTabs.goToPreviousSplitter");
        run(&mut s, "window.editorTabs.changeSplitterOrientation");
        assert_eq!(run(&mut s, "window.editorTabs.maximizeEditorNormalizeSplits")["maximized"], true);
        assert_eq!(run(&mut s, "window.editorTabs.maximizeEditorNormalizeSplits")["maximized"], false);
        // The last group sits right of both (now horizontal) splitters: it can only grow left.
        assert_eq!(run(&mut s, "window.editorTabs.stretchEditorToLeft")["moved"], true);
        assert_eq!(run(&mut s, "window.editorTabs.stretchEditorToBottom")["moved"], false);
        run(&mut s, "window.editorTabs.unsplit");
        assert_eq!(s.ws.group_ids().len(), 2);
        run(&mut s, "window.editorTabs.unsplitAll");
        assert!(!s.ws.is_split());
        assert_eq!(s.ws.tabs().len(), 2);
        assert!(s.effects.contains(&Effect::FocusEditor));
    }

    #[test]
    fn focus_group_and_move_tab_validate_params() {
        let (_d, mut s) = session_with_files(&["a.rs", "b.rs"]);
        let first = s.ws.focused_group();
        let a = s.ws.tabs()[0];
        run(&mut s, "window.editorTabs.splitAndMoveRight");
        assert!(s.execute("window.editorTabs.focusGroup", &json!({ "group": 999 })).is_err());
        assert!(s.execute("window.editorTabs.focusGroup", &json!({ "group": "x" })).is_err());
        s.execute("window.editorTabs.focusGroup", &json!({ "group": first.0 })).unwrap();
        assert_eq!(s.ws.active_editor(), Some(a));
        let second = s.ws.group_ids()[1];
        s.execute("window.editorTabs.moveTab", &json!({ "editor": a.0, "group": second.0 })).unwrap();
        assert!(!s.ws.is_split());
        assert!(s.execute("window.editorTabs.moveTab", &json!({ "editor": 999, "group": second.0 })).is_err());
    }

    #[test]
    fn close_all_but_pinned_and_read_only() {
        let (_d, mut s) = session_with_files(&["a.rs", "b.rs", "c.rs"]);
        let tabs = s.ws.tabs().to_vec();
        s.ws.editor_mut(tabs[0]).unwrap().pinned = true;
        let doc_c = s.ws.editor(tabs[2]).unwrap().doc;
        s.ws.doc_mut(doc_c).unwrap().read_only = true;
        run(&mut s, "window.editorTabs.closeAllReadOnly");
        assert_eq!(s.ws.tabs(), &tabs[..2]);
        run(&mut s, "window.editorTabs.closeAllButPinned");
        assert_eq!(s.ws.tabs(), &tabs[..1]);
    }

    #[test]
    fn edits_in_one_view_move_the_other_views_caret() {
        let (_d, mut s) = session_with_files(&["a.rs"]);
        let first = s.ws.active_editor().unwrap();
        s.execute("editor.click", &json!({ "offset": 8 })).unwrap();
        run(&mut s, "window.editorTabs.splitRight");
        let second = s.ws.active_editor().unwrap();
        s.execute("editor.click", &json!({ "offset": 0 })).unwrap();
        s.execute("editor.type", &json!({ "text": "xyz" })).unwrap();
        assert_eq!(s.ws.editor(second).unwrap().selections.primary().head, 3);
        assert_eq!(s.ws.editor(first).unwrap().selections.primary().head, 11);
    }
}
