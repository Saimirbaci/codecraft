//! Edit › Macros: record the editor commands and typed text of the command stream, play the last
//! macro back, save named macros in `$CODECRAFT_HOME/macros.json`, rename and delete them, and
//! bind a shortcut to one (the UI's keymap runs `edit.macros.play` for it).
//!
//! Recording hooks [`crate::Session::execute`]: every top-level command that edits or moves in the
//! editor is appended (consecutive typing is merged); mouse gestures, popups' internal
//! navigation and the macro commands themselves are not recorded, like RustRover. Playback runs
//! the steps through `execute` again and stops at the first failing step.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{opt_str, opt_usize, req_str};
use crate::user_store::{Store, UserHome};
use crate::{CommandError, Session};

pub const MACROS_FILE: &str = "macros.json";
/// Steps kept per macro.
pub const MAX_STEPS: usize = 10_000;
/// Text merged into one typing step.
const MAX_TYPED: usize = 100_000;
/// Repetitions per playback.
const MAX_TIMES: usize = 1000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MacroStep {
    pub command: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Macro {
    pub name: String,
    pub steps: Vec<MacroStep>,
    /// A shortcut that plays the macro (`Ctrl+Alt+M` style).
    #[serde(default)]
    pub shortcut: Option<String>,
}

impl Macro {
    /// A readable one-line summary of the steps ("Type 'abc', Down, Line End").
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        for s in self.steps.iter().take(12) {
            let label = crate::registry().get(&s.command).map(|c| c.label).unwrap_or(s.command.as_str());
            match (s.command.as_str(), s.params.get("text").and_then(Value::as_str)) {
                ("editor.type", Some(t)) => {
                    let t: String = t.chars().take(30).collect();
                    parts.push(format!("Type '{}'", t.replace('\n', "⏎")));
                }
                _ => parts.push(label.trim_end_matches('…').to_string()),
            }
        }
        if self.steps.len() > 12 {
            parts.push(format!("… {} more", self.steps.len() - 12));
        }
        parts.join(", ")
    }
}

/// The Edit Macros dialog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditMacrosDialog {
    pub selected: Option<String>,
    /// Draft name and shortcut for the selected macro.
    pub rename: String,
    pub shortcut: String,
    pub error: Option<String>,
}

pub struct MacroState {
    /// Steps recorded so far (`Some` while recording).
    pub recording: Option<Vec<MacroStep>>,
    /// The macro recorded last (what Play Back Last Macro plays).
    pub last: Option<Macro>,
    pub saved: Store<Macro>,
    pub home: UserHome,
    /// After stopping: the name prompt's draft (empty name = keep it as the last macro only).
    pub name_prompt: Option<String>,
    pub edit: Option<EditMacrosDialog>,
    /// Play Saved Macros chooser.
    pub chooser: bool,
    /// A macro is being played (its steps are not recorded and macros cannot nest).
    pub playing: bool,
    /// Nesting depth of `Session::execute` (only top-level commands are recorded).
    pub(crate) depth: usize,
}

impl Default for MacroState {
    fn default() -> Self {
        Self {
            recording: None,
            last: None,
            saved: Store::new(MACROS_FILE),
            home: UserHome::default(),
            name_prompt: None,
            edit: None,
            chooser: false,
            playing: false,
            depth: 0,
        }
    }
}

impl MacroState {
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    pub fn set_home(&mut self, dir: std::path::PathBuf) {
        self.home.dir_override = Some(dir);
        self.saved.reset();
    }

    /// Saved macros (loaded on first use).
    pub fn list(&mut self) -> &[Macro] {
        self.saved.ensure(&self.home);
        &self.saved.items
    }

    /// The saved macro bound to a shortcut, if any (`shortcut` in the keymap's notation).
    pub fn for_shortcut(&mut self, shortcut: &str) -> Option<String> {
        let norm = normalize_shortcut(shortcut);
        self.list().iter().find(|m| m.shortcut.as_deref().map(normalize_shortcut).as_deref() == Some(norm.as_str())).map(|m| m.name.clone())
    }
}

/// `Shift+Cmd+M` and `cmd+shift+m` compare equal.
pub fn normalize_shortcut(s: &str) -> String {
    let mut parts: Vec<String> = s.split('+').map(|p| p.trim().to_ascii_lowercase()).filter(|p| !p.is_empty()).collect();
    let key = parts.pop().unwrap_or_default();
    parts.sort();
    parts.push(key);
    parts.join("+")
}

/// Commands that are never recorded (mouse gestures, chooser navigation, queries).
const NEVER_RECORDED: &[&str] = &[
    "editor.click",
    "editor.drag",
    "editor.boxSelect",
    "editor.setSelection",
    "editor.inlayHints",
    "editor.inlayHint.navigate",
    "editor.completion.select",
    "editor.chooser.move",
    "editor.chooser.setQuery",
    "editor.chooser.close",
    // A network request to the AI provider; its proposal is not reproducible.
    "code.codeCompletion.callInlineCompletion",
];

/// Whether a command is part of what a macro records.
pub fn recordable(id: &str, context: Context) -> bool {
    if id.starts_with("edit.macros.") || NEVER_RECORDED.contains(&id) {
        return false;
    }
    context == Context::Editor
        || ["editor.", "edit.", "code.", "navigate.", "refactor."].iter().any(|p| id.starts_with(p))
}

/// Called by `Session::execute` after a top-level command succeeded.
pub(crate) fn record(s: &mut Session, id: &str, params: &Value) {
    if s.macros.playing {
        return;
    }
    let Some(steps) = &mut s.macros.recording else { return };
    let Some(spec) = crate::registry().get(id) else { return };
    if !recordable(spec.id, spec.context) {
        return;
    }
    if spec.id == "editor.type"
        && let Some(text) = params.get("text").and_then(Value::as_str)
        && let Some(last) = steps.last_mut()
        && last.command == "editor.type"
        && let Some(prev) = last.params.get("text").and_then(Value::as_str)
        && prev.len() + text.len() <= MAX_TYPED
    {
        last.params = json!({ "text": format!("{prev}{text}") });
        return;
    }
    if steps.len() >= MAX_STEPS {
        return;
    }
    steps.push(MacroStep { command: spec.id.to_string(), params: params.clone() });
}

fn not_playing(s: &Session) -> Result<(), String> {
    if s.macros.playing { Err("a macro is playing".into()) } else { Ok(()) }
}

fn can_play(s: &Session) -> Result<(), String> {
    not_playing(s)?;
    if s.macros.is_recording() {
        return Err("stop recording first".into());
    }
    Ok(())
}

/// Run a macro's steps (`times` times); stops at the first failing step.
pub(crate) fn play(s: &mut Session, m: &Macro, times: usize) -> CmdResult {
    let times = times.clamp(1, MAX_TIMES);
    s.macros.playing = true;
    let mut done = 0usize;
    let mut error = None;
    'outer: for _ in 0..times {
        for (i, step) in m.steps.iter().enumerate() {
            if step.command.starts_with("edit.macros.") {
                continue;
            }
            if let Err(e) = s.execute(&step.command, &step.params) {
                error = Some(format!("step {} ({}): {e}", i + 1, step.command));
                break 'outer;
            }
            done += 1;
        }
    }
    s.macros.playing = false;
    match error {
        Some(e) => {
            s.status = format!("Macro '{}' stopped at {e}", m.name);
            Err(CommandError::Failed(s.status.clone()))
        }
        None => {
            s.status = format!("Played macro '{}'", m.name);
            Ok(json!({ "steps": done }))
        }
    }
}

fn find_saved(s: &mut Session, name: &str) -> Result<Macro, CommandError> {
    s.macros.list().iter().find(|m| m.name == name).cloned().ok_or_else(|| CommandError::BadParams(format!("no macro named '{name}'")))
}

fn save_named(s: &mut Session, name: &str) -> CmdResult {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::BadParams("the macro needs a name".into()));
    }
    let mut m = s.macros.last.clone().ok_or_else(|| CommandError::Failed("no macro was recorded".into()))?;
    m.name = name.to_string();
    let home = s.macros.home.clone();
    s.macros.saved.ensure(&home);
    let shortcut = s.macros.saved.items.iter().find(|x| x.name == name).and_then(|x| x.shortcut.clone());
    m.shortcut = shortcut;
    s.macros.saved.items.retain(|x| x.name != name);
    s.macros.saved.items.push(m.clone());
    let path = s.macros.saved.save(&home).map_err(CommandError::Failed)?;
    s.macros.last = Some(m);
    s.macros.name_prompt = None;
    s.status = format!("Saved macro '{name}'");
    Ok(json!({ "path": path }))
}

fn toggle_recording(s: &mut Session, p: &Value) -> CmdResult {
    match s.macros.recording.take() {
        None => {
            s.macros.recording = Some(Vec::new());
            s.macros.name_prompt = None;
            s.status = "Macro recording started".into();
            Ok(json!({ "recording": true }))
        }
        Some(steps) => {
            if steps.is_empty() {
                s.status = "Macro recording stopped: nothing was recorded".into();
                return Ok(json!({ "recording": false, "steps": 0 }));
            }
            let n = steps.len();
            s.macros.last = Some(Macro { name: "Last macro".into(), steps, shortcut: None });
            match opt_str(p, "name") {
                Some(name) if !name.trim().is_empty() => {
                    save_named(s, name)?;
                }
                _ => s.macros.name_prompt = Some(String::new()),
            }
            s.status = format!("Macro recording stopped: {n} step(s)");
            Ok(json!({ "recording": false, "steps": n }))
        }
    }
}

fn edit_macros(s: &mut Session, p: &Value) -> CmdResult {
    let Some(action) = opt_str(p, "action") else {
        let first = s.macros.list().first().map(|m| (m.name.clone(), m.shortcut.clone().unwrap_or_default()));
        s.macros.edit = Some(EditMacrosDialog {
            rename: first.as_ref().map(|f| f.0.clone()).unwrap_or_default(),
            shortcut: first.as_ref().map(|f| f.1.clone()).unwrap_or_default(),
            selected: first.map(|f| f.0),
            error: None,
        });
        return Ok(json!({ "dialog": true }));
    };
    let name = req_str(p, "name")?.to_string();
    let home = s.macros.home.clone();
    s.macros.saved.ensure(&home);
    let index = s
        .macros
        .saved
        .items
        .iter()
        .position(|m| m.name == name)
        .ok_or_else(|| CommandError::BadParams(format!("no macro named '{name}'")))?;
    let result = match action {
        "rename" => {
            let new_name = req_str(p, "newName")?.trim().to_string();
            if new_name.is_empty() {
                return Err(CommandError::BadParams("the new name is empty".into()));
            }
            if new_name != name && s.macros.saved.items.iter().any(|m| m.name == new_name) {
                return Err(CommandError::BadParams(format!("a macro named '{new_name}' exists")));
            }
            if let Some(m) = s.macros.saved.items.get_mut(index) {
                m.name = new_name.clone();
            }
            json!({ "name": new_name })
        }
        "delete" => {
            s.macros.saved.items.remove(index);
            json!({ "deleted": name })
        }
        "setShortcut" => {
            let sc = opt_str(p, "shortcut").map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
            if let Some(sc) = &sc {
                let parts: Vec<&str> = sc.split('+').collect();
                if parts.iter().any(|x| x.trim().is_empty()) || parts.len() > 5 {
                    return Err(CommandError::BadParams(format!("'{sc}' is not a shortcut (e.g. Ctrl+Alt+M)")));
                }
                let norm = normalize_shortcut(sc);
                if s.macros.saved.items.iter().any(|m| m.name != name && m.shortcut.as_deref().map(normalize_shortcut) == Some(norm.clone())) {
                    return Err(CommandError::BadParams(format!("{sc} is already bound to another macro")));
                }
            }
            if let Some(m) = s.macros.saved.items.get_mut(index) {
                m.shortcut = sc.clone();
            }
            json!({ "shortcut": sc })
        }
        other => return Err(CommandError::BadParams(format!("unknown action '{other}' (rename, delete, setShortcut)"))),
    };
    s.macros.saved.save(&home).map_err(CommandError::Failed)?;
    if let Some(d) = &mut s.macros.edit {
        if action == "delete" {
            d.selected = None;
            d.rename.clear();
            d.shortcut.clear();
        } else if action == "rename" {
            d.selected = result.get("name").and_then(Value::as_str).map(str::to_string);
        }
        d.error = None;
    }
    Ok(result)
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "edit.macros.startMacroRecording",
            "Start Macro Recording",
            None,
            "{name?: string} — start recording editor commands and typing; run again to stop (with `name`, the macro is saved under it, otherwise a name prompt opens)",
            Context::Global,
            not_playing,
            toggle_recording,
        ),
        spec(
            "edit.macros.playBackLastMacro",
            "Play Back Last Macro",
            None,
            "{times?: int} — play the macro recorded last",
            Context::Global,
            |s| {
                can_play(s)?;
                s.macros.last.as_ref().map(|_| ()).ok_or_else(|| "no macro has been recorded".into())
            },
            |s, p| {
                let m = s.macros.last.clone().ok_or_else(|| CommandError::Failed("no macro has been recorded".into()))?;
                play(s, &m, opt_usize(p, "times")?.unwrap_or(1))
            },
        ),
        spec(
            "edit.macros.playSavedMacros",
            "Play Saved Macros…",
            None,
            "{name?: string, times?: int} — play a saved macro; without `name` opens the chooser",
            Context::Global,
            can_play,
            |s, p| match opt_str(p, "name") {
                Some(name) => {
                    let m = find_saved(s, name)?;
                    s.macros.chooser = false;
                    play(s, &m, opt_usize(p, "times")?.unwrap_or(1))
                }
                None => {
                    if s.macros.list().is_empty() {
                        s.status = "There are no saved macros (Edit › Macros › Start Macro Recording)".into();
                        return Ok(json!({ "macros": 0 }));
                    }
                    s.macros.chooser = true;
                    Ok(json!({ "chooser": true }))
                }
            },
        ),
        spec(
            "edit.macros.editMacros",
            "Edit Macros",
            None,
            "{action?: \"rename\"|\"delete\"|\"setShortcut\", name?: string, newName?: string, shortcut?: string} — without `action` opens the Edit Macros dialog",
            Context::Global,
            not_playing,
            edit_macros,
        ),
        spec("edit.macros.play", "Play Macro", None, "{name: string} — play a saved macro (shortcut bindings)", Context::Global, can_play, |s, p| {
            let m = find_saved(s, req_str(p, "name")?)?;
            play(s, &m, 1)
        }),
        spec(
            "edit.macros.save",
            "Save Macro",
            None,
            "{name: string} — save the last recorded macro under a name",
            Context::Global,
            always,
            |s, p| save_named(s, req_str(p, "name")?),
        ),
        spec("edit.macros.dismiss", "Close", None, "Close the macro name prompt, chooser and dialog", Context::Global, always, |s, _| {
            s.macros.name_prompt = None;
            s.macros.chooser = false;
            s.macros.edit = None;
            ok()
        }),
        spec("edit.macros.list", "Macros", None, "→ {recording, last, saved: [{name, steps, shortcut, summary}]}", Context::Global, always, |s, _| {
            let recording = s.macros.recording.as_ref().map(Vec::len);
            let last = s.macros.last.clone();
            let saved: Vec<Value> = s
                .macros
                .list()
                .iter()
                .map(|m| json!({ "name": m.name, "steps": m.steps, "shortcut": m.shortcut, "summary": m.summary() }))
                .collect();
            Ok(json!({ "recording": recording, "last": last, "saved": saved }))
        }),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn session() -> (tempfile::TempDir, Session) {
        let tmp = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.isolate_home(tmp.path());
        let d = s.ws.add_document(None, "one\ntwo\nthree\n");
        s.ws.open_editor(d);
        (tmp, s)
    }

    fn text(s: &Session) -> String {
        s.ws.active().unwrap().1.doc.text()
    }

    #[test]
    fn record_play_save_and_edit() {
        let (tmp, mut s) = session();
        s.execute("edit.macros.startMacroRecording", &json!({})).unwrap();
        assert!(s.enabled("edit.macros.playBackLastMacro").is_err());
        s.execute("editor.lineStart", &json!({})).unwrap();
        s.execute("editor.type", &json!({ "text": "- " })).unwrap();
        s.execute("editor.type", &json!({ "text": "[ ] " })).unwrap();
        s.execute("editor.down", &json!({})).unwrap();
        // Not recorded: mouse, macro commands, non-editor commands.
        s.execute("editor.click", &json!({ "offset": 10 })).ok();
        s.execute("view.toggleLineNumbers", &json!({})).unwrap();
        s.execute("editor.setSelection", &json!({ "anchor": 10, "head": 10 })).ok();
        let v = s.execute("edit.macros.startMacroRecording", &json!({})).unwrap();
        assert_eq!(v["steps"], 3, "{:?}", s.macros.last);
        assert_eq!(s.macros.name_prompt.as_deref(), Some(""));
        assert_eq!(text(&s), "- [ ] one\ntwo\nthree\n");
        s.execute("edit.macros.playBackLastMacro", &json!({ "times": 2 })).unwrap();
        assert_eq!(text(&s), "- [ ] one\n- [ ] two\n- [ ] three\n");

        // Save, list, bind a shortcut, rename, play by name, delete.
        s.execute("edit.macros.save", &json!({ "name": "checkbox" })).unwrap();
        assert!(tmp.path().join(MACROS_FILE).is_file());
        s.macros.saved.reset();
        let list = s.execute("edit.macros.list", &json!({})).unwrap();
        assert_eq!(list["saved"][0]["name"], "checkbox");
        assert!(list["saved"][0]["summary"].as_str().unwrap().contains("Type '- [ ] '"));
        s.execute("edit.macros.editMacros", &json!({ "action": "setShortcut", "name": "checkbox", "shortcut": "Ctrl+Alt+M" })).unwrap();
        assert_eq!(s.macros.for_shortcut("alt+ctrl+m").as_deref(), Some("checkbox"));
        s.execute("edit.macros.editMacros", &json!({ "action": "rename", "name": "checkbox", "newName": "todo" })).unwrap();
        assert_eq!(s.macros.for_shortcut("Ctrl+Alt+M").as_deref(), Some("todo"));
        s.execute("editor.textEnd", &json!({})).unwrap();
        s.execute("editor.type", &json!({ "text": "four" })).unwrap();
        s.execute("edit.macros.playSavedMacros", &json!({ "name": "todo" })).unwrap();
        assert!(text(&s).ends_with("- [ ] four"), "{}", text(&s));
        s.execute("edit.macros.editMacros", &json!({ "action": "delete", "name": "todo" })).unwrap();
        assert!(s.execute("edit.macros.play", &json!({ "name": "todo" })).is_err());
        assert!(s.execute("edit.macros.editMacros", &json!({ "action": "explode", "name": "x" })).is_err());
    }

    #[test]
    fn playback_stops_at_a_failing_step_and_never_nests() {
        let (_tmp, mut s) = session();
        s.macros.last = Some(Macro {
            name: "bad".into(),
            steps: vec![
                MacroStep { command: "editor.type".into(), params: json!({ "text": "x" }) },
                MacroStep { command: "edit.macros.playBackLastMacro".into(), params: Value::Null },
                MacroStep { command: "no.such.command".into(), params: Value::Null },
                MacroStep { command: "editor.type".into(), params: json!({ "text": "y" }) },
            ],
            shortcut: None,
        });
        assert!(s.execute("edit.macros.playBackLastMacro", &json!({ "times": u64::MAX })).is_err());
        assert_eq!(text(&s), "xone\ntwo\nthree\n");
        assert!(!s.macros.playing);
        // Stopping with nothing recorded keeps the last macro.
        s.execute("edit.macros.startMacroRecording", &json!({})).unwrap();
        s.execute("edit.macros.startMacroRecording", &json!({})).unwrap();
        assert_eq!(s.macros.last.as_ref().unwrap().name, "bad");
        assert_eq!(normalize_shortcut("Shift+Cmd+M"), normalize_shortcut(" cmd + shift + m"));
    }
}
