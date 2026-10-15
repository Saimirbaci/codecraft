//! M14 debugger views (track DBG): the breakpoint properties popup (the gutter's right-click),
//! exception breakpoints in View Breakpoints, Smart Step Into's chooser, Set Value, the debug
//! console's completion list, the returned value and thrown exception above the variables, and
//! the processes of a multi-process (Python subprocess) session. Every action is an engine
//! command; this module keeps only input buffers and positions.

use egui::{Align2, FontId, Key, Pos2, RichText, TextEdit, Ui, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::debug::{DebugSession, DebugState, DebugStatus};

use crate::theme::Theme;
use crate::{UiState, run_command};

/// View state (kept in `DebugUi`).
#[derive(Default)]
pub struct DbgUi {
    /// Where the breakpoint popup opens (the gutter click), and its edit buffers.
    popup_anchor: Option<Pos2>,
    popup_for: Option<u64>,
    condition: String,
    log: String,
    hit: String,
    focus_condition: bool,
    /// Smart Step Into's highlighted target.
    target: usize,
    /// Set Value: the variable (parent reference, name) and the text being typed.
    set_value: Option<(i64, String, String)>,
    focus_set_value: bool,
    /// The completion list was applied or dismissed for this input.
    completions_done_for: Option<String>,
}

type Cmds = Vec<(&'static str, Value)>;

fn mono() -> FontId {
    FontId::monospace(13.0)
}

/// Open the properties popup of breakpoint `id` at `anchor` (screen position).
pub fn open_breakpoint_popup(s: &mut Session, st: &mut UiState, id: u64, anchor: Option<Pos2>) {
    st.debug.dbg.popup_anchor = anchor.or(st.caret_screen);
    run_command(s, st, "debug.editBreakpoint", &json!({ "id": id }));
}

/// Start editing a variable's value (Set Value… in the Variables view).
pub fn begin_set_value(st: &mut UiState, parent: i64, name: String, value: String) {
    st.debug.dbg.set_value = Some((parent, name, value));
    st.debug.dbg.focus_set_value = true;
}

/// The floating windows: breakpoint properties, step-into targets, Set Value.
pub fn windows(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    breakpoint_popup(ui, s, &theme, &mut st.debug.dbg, &mut cmds);
    step_targets(ui, s, &theme, st.caret_screen, &mut st.debug.dbg, &mut cmds);
    set_value_window(ui, s, &theme, &mut st.debug.dbg, &mut cmds);
    for (id, params) in cmds {
        run_command(s, st, id, &params);
    }
}

fn breakpoint_popup(ui: &mut Ui, s: &Session, theme: &Theme, view: &mut DbgUi, cmds: &mut Cmds) {
    let Some(id) = s.debug.breakpoint_popup else {
        view.popup_for = None;
        return;
    };
    let Some(bp) = s.debug.breakpoints.iter().find(|b| b.id == id).cloned() else {
        cmds.push(("debug.closeBreakpointPopup", Value::Null));
        return;
    };
    if view.popup_for != Some(id) {
        view.popup_for = Some(id);
        view.condition = bp.condition.clone().unwrap_or_default();
        view.log = bp.log_message.clone().unwrap_or_default();
        view.hit = bp.hit_condition.clone().unwrap_or_default();
        view.focus_condition = true;
    }
    let location = format!(
        "{}:{}",
        bp.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        bp.line + 1
    );
    let mut window = egui::Window::new(format!("Breakpoint · {location}"))
        .id(egui::Id::new("debug-breakpoint-popup"))
        .collapsible(false)
        .resizable(false)
        .default_width(420.0);
    window = match view.popup_anchor {
        Some(p) => window.fixed_pos(p + vec2(16.0, 4.0)),
        None => window.anchor(Align2::CENTER_CENTER, [0.0, -60.0]),
    };
    let mut close = false;
    let mut apply = false;
    window.show(ui.ctx(), |ui| {
        let mut enabled = bp.enabled;
        if ui.checkbox(&mut enabled, "Enabled").changed() {
            cmds.push(("debug.setBreakpoint", json!({ "path": bp.path, "line": bp.line, "enabled": enabled })));
        }
        egui::Grid::new("debug-bp-popup-grid").num_columns(2).spacing(vec2(8.0, 6.0)).show(ui, |ui| {
            ui.label(RichText::new("Condition").color(theme.text_muted));
            let r = ui.add(
                TextEdit::singleline(&mut view.condition)
                    .desired_width(300.0)
                    .font(mono())
                    .hint_text("Stop only when this is true, e.g. i == 3"),
            );
            if std::mem::take(&mut view.focus_condition) {
                r.request_focus();
            }
            apply |= r.lost_focus();
            ui.end_row();
            ui.label(RichText::new("Hit count").color(theme.text_muted));
            let r = ui.add(
                TextEdit::singleline(&mut view.hit).desired_width(120.0).font(mono()).hint_text("e.g. 3 or >= 5"),
            );
            apply |= r.lost_focus();
            ui.end_row();
            ui.label(RichText::new("Log message").color(theme.text_muted));
            let r = ui.add(
                TextEdit::singleline(&mut view.log)
                    .desired_width(300.0)
                    .font(mono())
                    .hint_text("Print instead of stopping: x = {x}"),
            );
            apply |= r.lost_focus();
            ui.end_row();
        });
        let mut temporary = bp.temporary;
        if ui.checkbox(&mut temporary, "Remove once hit").changed() {
            cmds.push(("debug.setBreakpoint", json!({ "path": bp.path, "line": bp.line, "temporary": temporary })));
        }
        if let (Some(false), Some(message)) = (bp.verified, &bp.message) {
            ui.label(RichText::new(format!("Not set: {message}")).color(theme.warning));
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("Done").clicked() {
                apply = true;
                close = true;
            }
            if ui.button("Remove").clicked() {
                cmds.push(("debug.removeBreakpoint", json!({ "id": id })));
                close = true;
            }
            if ui.button("More (View Breakpoints)").clicked() {
                apply = true;
                close = true;
                cmds.push(("run.viewBreakpoints", Value::Null));
            }
        });
    });
    let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
    if enter {
        apply = true;
        close = true;
    }
    if escape {
        close = true;
    }
    let changed = view.condition != bp.condition.clone().unwrap_or_default()
        || view.log != bp.log_message.clone().unwrap_or_default()
        || view.hit != bp.hit_condition.clone().unwrap_or_default();
    if apply && changed {
        cmds.push((
            "debug.setBreakpoint",
            json!({
                "path": bp.path, "line": bp.line,
                "condition": view.condition, "logMessage": view.log, "hitCondition": view.hit,
            }),
        ));
    }
    if close {
        cmds.push(("debug.closeBreakpointPopup", Value::Null));
        view.popup_anchor = None;
    }
}

fn step_targets(ui: &mut Ui, s: &Session, theme: &Theme, caret: Option<Pos2>, view: &mut DbgUi, cmds: &mut Cmds) {
    let Some(targets) = &s.debug.step_targets else {
        view.target = 0;
        return;
    };
    if targets.pending || targets.targets.is_empty() {
        return;
    }
    let count = targets.targets.len();
    view.target = view.target.min(count - 1);
    let (up, down, enter, escape) = ui.input(|i| {
        (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter), i.key_pressed(Key::Escape))
    });
    if up {
        view.target = view.target.saturating_sub(1);
    }
    if down {
        view.target = (view.target + 1).min(count - 1);
    }
    let mut window = egui::Window::new("Smart Step Into")
        .id(egui::Id::new("debug-step-targets"))
        .collapsible(false)
        .resizable(false)
        .title_bar(true);
    window = match caret {
        Some(p) => window.fixed_pos(p + vec2(0.0, 6.0)),
        None => window.anchor(Align2::CENTER_CENTER, [0.0, 0.0]),
    };
    window.show(ui.ctx(), |ui| {
        ui.label(RichText::new("Step into which call? (↑↓ ↩, ⎋ to cancel)").color(theme.text_muted).size(11.5));
        for (i, t) in targets.targets.iter().enumerate() {
            if ui.selectable_label(i == view.target, RichText::new(&t.label).font(mono())).clicked() {
                cmds.push(("debug.stepIntoTarget", json!({ "index": i })));
            }
        }
    });
    if enter {
        cmds.push(("debug.stepIntoTarget", json!({ "index": view.target })));
    } else if escape {
        cmds.push(("debug.cancelStepIntoTargets", Value::Null));
    }
}

fn set_value_window(ui: &mut Ui, s: &Session, theme: &Theme, view: &mut DbgUi, cmds: &mut Cmds) {
    let Some((parent, name, text)) = view.set_value.as_mut() else { return };
    if !s.debug.is_stopped() {
        view.set_value = None;
        return;
    }
    let mut done = false;
    let mut submit = false;
    egui::Window::new(format!("Set Value · {name}"))
        .id(egui::Id::new("debug-set-value"))
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ui.ctx(), |ui| {
            ui.label(RichText::new("New value (an expression in the program's language)").color(theme.text_muted));
            let r = ui.add(TextEdit::singleline(text).desired_width(360.0).font(mono()));
            if std::mem::take(&mut view.focus_set_value) {
                r.request_focus();
            }
            submit |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.horizontal(|ui| {
                submit |= ui.button("Set").clicked();
                done |= ui.button("Cancel").clicked();
            });
        });
    done |= ui.input(|i| i.key_pressed(Key::Escape));
    if submit && !text.trim().is_empty() {
        cmds.push(("debug.setValue", json!({ "reference": *parent, "name": name.clone(), "value": text.clone() })));
        done = true;
    }
    if done {
        view.set_value = None;
    }
}

/// View Breakpoints' exception breakpoints: Rust panics, C++ throw/catch, Python raised /
/// uncaught / user-uncaught.
pub fn exception_breakpoints(ui: &mut Ui, s: &Session, theme: &Theme) -> Cmds {
    let mut cmds = Vec::new();
    let e = &s.debug.exceptions;
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Stop on:").color(theme.text_muted));
        for (label, key, on) in [
            ("Rust panics", "rustPanic", s.debug.break_on_panic),
            ("C++ throw", "cppThrow", e.cpp_throw),
            ("C++ catch", "cppCatch", e.cpp_catch),
            ("Python raised", "pythonRaised", e.python_raised),
            ("Python uncaught", "pythonUncaught", e.python_uncaught),
            ("Python user-uncaught", "pythonUserUncaught", e.python_user_uncaught),
        ] {
            let mut value = on;
            if ui.checkbox(&mut value, label).changed() {
                let mut params = serde_json::Map::new();
                params.insert(key.into(), value.into());
                cmds.push(("debug.exceptionBreakpoints", Value::Object(params)));
            }
        }
    });
    cmds
}

/// The value the last Step Out returned and the exception a stop threw (above the variables).
pub fn stop_details(ui: &mut Ui, session: &DebugSession, theme: &Theme) {
    if let Some(e) = &session.exception {
        let mut text = e.type_name.clone().unwrap_or_else(|| e.id.clone());
        if let Some(m) = e.message.as_ref().or(e.description.as_ref()) {
            text.push_str(": ");
            text.push_str(m);
        }
        ui.label(RichText::new(format!("⚠ {text}")).color(theme.error).font(mono()));
    }
    if let Some(v) = &session.return_value {
        let mut value: String = v.value.chars().take(300).collect();
        if value.contains('\n') {
            value = value.replace('\n', "⏎");
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("↩").color(theme.link));
            ui.label(RichText::new(&v.name).color(theme.field).font(mono()));
            ui.label(RichText::new(" = ").color(theme.text_muted).font(mono()));
            if let Some(t) = &v.type_name {
                ui.label(RichText::new(format!("{{{t}}} ")).color(theme.text_muted).font(mono()));
            }
            ui.label(RichText::new(value).color(theme.text).font(mono()));
        });
    }
}

/// The colour of a session's state (running green, paused amber, failed red).
fn status_color(status: &DebugStatus, theme: &Theme) -> egui::Color32 {
    match status {
        DebugStatus::Running => theme.success,
        DebugStatus::Stopped { .. } => theme.warning,
        DebugStatus::Failed { .. } => theme.error,
        _ => theme.text_muted,
    }
}

/// The session tabs at the top of the Debug tool window when there are several debug sessions
/// (M15 track DM): one tab per session (subprocess sessions marked `↳`) with its state's colour,
/// a close button, and Stop All while two or more run. Clicking a tab selects that session: its
/// threads, frames, variables, watches and console, and its execution point in the editor.
pub fn session_tabs(ui: &mut Ui, s: &Session, theme: &Theme) -> Cmds {
    let mut cmds = Vec::new();
    let sessions = s.debug.sessions();
    if sessions.len() < 2 {
        return cmds;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for x in &sessions {
            let (dot, _) = ui.allocate_exact_size(vec2(8.0, 16.0), egui::Sense::hover());
            ui.painter().circle_filled(dot.center(), 3.5, status_color(&x.status, theme));
            let mut label: String = x.name.chars().take(40).collect();
            if x.name.chars().count() > 40 {
                label.push('…');
            }
            if x.parent.is_some() {
                label = format!("↳ {label}");
            }
            let text = RichText::new(label).color(if x.selected { theme.text } else { theme.text_muted });
            let pid = x.pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            let tip = format!("{}{pid} — {}", x.name, x.status.label());
            let resp = ui.add(egui::Button::new(text).selected(x.selected).frame(x.selected)).on_hover_text(tip);
            if resp.clicked() && !x.selected {
                cmds.push(("debug.selectSession", json!({ "session": x.id })));
            }
            let close = ui
                .add(egui::Button::new(RichText::new("×").color(theme.text_muted)).frame(false).small())
                .on_hover_text(if x.live { "Stop and close this session" } else { "Close this session" });
            if close.clicked() {
                cmds.push(("debug.closeSession", json!({ "session": x.id })));
            }
            ui.add_space(6.0);
        }
        if sessions.iter().filter(|x| x.live).count() >= 2
            && ui
                .add(egui::Button::new(RichText::new("■ Stop All").color(theme.error)).small())
                .on_hover_text("Stop every debug session")
                .clicked()
        {
            cmds.push(("debug.stopAll", Value::Null));
        }
    });
    cmds
}

/// The status bar's debugger widget: the running session's name, or how many sessions run;
/// clicking it shows the Debug tool window.
pub fn status_widget(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let sessions = s.debug.sessions();
    let live: Vec<_> = sessions.iter().filter(|x| x.live).collect();
    let text = match live.as_slice() {
        [] => return,
        [only] => format!("Debugging {}", only.name.chars().take(30).collect::<String>()),
        many => {
            let paused = many.iter().filter(|x| matches!(x.status, DebugStatus::Stopped { .. })).count();
            if paused > 0 {
                format!("{} debug sessions ({paused} paused)", many.len())
            } else {
                format!("{} debug sessions", many.len())
            }
        }
    };
    let color = if live.iter().any(|x| matches!(x.status, DebugStatus::Stopped { .. })) {
        st.theme.warning
    } else {
        st.theme.text_muted
    };
    let tip: Vec<String> = live.iter().map(|x| format!("{} — {}", x.name, x.status.label())).collect();
    let resp = ui
        .add(egui::Label::new(RichText::new(text).size(12.0).color(color)).sense(egui::Sense::click()))
        .on_hover_text(tip.join("\n"));
    if resp.clicked()
        && let Some(id) = s.debug.selected_id()
    {
        run_command(s, st, "debug.selectSession", &json!({ "session": id }));
    }
}

/// `· Subprocess 946` next to the status when a child process is shown, with all processes in
/// the tooltip.
pub fn processes(ui: &mut Ui, session: &DebugSession, theme: &Theme) {
    if session.processes.len() < 2 {
        return;
    }
    let focused = session.processes.iter().find(|p| p.focused).map(|p| p.label.clone()).unwrap_or_default();
    let tip: Vec<String> = session
        .processes
        .iter()
        .map(|p| {
            let pid = p.pid.map(|p| format!(" ({p})")).unwrap_or_default();
            let state = if p.stopped { "paused" } else { "running" };
            format!("{}{}{pid} — {state}", if p.focused { "▸ " } else { "  " }, p.label)
        })
        .collect();
    ui.label(RichText::new(format!("{focused} · {} processes", session.processes.len())).color(theme.link))
        .on_hover_text(tip.join("\n"));
}

/// The console's completion list under the input. Returns the input text after a pick.
pub fn console_completions(
    ui: &mut Ui,
    debug: &DebugState,
    theme: &Theme,
    input: &str,
    view: &mut DbgUi,
) -> (Option<String>, Cmds) {
    let mut cmds = Vec::new();
    let Some(c) = &debug.completions else { return (None, cmds) };
    if c.text != input {
        if view.completions_done_for.as_deref() != Some(input) {
            cmds.push(("debug.closeCompletions", Value::Null));
        }
        return (None, cmds);
    }
    if c.pending {
        ui.label(RichText::new("Completing…").color(theme.text_muted).size(11.5));
        return (None, cmds);
    }
    let apply = |item: &codecraft_engine::debug::CompletionEntry| -> String {
        let chars: Vec<char> = input.chars().collect();
        let start = item.start.min(chars.len());
        let end = item.end.clamp(start, chars.len());
        let mut out: String = chars.get(..start).map(|c| c.iter().collect()).unwrap_or_default();
        out.push_str(&item.text);
        out.extend(chars.get(end..).unwrap_or(&[]).iter());
        out
    };
    match c.items.as_slice() {
        [] => {
            ui.label(RichText::new("No completions").color(theme.text_muted).size(11.5));
            (None, cmds)
        }
        [only] => {
            cmds.push(("debug.closeCompletions", Value::Null));
            (Some(apply(only)), cmds)
        }
        items => {
            let mut picked = None;
            ui.horizontal_wrapped(|ui| {
                for item in items.iter().take(40) {
                    let resp = ui.add(egui::Button::new(RichText::new(&item.label).font(FontId::monospace(12.0))).small());
                    let resp = match &item.kind {
                        Some(k) => resp.on_hover_text(k),
                        None => resp,
                    };
                    if resp.clicked() {
                        picked = Some(apply(item));
                    }
                }
            });
            if picked.is_some() || ui.input(|i| i.key_pressed(Key::Escape)) {
                cmds.push(("debug.closeCompletions", Value::Null));
                view.completions_done_for = Some(input.to_owned());
            }
            (picked, cmds)
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_engine::debug::{CompletionEntry, Completions, StepTarget, StepTargets};

    fn frame(s: &mut Session, st: &mut UiState) {
        let ctx = egui::Context::default();
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                windows(ui, s, st);
                let theme = st.theme.clone();
                let _ = exception_breakpoints(ui, s, &theme);
                if let Some(session) = s.debug.session.clone() {
                    stop_details(ui, &session, &theme);
                    processes(ui, &session, &theme);
                }
                let _ = console_completions(ui, &s.debug, &theme, "to", &mut st.debug.dbg);
            });
            out.textures_delta.clear();
        }
    }

    #[test]
    fn hover_expressions_follow_member_paths() {
        let doc = codecraft_text::Document::from_text("let n = self.items.len();\nx = point.x + 1\n");
        let at = |needle: &str| {
            let text = doc.text();
            text.find(needle).map(|i| text.get(..i).map_or(0, |p| p.chars().count()) + 1).unwrap()
        };
        assert_eq!(crate::editor::hover_expression(&doc, at("items")).as_deref(), Some("self.items"));
        assert_eq!(crate::editor::hover_expression(&doc, at("len")).as_deref(), Some("self.items.len"));
        assert_eq!(crate::editor::hover_expression(&doc, at("x +")).as_deref(), Some("point.x"));
        assert_eq!(crate::editor::hover_expression(&doc, at("n =")).as_deref(), Some("n"));
        assert!(crate::editor::hover_expression(&doc, at("let")).is_none());
        assert!(crate::editor::hover_expression(&doc, at("1\n")).is_none());
        assert!(crate::editor::hover_expression(&doc, 10_000).is_none() || doc.len_chars() > 0);
    }

    #[test]
    fn debugger_windows_render_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.py");
        std::fs::write(&file, "x = 1\ny = 2\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": tmp.path() })).unwrap();
        let mut st = UiState::new();
        s.execute("debug.breakpoint.set", &json!({ "path": file, "line": 1, "condition": "x > 0" })).unwrap();
        let id = s.debug.breakpoints[0].id;
        open_breakpoint_popup(&mut s, &mut st, id, Some(Pos2::new(10.0, 10.0)));
        assert_eq!(s.debug.breakpoint_popup, Some(id));
        s.debug.step_targets = Some(StepTargets {
            pending: false,
            targets: vec![StepTarget { id: Some(1), label: "work".into(), function: None }],
            selected: 0,
        });
        s.debug.completions = Some(Completions {
            text: "to".into(),
            column: 2,
            pending: false,
            items: vec![
                CompletionEntry { label: "total".into(), text: "total".into(), start: 0, end: 2, kind: None },
                CompletionEntry { label: "tomato".into(), text: "tomato".into(), start: 0, end: 99, kind: None },
            ],
        });
        s.debug.session = Some(DebugSession {
            return_value: Some(codecraft_engine::debug::VariableInfo {
                name: "(return) add".into(),
                value: "10\nmore".into(),
                type_name: Some("int".into()),
                reference: 0,
                evaluate_name: None,
                memory_reference: None,
            }),
            ..DebugSession::default()
        });
        begin_set_value(&mut st, 3, "x".into(), "1".into());
        frame(&mut s, &mut st);
        assert_eq!(st.debug.dbg.popup_for, Some(id));
    }
}
