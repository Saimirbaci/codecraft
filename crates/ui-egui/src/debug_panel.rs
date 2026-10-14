//! The Debug tool window: toolbar, Threads & Variables (frames, watches, variables tree,
//! Evaluate field), the debug Console with its REPL, and the Breakpoints list. OWNED BY the
//! debugger track. Every action is an engine command; this module only paints and keeps input
//! buffers. The editor's gutter breakpoints and execution line live in `editor.rs`.

use std::path::Path;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Color32, CornerRadius, FontId, Frame, Key, Layout, Margin, Pos2, Rect, Response, RichText, ScrollArea,
    Sense, Shape, Stroke, TextEdit, Ui, pos2, vec2,
};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::debug::{
    self as dbg, Breakpoint, DebugConsoleKind, DebugSession, DebugState, DebugStatus, DebugTab, VariableInfo,
};

use crate::debug_views_ui::{self, ViewsUi};
use crate::theme::Theme;
use crate::{UiState, keymap, run_command};

const ROW_H: f32 = 20.0;
const INDENT: f32 = 14.0;
const MAX_DEPTH: usize = 40;

/// View state of the Debug tool window (kept in [`UiState`]).
#[derive(Default)]
pub struct DebugUi {
    /// The Evaluate expression field above the variables.
    pub evaluate: String,
    focus_evaluate: bool,
    /// The console's input line and the position in the engine's history while browsing it.
    pub console_input: String,
    history_pos: Option<usize>,
    /// A watch being typed (the "+" row).
    pub new_watch: Option<String>,
    /// The breakpoint selected in the Breakpoints tab, and its edit buffers.
    pub selected_breakpoint: Option<u64>,
    edit_condition: String,
    edit_log: String,
    edit_hit: String,
    edit_for: Option<u64>,
    console_follow: bool,
    console_follow_set: bool,
    /// Scroll the Attach to Process list to the selection (after ↑/↓).
    attach_scroll: bool,
    /// The Memory, Disassembly and Registers tabs and the debug forms.
    pub views: ViewsUi,
}

/// What the user did this frame; applied after painting (the tree is drawn from `&Session`).
enum Action {
    Command(&'static str, Value),
    Copy(String),
    SelectBreakpoint(u64),
}

pub fn show(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    if let Some(prefill) = s.debug.focus_evaluate.take() {
        if !prefill.is_empty() {
            st.debug.evaluate = prefill;
        }
        st.debug.focus_evaluate = true;
    }
    let mut actions = Vec::new();
    header(ui, s, &theme, &mut actions);
    ui.add_space(2.0);
    let mut cmds: Vec<debug_views_ui::Cmd> = Vec::new();
    if s.debug.choosing {
        chooser(ui, s, &theme, &mut actions);
        debug_views_ui::chooser_forms(ui, s, &theme, &mut st.debug.views, &mut cmds);
    } else {
        match s.debug.tab {
            DebugTab::Variables => threads_and_variables(ui, &s.debug, &theme, &mut st.debug, &mut actions),
            DebugTab::Console => console(ui, &s.debug, &theme, &mut st.debug, &mut actions),
            DebugTab::Breakpoints => {
                debug_views_ui::watchpoints(ui, s, &theme, &mut cmds);
                breakpoints(ui, s, &theme, &mut st.debug, &mut actions);
            }
            DebugTab::Memory => debug_views_ui::memory(ui, s, &theme, &mut st.debug.views, &mut cmds),
            DebugTab::Disassembly => debug_views_ui::disassembly(ui, s, &theme, &mut st.debug.views, &mut cmds),
            DebugTab::Registers => debug_views_ui::registers(ui, s, &theme, &mut cmds),
        }
    }
    actions.extend(cmds.into_iter().map(|(id, params)| Action::Command(id, params)));
    for action in actions {
        match action {
            Action::Command(id, params) => run_command(s, st, id, &params),
            Action::Copy(text) => ui.ctx().copy_text(text),
            Action::SelectBreakpoint(id) => {
                st.debug.selected_breakpoint = Some(id);
                s.debug.tab = DebugTab::Breakpoints;
            }
        }
    }
}

/// Ask the Breakpoints tab to select a breakpoint (the editor's gutter context click).
pub fn select_breakpoint(s: &mut Session, st: &mut UiState, id: u64) {
    st.debug.selected_breakpoint = Some(id);
    run_command(s, st, "debug.showTab", &json!({ "tab": "breakpoints" }));
}

// ---------------------------------------------------------------- header: tabs, toolbar, status

fn header(ui: &mut Ui, s: &Session, theme: &Theme, actions: &mut Vec<Action>) {
    let debug = &s.debug;
    ui.horizontal(|ui| {
        let bp_count = debug.breakpoints.len();
        for (tab, label, key) in [
            (DebugTab::Variables, "Threads & Variables".to_owned(), "variables"),
            (DebugTab::Console, "Console".to_owned(), "console"),
            (
                DebugTab::Breakpoints,
                if bp_count > 0 { format!("Breakpoints {bp_count}") } else { "Breakpoints".to_owned() },
                "breakpoints",
            ),
            (DebugTab::Memory, "Memory".to_owned(), "memory"),
            (DebugTab::Disassembly, "Disassembly".to_owned(), "disassembly"),
            (DebugTab::Registers, "Registers".to_owned(), "registers"),
        ] {
            let selected = debug.tab == tab && !debug.choosing;
            let text = RichText::new(label).color(if selected { theme.text } else { theme.text_muted });
            if ui.add(egui::Button::new(text).selected(selected).frame(selected)).clicked() {
                actions.push(Action::Command("debug.showTab", json!({ "tab": key })));
            }
        }
        ui.add_space(6.0);
        toolbar(ui, s, theme, actions);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| status_label(ui, debug, theme));
    });
}

fn shortcut_tip(label: &str, id: &str) -> String {
    match keymap::shortcut_label(id) {
        Some(sc) => format!("{label} ({sc})"),
        None => label.to_owned(),
    }
}

fn toolbar(ui: &mut Ui, s: &Session, theme: &Theme, actions: &mut Vec<Action>) {
    let enabled = |id: &str| s.enabled(id).is_ok();
    let buttons: [(Icon, &str, &'static str, bool); 10] = [
        (Icon::Rerun, "Rerun", "debug.rerun", enabled("debug.rerun")),
        (
            Icon::Resume,
            "Resume Program",
            "run.debuggingActions.resumeProgram",
            enabled("run.debuggingActions.resumeProgram"),
        ),
        (
            Icon::Pause,
            "Pause Program",
            "run.debuggingActions.pauseAllPrograms",
            enabled("run.debuggingActions.pauseAllPrograms"),
        ),
        (Icon::Stop, "Stop", "debug.stop", enabled("debug.stop")),
        (Icon::StepOver, "Step Over", "run.debuggingActions.stepOver", enabled("run.debuggingActions.stepOver")),
        (Icon::StepInto, "Step Into", "run.debuggingActions.stepInto", enabled("run.debuggingActions.stepInto")),
        (
            Icon::ForceStepInto,
            "Force Step Into",
            "run.debuggingActions.forceStepInto",
            enabled("run.debuggingActions.forceStepInto"),
        ),
        (Icon::StepOut, "Step Out", "run.debuggingActions.stepOut", enabled("run.debuggingActions.stepOut")),
        (
            Icon::RunToCursor,
            "Run to Cursor",
            "run.debuggingActions.runToCursor",
            enabled("run.debuggingActions.runToCursor") && s.ws.active().is_some(),
        ),
        (
            Icon::Evaluate,
            "Evaluate Expression",
            "run.debuggingActions.evaluateExpression",
            enabled("run.debuggingActions.evaluateExpression"),
        ),
    ];
    for (i, (icon, label, id, on)) in buttons.into_iter().enumerate() {
        if i == 4 || i == 8 {
            ui.add_space(4.0);
        }
        if icon_button(ui, theme, icon, on, &shortcut_tip(label, id)).clicked() {
            actions.push(Action::Command(id, Value::Null));
        }
    }
    // Reset Frame (track CL): drop the selected frame and return to its caller.
    let reset = "run.debuggingActions.resetFrame";
    let resp = ui
        .add_enabled(enabled(reset), egui::Button::new(RichText::new("⤺").color(theme.text)).frame(false))
        .on_hover_text(shortcut_tip("Reset Frame", reset));
    if resp.clicked() {
        actions.push(Action::Command(reset, Value::Null));
    }
}

fn status_label(ui: &mut Ui, debug: &DebugState, theme: &Theme) {
    let Some(session) = &debug.session else {
        ui.label(RichText::new("Not debugging").color(theme.text_muted));
        return;
    };
    let status = session.status();
    let color = match &status {
        DebugStatus::Running => theme.success,
        DebugStatus::Stopped { .. } => theme.warning,
        DebugStatus::Failed { .. } => theme.error,
        _ => theme.text_muted,
    };
    let mut text = status.label();
    if text.chars().count() > 80 {
        text = text.chars().take(80).collect::<String>() + "…";
    }
    ui.label(RichText::new(text).color(color)).on_hover_text(status.label());
    if matches!(status, DebugStatus::Building | DebugStatus::Starting) {
        ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
    }
    if !session.name.is_empty() {
        ui.label(RichText::new(&session.name).strong());
    }
}

// ---------------------------------------------------------------- icons

#[derive(Clone, Copy, PartialEq, Eq)]
enum Icon {
    Rerun,
    Resume,
    Pause,
    Stop,
    StepOver,
    StepInto,
    ForceStepInto,
    StepOut,
    RunToCursor,
    Evaluate,
}

fn icon_button(ui: &mut Ui, theme: &Theme, icon: Icon, enabled: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 22.0), if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(4), theme.hover);
    }
    paint_icon(ui.painter(), rect.center(), icon, theme, enabled);
    let resp = resp.on_hover_text(tip);
    if enabled { resp } else { resp.on_hover_cursor(egui::CursorIcon::Default) }
}

fn arrow_head(painter: &egui::Painter, tip: Pos2, dir: egui::Vec2, color: Color32) {
    let dir = dir.normalized();
    let side = vec2(-dir.y, dir.x);
    let back = tip - dir * 4.5;
    painter.add(Shape::convex_polygon(vec![tip, back + side * 3.2, back - side * 3.2], color, Stroke::NONE));
}

fn arc(center: Pos2, radius: f32, from_deg: f32, to_deg: f32) -> Vec<Pos2> {
    let steps = 14;
    (0..=steps)
        .map(|i| {
            let t = (from_deg + (to_deg - from_deg) * i as f32 / steps as f32).to_radians();
            center + vec2(t.cos(), -t.sin()) * radius
        })
        .collect()
}

fn paint_icon(painter: &egui::Painter, c: Pos2, icon: Icon, theme: &Theme, enabled: bool) {
    let pick = |color: Color32| if enabled { color } else { theme.text_disabled };
    let stroke = |color: Color32| Stroke::new(1.6, pick(color));
    match icon {
        Icon::Resume => {
            let green = pick(theme.success);
            painter.rect_filled(Rect::from_min_size(pos2(c.x - 6.0, c.y - 5.5), vec2(2.0, 11.0)), 0.0, green);
            let tri = vec![pos2(c.x - 2.5, c.y - 6.0), pos2(c.x + 6.5, c.y), pos2(c.x - 2.5, c.y + 6.0)];
            painter.add(Shape::convex_polygon(tri, green, Stroke::NONE));
        }
        Icon::Pause => {
            let color = pick(theme.text);
            painter.rect_filled(Rect::from_min_size(pos2(c.x - 5.0, c.y - 5.5), vec2(3.0, 11.0)), 1.0, color);
            painter.rect_filled(Rect::from_min_size(pos2(c.x + 2.0, c.y - 5.5), vec2(3.0, 11.0)), 1.0, color);
        }
        Icon::Stop => {
            painter.rect_filled(Rect::from_center_size(c, vec2(10.0, 10.0)), 1.5, pick(theme.error));
        }
        Icon::Rerun => {
            let pts = arc(c, 5.5, 60.0, 330.0);
            let (Some(&end), Some(&before)) = (pts.last(), pts.get(pts.len().saturating_sub(3))) else { return };
            painter.add(Shape::line(pts, stroke(theme.success)));
            arrow_head(painter, end + (end - before).normalized() * 2.0, end - before, pick(theme.success));
        }
        Icon::StepOver => {
            let blue = theme.link;
            let pts = arc(pos2(c.x, c.y + 1.0), 5.5, 180.0, 20.0);
            let (Some(&end), Some(&before)) = (pts.last(), pts.get(pts.len().saturating_sub(3))) else { return };
            painter.add(Shape::line(pts, stroke(blue)));
            arrow_head(painter, end + (end - before).normalized() * 2.0, end - before, pick(blue));
            painter.circle_filled(pos2(c.x, c.y + 5.5), 1.6, pick(theme.text_muted));
        }
        Icon::StepInto | Icon::ForceStepInto => {
            let color = if icon == Icon::StepInto { theme.link } else { theme.error };
            painter.line_segment([pos2(c.x, c.y - 6.5), pos2(c.x, c.y + 1.0)], stroke(color));
            arrow_head(painter, pos2(c.x, c.y + 4.0), vec2(0.0, 1.0), pick(color));
            painter.line_segment([pos2(c.x - 5.0, c.y + 6.5), pos2(c.x + 5.0, c.y + 6.5)], stroke(theme.text_muted));
        }
        Icon::StepOut => {
            painter.line_segment([pos2(c.x, c.y + 4.0), pos2(c.x, c.y - 3.0)], stroke(theme.link));
            arrow_head(painter, pos2(c.x, c.y - 6.5), vec2(0.0, -1.0), pick(theme.link));
            painter.line_segment([pos2(c.x - 5.0, c.y + 6.5), pos2(c.x + 5.0, c.y + 6.5)], stroke(theme.text_muted));
        }
        Icon::RunToCursor => {
            painter.line_segment([pos2(c.x - 6.5, c.y), pos2(c.x + 0.5, c.y)], stroke(theme.link));
            arrow_head(painter, pos2(c.x + 3.5, c.y), vec2(1.0, 0.0), pick(theme.link));
            painter.line_segment([pos2(c.x + 6.0, c.y - 6.0), pos2(c.x + 6.0, c.y + 6.0)], stroke(theme.text));
        }
        Icon::Evaluate => {
            let r = Rect::from_center_size(c, vec2(11.0, 12.0));
            painter.rect_stroke(r, 2.0, Stroke::new(1.3, pick(theme.text)), egui::StrokeKind::Inside);
            painter.line_segment([pos2(c.x - 2.5, c.y - 1.5), pos2(c.x + 2.5, c.y - 1.5)], stroke(theme.link));
            painter.line_segment([pos2(c.x - 2.5, c.y + 1.5), pos2(c.x + 2.5, c.y + 1.5)], stroke(theme.link));
        }
    }
}

// ---------------------------------------------------------------- shared row helpers

fn row(ui: &mut Ui, theme: &Theme, selected: bool) -> (Rect, Response) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(10.0), ROW_H), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(3), theme.list_selection);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(3), theme.hover);
    }
    (rect, resp)
}

fn text_job(parts: &[(&str, Color32)], font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.wrap.max_rows = 1;
    for (text, color) in parts {
        job.append(text, 0.0, TextFormat { font_id: font.clone(), color: *color, ..Default::default() });
    }
    job
}

fn paint_job(ui: &Ui, rect: Rect, x: f32, job: LayoutJob) {
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().with_clip_rect(rect).galley(pos2(x, y), galley, Color32::PLACEHOLDER);
}

/// Value colour like the editor: strings, numbers, keywords.
fn value_color(theme: &Theme, value: &str) -> Color32 {
    let v = value.trim();
    if v.starts_with('"') || v.starts_with('\'') || v.starts_with("b\"") {
        theme.string
    } else if v.parse::<f64>().is_ok() || v.starts_with("0x") {
        theme.number
    } else if matches!(v, "true" | "false" | "None" | "null" | "nil" | "True" | "False" | "undefined") {
        theme.keyword
    } else {
        theme.text_editor
    }
}

/// `alloc::vec::Vec<int, alloc::alloc::Global>` → `Vec<int>`: module paths and the default
/// allocator are noise in the Variables tree (the full type is in the tooltip).
fn short_type(full: &str) -> String {
    let mut out = String::with_capacity(full.len());
    let mut segment_start = 0;
    let mut chars = full.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c == ':' && chars.peek().is_some_and(|(_, n)| *n == ':') {
            chars.next();
            out.truncate(segment_start);
            continue;
        }
        out.push(c);
        if !(c.is_alphanumeric() || c == '_') {
            segment_start = out.len();
        }
    }
    out.replace(", Global>", ">")
}

fn file_label(path: &Path, line: usize) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{name}:{}", line.saturating_add(1))
}

fn empty_hint(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).color(theme.text_muted));
}

// ---------------------------------------------------------------- Threads & Variables

fn threads_and_variables(
    ui: &mut Ui,
    debug: &DebugState,
    theme: &Theme,
    view: &mut DebugUi,
    actions: &mut Vec<Action>,
) {
    let Some(session) = &debug.session else {
        let shortcut = keymap::shortcut_label("run.debug").unwrap_or_default();
        empty_hint(
            ui,
            theme,
            &format!(
                "Not debugging. Start with Run › Debug ({shortcut}), Debug Context Configuration (⌃⇧D) at a test or \
                 main function, or a Python or Zig file. Click the line numbers to toggle breakpoints."
            ),
        );
        watches_only(ui, debug, theme, view, actions);
        return;
    };
    let width = ui.available_width();
    egui::Panel::left("debug-frames")
        .resizable(true)
        .default_size((width * 0.36).clamp(160.0, 520.0))
        .size_range(140.0..=(width * 0.8).max(160.0))
        .frame(Frame::new().inner_margin(Margin { right: 6, ..Margin::ZERO }))
        .show(ui, |ui| frames(ui, session, theme, actions));
    variables(ui, debug, session, theme, view, actions);
}

fn frames(ui: &mut Ui, session: &DebugSession, theme: &Theme, actions: &mut Vec<Action>) {
    let current = session.thread.and_then(|id| session.threads.iter().find(|t| t.id == id));
    let thread_text = match current {
        Some(t) => format!("{} ({})", if t.name.is_empty() { "thread" } else { &t.name }, t.id),
        None if session.is_stopped() => "Threads".to_owned(),
        None => "Running…".to_owned(),
    };
    ui.add_enabled_ui(session.is_stopped() && !session.threads.is_empty(), |ui| {
        egui::ComboBox::from_id_salt("debug-thread")
            .width(ui.available_width().min(320.0))
            .selected_text(thread_text)
            .show_ui(ui, |ui| {
                for t in &session.threads {
                    let label =
                        if t.name.is_empty() { format!("thread {}", t.id) } else { format!("{} ({})", t.name, t.id) };
                    if ui.selectable_label(Some(t.id) == session.thread, label).clicked() {
                        actions.push(Action::Command("debug.selectThread", json!({ "id": t.id })));
                    }
                }
            });
    });
    ui.add_space(2.0);
    if session.frames.is_empty() {
        let text = if session.is_stopped() { "Collecting frames…" } else { "Frames are available while paused." };
        ui.label(RichText::new(text).color(theme.text_muted));
        return;
    }
    let font = FontId::proportional(13.0);
    ScrollArea::vertical().id_salt("debug-frames-scroll").auto_shrink([false, false]).show_rows(
        ui,
        ROW_H,
        session.frames.len(),
        |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, f) in session.frames.iter().enumerate().skip(range.start).take(range.len()) {
                let selected = i == session.frame;
                let (rect, resp) = row(ui, theme, selected);
                let name_color = if f.subtle { theme.text_muted } else { theme.text };
                let location = f.path.as_deref().map(|p| file_label(p, f.line)).unwrap_or_default();
                let job = text_job(&[(&f.name, name_color), ("  ", name_color), (&location, theme.text_muted)], &font);
                paint_job(ui, rect, rect.left() + 18.0, job);
                if selected {
                    paint_exec_arrow(ui.painter(), pos2(rect.left() + 8.0, rect.center().y), theme);
                }
                let resp = match &f.path {
                    Some(p) => resp.on_hover_text(format!("{}:{}", p.display(), f.line + 1)),
                    None => resp,
                };
                if resp.clicked() {
                    actions.push(Action::Command("debug.selectFrame", json!({ "index": i })));
                }
            }
        },
    );
}

/// The execution-point arrow (also used by the editor gutter).
pub fn paint_exec_arrow(painter: &egui::Painter, c: Pos2, theme: &Theme) {
    let shaft = Rect::from_min_max(pos2(c.x - 5.0, c.y - 2.5), pos2(c.x + 1.5, c.y + 2.5));
    painter.rect_filled(shaft, 0.0, theme.accent);
    painter.add(Shape::convex_polygon(
        vec![pos2(c.x + 1.0, c.y - 6.0), pos2(c.x + 6.5, c.y), pos2(c.x + 1.0, c.y + 6.0)],
        theme.accent,
        Stroke::NONE,
    ));
}

fn evaluate_field(ui: &mut Ui, debug: &DebugState, theme: &Theme, view: &mut DebugUi, actions: &mut Vec<Action>) {
    let can_eval = debug.is_stopped();
    let id = egui::Id::new("debug-evaluate-field");
    let resp = ui.add(
        TextEdit::singleline(&mut view.evaluate)
            .id(id)
            .desired_width(f32::INFINITY)
            .font(FontId::monospace(13.0))
            .hint_text(RichText::new("Evaluate expression (↩) or add a watch (⇧⌘↩)").color(theme.text_muted)),
    );
    if std::mem::take(&mut view.focus_evaluate) {
        resp.request_focus();
    }
    let submitted = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    if submitted || (resp.has_focus() && ui.input(|i| i.key_pressed(Key::Enter))) {
        let expr = view.evaluate.trim().to_owned();
        let add_watch = ui.input(|i| i.modifiers.shift && i.modifiers.command);
        if !expr.is_empty() {
            if add_watch {
                actions.push(Action::Command("debug.addWatch", json!({ "expression": expr })));
                view.evaluate.clear();
            } else if can_eval {
                actions.push(Action::Command("run.debuggingActions.evaluateExpression", json!({ "expression": expr })));
            }
        }
        resp.request_focus();
    }
}

fn watches_only(ui: &mut Ui, debug: &DebugState, theme: &Theme, view: &mut DebugUi, actions: &mut Vec<Action>) {
    if debug.watches.is_empty() {
        return;
    }
    ui.add_space(6.0);
    ui.label(RichText::new("Watches").color(theme.text_muted));
    ScrollArea::vertical().id_salt("debug-watches-idle").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        watches(ui, debug, None, theme, view, actions);
    });
}

fn variables(
    ui: &mut Ui,
    debug: &DebugState,
    session: &DebugSession,
    theme: &Theme,
    view: &mut DebugUi,
    actions: &mut Vec<Action>,
) {
    evaluate_field(ui, debug, theme, view, actions);
    ui.add_space(2.0);
    Frame::new().fill(theme.editor_bg).inner_margin(Margin::same(2)).show(ui, |ui| {
        ScrollArea::vertical().id_salt("debug-vars-scroll").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let tree = Tree { debug, session, theme };
            if let Some(evaluation) = &debug.evaluation {
                match (&evaluation.result, evaluation.pending) {
                    (_, true) => {
                        let (rect, _) = row(ui, theme, false);
                        let job = text_job(
                            &[
                                (&evaluation.expression, theme.field),
                                (" = ", theme.text_muted),
                                ("…", theme.text_muted),
                            ],
                            &FontId::monospace(13.0),
                        );
                        paint_job(ui, rect, rect.left() + 18.0, job);
                    }
                    (Ok(v), false) => {
                        tree.node(ui, 0, dbg::EVALUATION_PATH, v, 0, Glyph::Result, actions);
                    }
                    (Err(e), false) => {
                        let (rect, _) = row(ui, theme, false);
                        let job = text_job(
                            &[(&evaluation.expression, theme.field), (" = ", theme.text_muted), (e, theme.error)],
                            &FontId::monospace(13.0),
                        );
                        paint_job(ui, rect, rect.left() + 18.0, job);
                    }
                }
            }
            watches(ui, debug, Some(&tree), theme, view, actions);
            if !session.is_stopped() {
                let text = match session.status() {
                    DebugStatus::Running => "The program is running. Variables are shown while it is paused.",
                    DebugStatus::Building => "Building…",
                    DebugStatus::Starting => "Starting the debugger…",
                    _ => "",
                };
                if !text.is_empty() {
                    ui.add_space(4.0);
                    ui.label(RichText::new(text).color(theme.text_muted));
                }
                return;
            }
            let Some(first) = session.scopes.first() else {
                ui.label(RichText::new("Collecting data…").color(theme.text_muted));
                return;
            };
            // The first scope's variables at the top level (IntelliJ-style); other scopes nest.
            let root = dbg::scope_path(&first.name);
            tree.children(ui, 0, &root, first.reference, actions);
            for scope in session.scopes.iter().skip(1) {
                let info = VariableInfo {
                    name: scope.name.clone(),
                    value: String::new(),
                    type_name: None,
                    reference: scope.reference,
                    evaluate_name: None,
                    memory_reference: None,
                };
                tree.node(ui, 0, &dbg::scope_path(&scope.name), &info, 0, Glyph::Scope, actions);
            }
        });
    });
}

fn watches(
    ui: &mut Ui,
    debug: &DebugState,
    tree: Option<&Tree<'_>>,
    theme: &Theme,
    view: &mut DebugUi,
    actions: &mut Vec<Action>,
) {
    let font = FontId::monospace(13.0);
    for (i, w) in debug.watches.iter().enumerate() {
        let path = dbg::watch_path(&w.expression);
        let info = VariableInfo {
            name: w.expression.clone(),
            value: w.value.clone().unwrap_or_default(),
            type_name: w.type_name.clone(),
            reference: w.reference,
            evaluate_name: Some(w.expression.clone()),
            memory_reference: None,
        };
        let resp = match (tree, &w.error, &w.value) {
            (Some(tree), None, Some(_)) => tree.node(ui, 0, &path, &info, 0, Glyph::Watch, actions),
            (_, error, _) => {
                let (rect, resp) = row(ui, theme, false);
                paint_glyph(ui.painter(), pos2(rect.left() + 9.0, rect.center().y), Glyph::Watch, theme);
                let (text, color) = match error {
                    Some(e) => (e.as_str(), theme.error),
                    None if debug.is_stopped() => ("…", theme.text_muted),
                    None => ("not available", theme.text_muted),
                };
                let job = text_job(&[(&w.expression, theme.field), (" = ", theme.text_muted), (text, color)], &font);
                paint_job(ui, rect, rect.left() + 18.0, job);
                resp
            }
        };
        {
            resp.context_menu(|ui| {
                if ui.button("Remove Watch").clicked() {
                    actions.push(Action::Command("debug.removeWatch", json!({ "index": i })));
                    ui.close();
                }
                if let Some(v) = &w.value
                    && ui.button("Copy Value").clicked()
                {
                    actions.push(Action::Copy(v.clone()));
                    ui.close();
                }
            });
        }
    }
    // The "+" row for a new watch.
    match &mut view.new_watch {
        Some(text) => {
            let resp = ui.add(
                TextEdit::singleline(text)
                    .desired_width(f32::INFINITY)
                    .font(font.clone())
                    .hint_text(RichText::new("New watch (↩ to add, ⎋ to cancel)").color(theme.text_muted)),
            );
            resp.request_focus();
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                view.new_watch = None;
            } else if resp.lost_focus() {
                let expr = text.trim().to_owned();
                if !expr.is_empty() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    actions.push(Action::Command("debug.addWatch", json!({ "expression": expr })));
                }
                view.new_watch = None;
            }
        }
        None => {
            let resp =
                ui.add(egui::Button::new(RichText::new("+ New Watch").color(theme.text_muted).size(12.0)).frame(false));
            if resp.clicked() {
                view.new_watch = Some(String::new());
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Glyph {
    Variable,
    Watch,
    Scope,
    Result,
}

fn paint_glyph(painter: &egui::Painter, c: Pos2, glyph: Glyph, theme: &Theme) {
    match glyph {
        Glyph::Variable => {
            painter.circle_filled(c, 3.5, theme.field);
        }
        Glyph::Watch => {
            painter.circle_stroke(pos2(c.x - 2.6, c.y), 2.4, Stroke::new(1.2, theme.link));
            painter.circle_stroke(pos2(c.x + 2.6, c.y), 2.4, Stroke::new(1.2, theme.link));
        }
        Glyph::Scope => {
            painter.rect_stroke(
                Rect::from_center_size(c, vec2(8.0, 7.0)),
                1.0,
                Stroke::new(1.2, theme.text_muted),
                egui::StrokeKind::Inside,
            );
        }
        Glyph::Result => {
            painter
                .line_segment([pos2(c.x - 3.0, c.y - 1.5), pos2(c.x + 3.0, c.y - 1.5)], Stroke::new(1.3, theme.link));
            painter
                .line_segment([pos2(c.x - 3.0, c.y + 1.5), pos2(c.x + 3.0, c.y + 1.5)], Stroke::new(1.3, theme.link));
        }
    }
}

/// The Variables tree, drawn from the session's lazily loaded children.
struct Tree<'a> {
    debug: &'a DebugState,
    session: &'a DebugSession,
    theme: &'a Theme,
}

impl Tree<'_> {
    fn children(&self, ui: &mut Ui, depth: usize, path: &str, reference: i64, actions: &mut Vec<Action>) {
        if depth > MAX_DEPTH {
            return;
        }
        match self.session.variables.get(&reference) {
            Some(vars) if vars.is_empty() => self.note(ui, depth, "No variables"),
            Some(vars) => {
                for v in vars {
                    self.node(ui, depth, &dbg::child_path(path, &v.name), v, reference, Glyph::Variable, actions);
                }
            }
            None if self.session.loading.contains(&reference) => self.note(ui, depth, "Collecting data…"),
            None => {}
        }
    }

    fn note(&self, ui: &mut Ui, depth: usize, text: &str) {
        let (rect, _) = row(ui, self.theme, false);
        let job = text_job(&[(text, self.theme.text_muted)], &FontId::proportional(12.5));
        paint_job(ui, rect, rect.left() + 18.0 + depth as f32 * INDENT, job);
    }

    /// One node; `parent` is the variables reference it was listed under (0 for roots).
    #[allow(clippy::too_many_arguments)]
    fn node(
        &self,
        ui: &mut Ui,
        depth: usize,
        path: &str,
        v: &VariableInfo,
        parent: i64,
        glyph: Glyph,
        actions: &mut Vec<Action>,
    ) -> Response {
        let theme = self.theme;
        let expandable = v.reference > 0;
        let expanded = expandable && self.debug.expanded.contains(path);
        let (rect, resp) = row(ui, theme, false);
        let x0 = rect.left() + depth as f32 * INDENT;
        let cy = rect.center().y;
        if expandable {
            let c = pos2(x0 + 6.0, cy);
            let tri = if expanded {
                vec![pos2(c.x - 3.5, c.y - 2.0), pos2(c.x + 3.5, c.y - 2.0), pos2(c.x, c.y + 2.5)]
            } else {
                vec![pos2(c.x - 2.0, c.y - 3.5), pos2(c.x + 2.5, c.y), pos2(c.x - 2.0, c.y + 3.5)]
            };
            ui.painter().add(Shape::convex_polygon(tri, theme.text_muted, Stroke::NONE));
        }
        paint_glyph(ui.painter(), pos2(x0 + 18.0, cy), glyph, theme);
        let font = FontId::monospace(13.0);
        let type_text = v.type_name.as_deref().map(|t| format!("{{{}}} ", short_type(t))).unwrap_or_default();
        let mut value: String = v.value.chars().take(400).collect();
        if value.contains('\n') {
            value = value.replace('\n', "⏎");
        }
        let name_color = if glyph == Glyph::Scope { theme.text } else { theme.field };
        let parts: Vec<(&str, Color32)> = if glyph == Glyph::Scope {
            vec![(&v.name, name_color)]
        } else {
            vec![
                (&v.name, name_color),
                (" = ", theme.text_muted),
                (&type_text, theme.text_muted),
                (&value, value_color(theme, &v.value)),
            ]
        };
        paint_job(ui, rect, x0 + 27.0, text_job(&parts, &font));
        let resp = resp.on_hover_text(match &v.type_name {
            Some(t) => format!("{}: {t}\n{}", v.name, v.value),
            None => format!("{}\n{}", v.name, v.value),
        });
        if resp.clicked() && expandable {
            actions.push(Action::Command(
                "debug.expandVariable",
                json!({ "path": path, "reference": v.reference, "expanded": !expanded }),
            ));
        }
        if glyph != Glyph::Watch {
            resp.context_menu(|ui| {
                if ui.button("Copy Value").clicked() {
                    actions.push(Action::Copy(v.value.clone()));
                    ui.close();
                }
                if ui.button("Copy Name").clicked() {
                    actions.push(Action::Copy(v.name.clone()));
                    ui.close();
                }
                if glyph == Glyph::Variable && ui.button("Add to Watches").clicked() {
                    let expr = v.evaluate_name.clone().unwrap_or_else(|| v.name.clone());
                    actions.push(Action::Command("debug.addWatch", json!({ "expression": expr })));
                    ui.close();
                }
                if glyph == Glyph::Variable && self.debug.is_stopped() {
                    ui.separator();
                    let expr = v.evaluate_name.clone().unwrap_or_else(|| v.name.clone());
                    for (label, access) in
                        [("Add Watchpoint (Break on Change)", "write"), ("Break on Value Access", "readWrite")]
                    {
                        if ui.button(label).clicked() {
                            actions.push(Action::Command(
                                "debug.watchpoint.add",
                                json!({ "name": v.name, "reference": parent, "evaluateName": expr, "access": access }),
                            ));
                            ui.close();
                        }
                    }
                    if ui.button("Show in Memory View").clicked() {
                        let address = v.memory_reference.clone().unwrap_or_else(|| format!("&({expr})"));
                        actions.push(Action::Command("debug.memory.show", json!({ "address": address })));
                        ui.close();
                    }
                }
            });
        }
        if expanded {
            self.children(ui, depth + 1, path, v.reference, actions);
        }
        resp
    }
}

// ---------------------------------------------------------------- Console

fn console(ui: &mut Ui, debug: &DebugState, theme: &Theme, view: &mut DebugUi, actions: &mut Vec<Action>) {
    if !view.console_follow_set {
        view.console_follow = true;
        view.console_follow_set = true;
    }
    ui.horizontal(|ui| {
        ui.checkbox(&mut view.console_follow, "Scroll to end");
        if ui.button("Clear").clicked() {
            actions.push(Action::Command("debug.clearConsole", Value::Null));
        }
    });
    let font = FontId::monospace(12.5);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let input_h = 28.0;
    let height = (ui.available_height() - input_h).max(40.0);
    Frame::new().fill(theme.editor_bg).inner_margin(Margin::same(4)).show(ui, |ui| {
        ui.set_height(height - 8.0);
        ScrollArea::both()
            .id_salt("debug-console-scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(view.console_follow)
            .show_rows(ui, row_h, debug.console.len(), |ui, range| {
                for line in debug.console.iter().skip(range.start).take(range.len()) {
                    let text = dbg::console_text(line);
                    let color = match line.kind {
                        DebugConsoleKind::Stdout => theme.text_editor,
                        DebugConsoleKind::Stderr => theme.console_stderr,
                        DebugConsoleKind::Info => theme.console_system,
                        DebugConsoleKind::Input => theme.link,
                        DebugConsoleKind::Result => theme.text,
                        DebugConsoleKind::Error => theme.error,
                    };
                    match crate::panels::find_location(text) {
                        Some((path, ln, col, range)) => {
                            let mut job = LayoutJob::default();
                            let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
                            job.append(text.get(..range.start).unwrap_or(""), 0.0, fmt(color));
                            let mut link = fmt(theme.link);
                            link.underline = Stroke::new(1.0, theme.link);
                            job.append(text.get(range.clone()).unwrap_or(""), 0.0, link);
                            job.append(text.get(range.end..).unwrap_or(""), 0.0, fmt(color));
                            let resp = ui
                                .add(egui::Label::new(job).sense(Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if resp.clicked() {
                                actions.push(Action::Command(
                                    "navigate.openLocation",
                                    json!({ "path": path, "line": ln.saturating_sub(1), "column": col.saturating_sub(1) }),
                                ));
                            }
                        }
                        None => {
                            ui.add(egui::Label::new(RichText::new(text).font(font.clone()).color(color)).extend());
                        }
                    }
                }
            });
    });
    // The REPL input.
    ui.horizontal(|ui| {
        ui.label(RichText::new("›").color(theme.link).font(FontId::monospace(14.0)));
        let enabled = debug.is_active();
        let lldb = debug.session.as_ref().is_some_and(|x| x.adapter.starts_with("lldb") || x.adapter == "codelldb");
        let resp = ui.add_enabled(
            enabled,
            TextEdit::singleline(&mut view.console_input)
                .id(egui::Id::new("debug-console-input"))
                .desired_width(f32::INFINITY)
                .font(FontId::monospace(13.0))
                .hint_text(
                    RichText::new(if enabled && lldb {
                        "LLDB command or expression (↩), e.g. bt, frame variable, register read"
                    } else if enabled {
                        "Expression or debugger command (↩)"
                    } else {
                        "Start debugging to evaluate"
                    })
                    .color(theme.text_muted),
                ),
        );
        let history = &debug.views.console_history;
        if resp.has_focus() {
            let (up, down) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown)));
            if up && !history.is_empty() {
                let pos = view.history_pos.map_or(history.len().saturating_sub(1), |p| p.saturating_sub(1));
                view.history_pos = Some(pos);
                view.console_input = history.get(pos).cloned().unwrap_or_default();
            } else if down && let Some(p) = view.history_pos {
                let next = p.saturating_add(1);
                if next >= history.len() {
                    view.history_pos = None;
                    view.console_input.clear();
                } else {
                    view.history_pos = Some(next);
                    view.console_input = history.get(next).cloned().unwrap_or_default();
                }
            }
        }
        if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            let expr = view.console_input.trim().to_owned();
            if !expr.is_empty() {
                // The engine keeps the history.
                actions.push(Action::Command("debug.consoleEvaluate", json!({ "expression": expr })));
                view.console_input.clear();
                view.history_pos = None;
            }
            resp.request_focus();
        }
    });
}

// ---------------------------------------------------------------- Breakpoints

/// The breakpoint marker: filled when verified (or no session), hollow when the adapter could not
/// set it, grey when disabled; a small dot marks a condition or log message.
pub fn paint_breakpoint(painter: &egui::Painter, c: Pos2, radius: f32, bp: &Breakpoint, theme: &Theme) {
    let color = if bp.enabled { theme.breakpoint } else { theme.breakpoint_disabled };
    if bp.enabled && bp.verified == Some(false) {
        painter.circle_stroke(c, radius - 0.8, Stroke::new(1.6, color));
    } else if bp.temporary {
        painter.circle_filled(c, radius, color);
        painter.circle_filled(c, radius * 0.4, theme.editor_bg);
    } else {
        painter.circle_filled(c, radius, color);
    }
    if bp.condition.is_some() || bp.log_message.is_some() || bp.hit_condition.is_some() {
        painter.circle_filled(pos2(c.x + radius * 0.75, c.y + radius * 0.75), radius * 0.38, theme.warning);
    }
}

fn breakpoints(ui: &mut Ui, s: &Session, theme: &Theme, view: &mut DebugUi, actions: &mut Vec<Action>) {
    let debug = &s.debug;
    ui.horizontal(|ui| {
        let mut on_panic = debug.break_on_panic;
        if ui.checkbox(&mut on_panic, "Break on Rust panics").changed() {
            actions.push(Action::Command("debug.breakOnPanic", json!({ "enabled": on_panic })));
        }
        ui.add_space(8.0);
        if ui.add_enabled(!debug.breakpoints.is_empty(), egui::Button::new("Remove All")).clicked() {
            actions.push(Action::Command("debug.removeAllBreakpoints", Value::Null));
            view.selected_breakpoint = None;
        }
    });
    if debug.breakpoints.is_empty() {
        empty_hint(
            ui,
            theme,
            "No breakpoints. Click a line number in the editor or press ⌘F8 to add one. Watchpoints are added \
             from a variable's context menu while paused.",
        );
        return;
    }
    let mut sorted: Vec<&Breakpoint> = debug.breakpoints.iter().collect();
    sorted.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    let editor_h = if view.selected_breakpoint.is_some() { 92.0 } else { 0.0 };
    let list_h = (ui.available_height() - editor_h).max(40.0);
    let font = FontId::proportional(13.0);
    ScrollArea::vertical().id_salt("debug-bps").max_height(list_h).auto_shrink([false, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for bp in &sorted {
            let selected = view.selected_breakpoint == Some(bp.id);
            let (rect, resp) = row(ui, theme, selected);
            // Enabled checkbox.
            let check = Rect::from_center_size(pos2(rect.left() + 10.0, rect.center().y), vec2(12.0, 12.0));
            let check_resp = ui.interact(check, ui.id().with(("bp-enable", bp.id)), Sense::click());
            ui.painter().rect_stroke(check, 2.0, Stroke::new(1.0, theme.text_muted), egui::StrokeKind::Inside);
            if bp.enabled {
                ui.painter().line_segment(
                    [pos2(check.left() + 2.5, check.center().y), pos2(check.center().x - 0.5, check.bottom() - 3.0)],
                    Stroke::new(1.6, theme.text),
                );
                ui.painter().line_segment(
                    [pos2(check.center().x - 0.5, check.bottom() - 3.0), pos2(check.right() - 2.5, check.top() + 3.0)],
                    Stroke::new(1.6, theme.text),
                );
            }
            paint_breakpoint(ui.painter(), pos2(rect.left() + 30.0, rect.center().y), 5.5, bp, theme);
            let location = file_label(&bp.path, bp.line);
            let preview =
                s.ws.doc_by_path(&bp.path)
                    .and_then(|d| s.ws.doc(d))
                    .and_then(|d| d.doc.line_text(bp.line))
                    .map(|t| t.trim().chars().take(120).collect::<String>())
                    .unwrap_or_default();
            let mut extra = String::new();
            if let Some(c) = &bp.condition {
                extra.push_str(&format!("  if {c}"));
            }
            if let Some(h) = &bp.hit_condition {
                extra.push_str(&format!("  hits {h}"));
            }
            if let Some(m) = &bp.log_message {
                extra.push_str(&format!("  log \"{m}\""));
            }
            if bp.temporary {
                extra.push_str("  (temporary)");
            }
            let job = text_job(
                &[
                    (&location, if bp.enabled { theme.text } else { theme.text_muted }),
                    (&extra, theme.warning),
                    ("   ", theme.text_muted),
                    (&preview, theme.text_muted),
                ],
                &font,
            );
            paint_job(ui, rect, rect.left() + 42.0, job);
            let mut tip = bp.path.display().to_string();
            match (bp.verified, &bp.message) {
                (Some(false), Some(m)) => tip.push_str(&format!("\nNot set: {m}")),
                (Some(false), None) => tip.push_str("\nThe debugger could not set this breakpoint (yet)."),
                _ => {}
            }
            let resp = resp.on_hover_text(tip);
            if check_resp.clicked() {
                actions.push(Action::Command(
                    "debug.setBreakpoint",
                    json!({ "path": bp.path, "line": bp.line, "enabled": !bp.enabled }),
                ));
            } else if resp.double_clicked() {
                actions.push(Action::Command(
                    "navigate.openLocation",
                    json!({ "path": bp.path, "line": bp.line, "column": 0 }),
                ));
            } else if resp.clicked() {
                actions.push(Action::SelectBreakpoint(bp.id));
            }
            resp.context_menu(|ui| {
                if ui.button("Go to Source").clicked() {
                    actions.push(Action::Command(
                        "navigate.openLocation",
                        json!({ "path": bp.path, "line": bp.line, "column": 0 }),
                    ));
                    ui.close();
                }
                if ui.button("Remove").clicked() {
                    actions.push(Action::Command("debug.removeBreakpoint", json!({ "id": bp.id })));
                    ui.close();
                }
            });
        }
    });
    let Some(bp) = view.selected_breakpoint.and_then(|id| debug.breakpoints.iter().find(|b| b.id == id)) else {
        view.selected_breakpoint = None;
        return;
    };
    if view.edit_for != Some(bp.id) {
        view.edit_for = Some(bp.id);
        view.edit_condition = bp.condition.clone().unwrap_or_default();
        view.edit_log = bp.log_message.clone().unwrap_or_default();
        view.edit_hit = bp.hit_condition.clone().unwrap_or_default();
    }
    ui.separator();
    let mut apply = false;
    egui::Grid::new("debug-bp-editor").num_columns(2).spacing(vec2(8.0, 4.0)).show(ui, |ui| {
        ui.label(RichText::new("Condition").color(theme.text_muted));
        let r = ui.add(
            TextEdit::singleline(&mut view.edit_condition)
                .desired_width(f32::INFINITY)
                .font(FontId::monospace(13.0))
                .hint_text("Stop only when this expression is true"),
        );
        apply |= r.lost_focus();
        ui.end_row();
        ui.label(RichText::new("Log message").color(theme.text_muted));
        let r = ui.add(
            TextEdit::singleline(&mut view.edit_log)
                .desired_width(f32::INFINITY)
                .font(FontId::monospace(13.0))
                .hint_text("Print instead of stopping, e.g. x = {x}"),
        );
        apply |= r.lost_focus();
        ui.end_row();
        ui.label(RichText::new("Hit count").color(theme.text_muted));
        ui.horizontal(|ui| {
            let r = ui.add(
                TextEdit::singleline(&mut view.edit_hit)
                    .desired_width(120.0)
                    .font(FontId::monospace(13.0))
                    .hint_text("e.g. 3 or >= 5"),
            );
            apply |= r.lost_focus();
            let mut temporary = bp.temporary;
            if ui.checkbox(&mut temporary, "Remove once hit").changed() {
                actions.push(Action::Command(
                    "debug.setBreakpoint",
                    json!({ "path": bp.path, "line": bp.line, "temporary": temporary }),
                ));
            }
            if ui.button("Remove").clicked() {
                actions.push(Action::Command("debug.removeBreakpoint", json!({ "id": bp.id })));
            }
        });
        ui.end_row();
    });
    let changed = view.edit_condition != bp.condition.clone().unwrap_or_default()
        || view.edit_log != bp.log_message.clone().unwrap_or_default()
        || view.edit_hit != bp.hit_condition.clone().unwrap_or_default();
    if apply && changed {
        actions.push(Action::Command(
            "debug.setBreakpoint",
            json!({
                "path": bp.path, "line": bp.line,
                "condition": view.edit_condition, "logMessage": view.edit_log, "hitCondition": view.edit_hit,
            }),
        ));
    }
}

// ---------------------------------------------------------------- Run › Debug… chooser

fn chooser(ui: &mut Ui, s: &Session, theme: &Theme, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Debug").strong());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Cancel").clicked() {
                actions.push(Action::Command("debug.showTab", json!({ "tab": "variables" })));
            }
        });
    });
    ui.add_space(4.0);
    let font = FontId::proportional(13.0);
    ScrollArea::vertical().id_salt("debug-chooser").auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, c) in s.run_configs.iter().enumerate() {
            let (rect, resp) = row(ui, theme, i == s.selected_run_config);
            paint_bug(ui.painter(), pos2(rect.left() + 10.0, rect.center().y), theme);
            let detail = c.command_line();
            paint_job(
                ui,
                rect,
                rect.left() + 24.0,
                text_job(&[(&c.name, theme.text), ("   ", theme.text), (&detail, theme.text_muted)], &font),
            );
            if resp.clicked() {
                actions.push(Action::Command("run.debug", json!({ "config": i })));
            }
        }
        if let Some((_, d)) = s.ws.active()
            && let Some(name) = d.path.as_deref().and_then(Path::file_name)
        {
            let (rect, resp) = row(ui, theme, false);
            paint_bug(ui.painter(), pos2(rect.left() + 10.0, rect.center().y), theme);
            let label = format!("Debug context at the caret in {}", name.to_string_lossy());
            paint_job(ui, rect, rect.left() + 24.0, text_job(&[(&label, theme.text)], &font));
            if resp.clicked() {
                actions.push(Action::Command("debug.atCaret", Value::Null));
            }
        }
        if s.run_configs.is_empty() && s.ws.active().is_none() {
            empty_hint(ui, theme, "No run configurations. Open a Cargo project, or a Python or Zig file.");
        }
    });
}

/// A small bug glyph for debug actions.
fn paint_bug(painter: &egui::Painter, c: Pos2, theme: &Theme) {
    let color = theme.success;
    painter.circle_filled(pos2(c.x, c.y + 1.0), 4.0, color);
    painter.circle_filled(pos2(c.x, c.y - 3.5), 2.2, color);
    for dy in [-1.0f32, 1.5, 4.0] {
        painter.line_segment([pos2(c.x - 6.5, c.y + dy), pos2(c.x + 6.5, c.y + dy)], Stroke::new(1.0, color));
    }
}

/// Run › Attach to Process…: the process list (filter, ↑/↓, Enter to attach, Esc to close).
pub fn attach_window(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let Some(picker) = s.debug.attach.clone() else { return };
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut open = true;
    let visible = picker.visible();
    crate::windows::resizable(egui::Window::new("Attach to Process").default_size([640.0, 420.0]), st, "attach")
        .id(egui::Id::new("attach-to-process"))
        .open(&mut open)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ui.ctx(), |ui| {
            let mut query = picker.query.clone();
            ui.horizontal(|ui| {
                let field = ui.add(
                    TextEdit::singleline(&mut query).hint_text("Filter by name, command or pid").desired_width(360.0),
                );
                if !field.has_focus() && !ui.ctx().memory(|m| m.focused().is_some()) {
                    field.request_focus();
                }
                let mut all = picker.all_users;
                if ui.checkbox(&mut all, "All users").changed() {
                    cmds.push(("debug.attach.refresh", json!({ "allUsers": all })));
                }
                if ui.button("Refresh").clicked() {
                    cmds.push(("debug.attach.refresh", json!({ "allUsers": picker.all_users })));
                }
            });
            if query != picker.query {
                cmds.push(("debug.attach.filter", json!({ "query": query })));
            }
            if picker.loading {
                ui.label(RichText::new("Listing processes…").color(theme.text_muted));
            }
            if let Some(e) = &picker.error {
                ui.label(RichText::new(e).color(theme.error));
            }
            if !picker.loading && visible.is_empty() {
                ui.label(RichText::new("No matching processes.").color(theme.text_muted));
            }
            ScrollArea::vertical().auto_shrink([false, false]).max_height(320.0).show(ui, |ui| {
                for (row, index) in visible.iter().enumerate().take(500) {
                    let Some(p) = picker.processes.get(*index) else { continue };
                    let kind = match p.kind {
                        dbg::ProcessKind::Rust => "Rust",
                        dbg::ProcessKind::Python => "Python",
                        dbg::ProcessKind::Native => "Native",
                    };
                    let text = format!("{:>7}  {}  ·  {kind}", p.pid, p.name);
                    let resp = ui.selectable_label(row == picker.selected, text).on_hover_text(&p.command);
                    if row == picker.selected && st.debug.attach_scroll {
                        resp.scroll_to_me(None);
                    }
                    if resp.double_clicked() {
                        cmds.push(("debug.attach.confirm", json!({ "index": row })));
                    } else if resp.clicked() {
                        let delta = i64::try_from(row).unwrap_or(0) - i64::try_from(picker.selected).unwrap_or(0);
                        cmds.push(("debug.attach.move", json!({ "delta": delta })));
                    }
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                let can = picker.selected_process().is_some();
                if ui.add_enabled(can, egui::Button::new("Attach")).clicked() {
                    cmds.push(("debug.attach.confirm", Value::Null));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("debug.attach.cancel", Value::Null));
                }
            });
        });
    st.debug.attach_scroll = false;
    let (up, down, enter, escape) = ui.ctx().input(|i| {
        (
            i.key_pressed(Key::ArrowUp),
            i.key_pressed(Key::ArrowDown),
            i.key_pressed(Key::Enter),
            i.key_pressed(Key::Escape),
        )
    });
    if up || down {
        cmds.push(("debug.attach.move", json!({ "delta": if up { -1 } else { 1 } })));
        st.debug.attach_scroll = true;
    }
    if enter {
        cmds.push(("debug.attach.confirm", Value::Null));
    }
    if escape || !open {
        cmds.push(("debug.attach.cancel", Value::Null));
    }
    for (id, params) in cmds {
        run_command(s, st, id, &params);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_types_are_shortened() {
        assert_eq!(short_type("alloc::vec::Vec<int, alloc::alloc::Global>"), "Vec<int>");
        assert_eq!(short_type("core::option::Option<u8>"), "Option<u8>");
        assert_eq!(short_type("&[i32]"), "&[i32]");
        assert_eq!(
            short_type("std::collections::hash::map::HashMap<alloc::string::String, i32>"),
            "HashMap<String, i32>"
        );
        assert_eq!(short_type("int *"), "int *");
        assert_eq!(short_type("::"), "");
    }

    #[test]
    fn values_are_coloured_by_kind() {
        let theme = Theme::dark();
        assert_eq!(value_color(&theme, "\"hi\""), theme.string);
        assert_eq!(value_color(&theme, "-12.5"), theme.number);
        assert_eq!(value_color(&theme, "None"), theme.keyword);
        assert_eq!(value_color(&theme, "size=3"), theme.text_editor);
    }
}
