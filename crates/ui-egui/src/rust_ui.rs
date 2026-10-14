//! Rust tools (M12 track RS): the macro expansion popup and the Rust REPL tool window. Thin:
//! every action is an engine command.

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Align2, FontId, Id, Key, Layout, Modifiers, RichText, ScrollArea, TextEdit, vec2};
use serde_json::Value;

use codecraft_engine::Session;
use codecraft_engine::rust_repl::ReplKind;
use codecraft_syntax::Language;

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

/// Rust code as a highlighted layout job (theme syntax colours).
fn highlighted(text: &str, st: &UiState, size: f32) -> LayoutJob {
    let hl = codecraft_syntax::highlight(Language::Rust, text);
    let font = FontId::monospace(size);
    let mut job = LayoutJob::default();
    for (i, line) in text.split('\n').enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let mut col = 0usize;
        for span in hl.line(i) {
            let (start, end) = (span.start.min(chars.len()), span.end.min(chars.len()));
            if start > col {
                let plain: String = chars.get(col..start).unwrap_or_default().iter().collect();
                job.append(&plain, 0.0, TextFormat::simple(font.clone(), st.theme.text_editor));
            }
            if end > start {
                let part: String = chars.get(start..end).unwrap_or_default().iter().collect();
                job.append(&part, 0.0, TextFormat::simple(font.clone(), st.theme.syntax(span.kind)));
            }
            col = col.max(end);
        }
        if col < chars.len() {
            let rest: String = chars.get(col..).unwrap_or_default().iter().collect();
            job.append(&rest, 0.0, TextFormat::simple(font.clone(), st.theme.text_editor));
        }
        job.append("\n", 0.0, TextFormat::simple(font.clone(), st.theme.text_editor));
    }
    job
}

/// The macro expansion popup (Show Recursive Macro Expansion).
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let Some(view) = s.rust.macros.view.clone() else { return };
    let ctx = ui.ctx().clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let title = format!("Recursive expansion of {} macro", view.name);
    egui::Window::new(title)
        .id(Id::new("rust-macro-expansion"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(640.0, 420.0))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(&ctx, |ui| {
            ui.horizontal(|ui| {
                let file = view.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                ui.label(RichText::new(format!("{file}:{}", view.line + 1)).color(st.theme.text_muted).small());
                if !view.formatted {
                    ui.label(RichText::new("formatting…").color(st.theme.text_muted).small());
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Close").clicked() {
                        cmds.push(("rust.macroExpansion.close", Value::Null));
                    }
                    if ui.small_button("Copy").on_hover_text("Copy the expansion").clicked() {
                        cmds.push(("rust.macroExpansion.copy", Value::Null));
                    }
                });
            });
            ui.separator();
            ScrollArea::both().id_salt("rust-macro-expansion-scroll").auto_shrink([false, false]).show(ui, |ui| {
                let job = highlighted(&view.text, st, 13.0);
                ui.add(egui::Label::new(job).selectable(true).extend());
            });
        });
    if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
        cmds.push(("rust.macroExpansion.close", Value::Null));
    }
    run_all(s, st, cmds);
}

/// The Rust REPL tool window: transcript and a multi-line input with history.
pub fn show_repl(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let r = &s.rust.repl;
    let alive = r.is_alive();
    ui.horizontal(|ui| {
        let status = if !alive {
            ("Not running", theme.text_muted)
        } else if r.running {
            ("Evaluating…", theme.warning)
        } else if r.ready {
            ("Ready", theme.success)
        } else {
            ("Starting…", theme.text_muted)
        };
        ui.label(RichText::new(status.0).color(status.1).small());
        ui.label(RichText::new("evcxr").color(theme.text_muted).small());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("Clear").on_hover_text("Clear the REPL output").clicked() {
                cmds.push(("rust.repl.clear", Value::Null));
            }
            if ui.add_enabled(alive, egui::Button::new("■").small()).on_hover_text("Stop the REPL").clicked() {
                cmds.push(("rust.repl.stop", Value::Null));
            }
            let (label, tip) = if alive { ("⟲", "Restart the REPL") } else { ("▶", "Start the REPL") };
            if ui.small_button(label).on_hover_text(tip).clicked() {
                cmds.push(("rust.repl.restart", Value::Null));
            }
        });
    });
    ui.separator();

    let input_id = Id::new("rust-repl-input");
    egui::Panel::bottom("rust-repl-input-panel").show_separator_line(true).show(ui, |ui| {
        let focused = ui.memory(|m| m.has_focus(input_id));
        let single_line = !s.rust.repl.draft.contains('\n');
        if focused {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                cmds.push(("rust.repl.execute", Value::Null));
            }
            if single_line && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                cmds.push(("rust.repl.historyPrevious", Value::Null));
            }
            if single_line && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                cmds.push(("rust.repl.historyNext", Value::Null));
            }
        }
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(if single_line { ">>" } else { ".." }).monospace().color(theme.accent));
            let rows = s.rust.repl.draft.lines().count().clamp(1, 12);
            let edit = TextEdit::multiline(&mut s.rust.repl.draft)
                .id(input_id)
                .font(FontId::monospace(13.0))
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .hint_text("Rust code or :command (Enter runs a complete input, Shift+Enter adds a line)");
            ui.add(edit);
        });
    });

    ScrollArea::both().id_salt("rust-repl-out").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if s.rust.repl.transcript.is_empty() {
            ui.label(
                RichText::new("Press ▶ or type Rust code below to start the REPL (evcxr).").color(theme.text_muted),
            );
        }
        for (kind, text) in &s.rust.repl.transcript {
            let text = text.strip_suffix('\n').unwrap_or(text);
            match kind {
                ReplKind::Input => {
                    ui.add(egui::Label::new(highlighted(text, st, 13.0)).selectable(true));
                }
                _ => {
                    let color = match kind {
                        ReplKind::Error => theme.console_stderr,
                        ReplKind::System => theme.console_system,
                        _ => theme.text_editor,
                    };
                    ui.add(egui::Label::new(RichText::new(text).monospace().color(color)).selectable(true));
                }
            }
        }
    });
    let refocus = cmds.iter().any(|(id, _)| id.starts_with("rust.repl.history") || *id == "rust.repl.execute");
    run_all(s, st, cmds);
    if refocus {
        ui.memory_mut(|m| m.request_focus(input_id));
    }
}
