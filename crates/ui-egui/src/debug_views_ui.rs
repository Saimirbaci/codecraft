//! The Debug tool window's Memory, Disassembly and Registers tabs, the watchpoint rows of the
//! Breakpoints tab, and the Core Dump / Remote Debug forms. OWNED BY the debugger track. Thin:
//! everything is drawn from `s.debug.views` and every action is an engine command
//! (`debug.memory.*`, `debug.disassembly.*`, `debug.registers.*`, `debug.watchpoint.*`,
//! `debug.coreDump`, `debug.remote`).

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Frame, Key, Margin, RichText, ScrollArea, Sense, Stroke, TextEdit, Ui, pos2, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::debug::DebugForm;
use codecraft_engine::debug_views::{self as views, BYTES_PER_ROW, DisassemblyRow, WatchAccess, Watchpoint};

use crate::theme::Theme;

/// A command to run after painting.
pub type Cmd = (&'static str, Value);

const ROW_H: f32 = 18.0;

/// View state of the views (input buffers, what to scroll to).
#[derive(Default)]
pub struct ViewsUi {
    /// The Memory view's address/expression field.
    memory_input: String,
    memory_input_set: bool,
    /// The byte clicked for editing and the hex being typed.
    edit_at: Option<u64>,
    edit_text: String,
    /// The disassembly scrolled to this program counter already.
    scrolled_to: Option<u64>,
    /// Core Dump / Remote Debug form fields.
    core: String,
    program: String,
    address: String,
    /// Python Debug Server path mappings (`local=remote; …`).
    mappings: String,
}

fn mono() -> FontId {
    FontId::monospace(12.5)
}

fn job(parts: &[(&str, Color32)]) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    job.wrap.max_rows = 1;
    for (text, color) in parts {
        job.append(text, 0.0, TextFormat { font_id: mono(), color: *color, ..Default::default() });
    }
    job
}

fn hint(ui: &mut Ui, theme: &Theme, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).color(theme.text_muted));
}

fn error_line(ui: &mut Ui, theme: &Theme, error: &Option<String>) {
    if let Some(e) = error {
        ui.label(RichText::new(e).color(theme.error));
    }
}

// ---------------------------------------------------------------- Memory

pub fn memory(ui: &mut Ui, s: &Session, theme: &Theme, view: &mut ViewsUi, cmds: &mut Vec<Cmd>) {
    let m = &s.debug.views.memory;
    if !view.memory_input_set {
        view.memory_input = m.input.clone();
        view.memory_input_set = true;
    }
    let paused = s.debug.is_stopped();
    ui.horizontal(|ui| {
        let field = ui.add_enabled(
            paused,
            TextEdit::singleline(&mut view.memory_input)
                .id(egui::Id::new("debug-memory-address"))
                .desired_width(260.0)
                .font(FontId::monospace(13.0))
                .hint_text(RichText::new("Address or expression: 0x…, &x, ptr").color(theme.text_muted)),
        );
        let go = ui.add_enabled(paused, egui::Button::new("Go")).clicked();
        if go || (field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) {
            let address = view.memory_input.trim().to_owned();
            if !address.is_empty() {
                cmds.push(("debug.memory.goto", json!({ "address": address })));
            }
        }
        let has_page = paused && m.address.is_some();
        if ui.add_enabled(has_page, egui::Button::new("◀")).on_hover_text("Previous page").clicked() {
            cmds.push(("debug.memory.page", json!({ "delta": -1 })));
        }
        if ui.add_enabled(has_page, egui::Button::new("▶")).on_hover_text("Next page").clicked() {
            cmds.push(("debug.memory.page", json!({ "delta": 1 })));
        }
        if ui.add_enabled(has_page, egui::Button::new("Refresh")).clicked() {
            cmds.push(("debug.memory.refresh", Value::Null));
        }
        if m.loading {
            ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
        }
        if m.via_console {
            ui.label(RichText::new("via LLDB console").color(theme.text_muted).size(11.0));
        }
    });
    error_line(ui, theme, &m.error);
    if let Some(at) = view.edit_at {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Write at 0x{at:x}:")).color(theme.text_muted));
            let field = ui.add(
                TextEdit::singleline(&mut view.edit_text)
                    .id(egui::Id::new("debug-memory-edit"))
                    .desired_width(220.0)
                    .font(FontId::monospace(13.0))
                    .hint_text("hex bytes, e.g. 2a 00 ff"),
            );
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if ui.button("Write").clicked() || enter {
                cmds.push((
                    "debug.memory.write",
                    json!({ "address": format!("0x{at:x}"), "bytes": view.edit_text.trim() }),
                ));
                view.edit_at = None;
            } else if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                view.edit_at = None;
            }
        });
    }
    if !paused && m.bytes.is_empty() {
        hint(ui, theme, "The memory is shown while the program is paused. Enter an address or `&variable`.");
        return;
    }
    let rows = m.rows();
    if rows.is_empty() {
        if m.error.is_none() && !m.loading {
            hint(ui, theme, "Enter an address (0x…) or an expression (&x, a pointer) and press Go.");
        }
        return;
    }
    let char_w = ui.fonts_mut(|f| f.glyph_width(&mono(), '0'));
    Frame::new().fill(theme.editor_bg).inner_margin(Margin::same(4)).show(ui, |ui| {
        ScrollArea::both().id_salt("debug-memory-rows").auto_shrink([false, false]).show_rows(
            ui,
            ROW_H,
            rows.len(),
            |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (row_address, bytes) in rows.iter().skip(range.start).take(range.len()) {
                    let width = char_w * (18.0 + 3.0 * BYTES_PER_ROW as f32 + 2.0 + BYTES_PER_ROW as f32 + 4.0);
                    let (rect, _) =
                        ui.allocate_exact_size(vec2(width.max(ui.available_width()), ROW_H), Sense::hover());
                    let y = rect.center().y;
                    let galley =
                        ui.fonts_mut(|f| f.layout_job(job(&[(&format!("{row_address:016x}"), theme.line_number)])));
                    ui.painter().galley(pos2(rect.left(), y - galley.size().y / 2.0), galley, Color32::PLACEHOLDER);
                    let hex_x = rect.left() + char_w * 18.0;
                    for (i, byte) in bytes.iter().enumerate() {
                        let gap = if i >= BYTES_PER_ROW / 2 { char_w } else { 0.0 };
                        let x = hex_x + gap + char_w * 3.0 * i as f32;
                        let address = row_address.saturating_add(i as u64);
                        let offset =
                            usize::try_from(address.saturating_sub(m.address.unwrap_or(0))).unwrap_or(usize::MAX);
                        let changed = m.changed.contains(&offset);
                        let (text, color) = match byte {
                            Some(b) => (format!("{b:02x}"), if changed { theme.warning } else { theme.text_editor }),
                            None => ("??".to_owned(), theme.text_disabled),
                        };
                        let cell =
                            egui::Rect::from_min_size(pos2(x - 1.0, rect.top()), vec2(char_w * 2.0 + 2.0, ROW_H));
                        let resp = ui.interact(cell, ui.id().with(("mem-byte", address)), Sense::click());
                        if view.edit_at == Some(address) {
                            ui.painter().rect_filled(cell, 2.0, theme.selection);
                        } else if resp.hovered() {
                            ui.painter().rect_filled(cell, 2.0, theme.hover);
                        }
                        let galley = ui.fonts_mut(|f| f.layout_job(job(&[(&text, color)])));
                        ui.painter().galley(pos2(x, y - galley.size().y / 2.0), galley, Color32::PLACEHOLDER);
                        let resp = resp.on_hover_text(format!("0x{address:x}"));
                        if resp.clicked() && paused && byte.is_some() {
                            view.edit_at = Some(address);
                            view.edit_text = text.clone();
                        }
                    }
                    let ascii_x = hex_x + char_w * (3.0 * BYTES_PER_ROW as f32 + 2.0);
                    let galley = ui.fonts_mut(|f| f.layout_job(job(&[(&views::ascii_row(bytes), theme.string)])));
                    ui.painter().galley(pos2(ascii_x, y - galley.size().y / 2.0), galley, Color32::PLACEHOLDER);
                }
            },
        );
    });
}

// ---------------------------------------------------------------- Disassembly

pub fn disassembly(ui: &mut Ui, s: &Session, theme: &Theme, view: &mut ViewsUi, cmds: &mut Vec<Cmd>) {
    let d = &s.debug.views.disassembly;
    let paused = s.debug.is_stopped();
    ui.horizontal(|ui| {
        if ui.add_enabled(paused, egui::Button::new("Step Over Instruction")).clicked() {
            cmds.push(("debug.disassembly.stepOverInstruction", Value::Null));
        }
        if ui.add_enabled(paused, egui::Button::new("Step Into Instruction")).clicked() {
            cmds.push(("debug.disassembly.stepIntoInstruction", Value::Null));
        }
        if ui.add_enabled(paused, egui::Button::new("Refresh")).clicked() {
            cmds.push(("debug.disassembly.refresh", Value::Null));
        }
        if d.loading {
            ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
        }
        ui.label(
            RichText::new("Step Over and Step Into go by instruction while this tab is shown.")
                .color(theme.text_muted)
                .size(11.0),
        );
    });
    error_line(ui, theme, &d.error);
    if d.rows.is_empty() {
        if d.error.is_none() && !d.loading {
            hint(ui, theme, "The disassembly is shown while the program is paused.");
        }
        return;
    }
    let current_row = d.current_row();
    let scroll_to = match (current_row, d.current) {
        (Some(row), Some(pc)) if view.scrolled_to != Some(pc) => {
            view.scrolled_to = Some(pc);
            Some(row)
        }
        _ => None,
    };
    Frame::new().fill(theme.editor_bg).inner_margin(Margin::same(4)).show(ui, |ui| {
        let mut area = ScrollArea::both().id_salt("debug-disassembly-rows").auto_shrink([false, false]);
        if let Some(row) = scroll_to {
            let height = ui.available_height();
            area = area.vertical_scroll_offset((row as f32 * ROW_H - height / 3.0).max(0.0));
        }
        area.show_rows(ui, ROW_H, d.rows.len(), |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, row) in d.rows.iter().enumerate().skip(range.start).take(range.len()) {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(600.0), ROW_H), Sense::click());
                let current = Some(i) == current_row;
                if current {
                    ui.painter().rect_filled(rect, 0.0, theme.execution_line);
                    crate::debug_panel::paint_exec_arrow(ui.painter(), pos2(rect.left() + 8.0, rect.center().y), theme);
                }
                let line = match row {
                    DisassemblyRow::Symbol { name } => job(&[(&format!("{name}:"), theme.function)]),
                    DisassemblyRow::Source { path, line, text } => {
                        let location = match (path, line) {
                            (Some(p), Some(l)) => format!(
                                "{}:{}  ",
                                p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                                l.saturating_add(1)
                            ),
                            _ => String::new(),
                        };
                        job(&[(&location, theme.text_muted), (text.trim_end(), theme.comment)])
                    }
                    DisassemblyRow::Instruction { address, bytes, text, offset } => {
                        let address = format!("0x{address:012x}  ");
                        let offset = offset.as_deref().map(|o| format!("{o:<8}")).unwrap_or_default();
                        let bytes = bytes.as_deref().map(|b| format!("{b:<14}")).unwrap_or_default();
                        let (mnemonic, operands) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
                        job(&[
                            (&address, theme.line_number),
                            (&offset, theme.text_muted),
                            (&bytes, theme.text_disabled),
                            (&format!("{mnemonic:<8}"), theme.keyword),
                            (operands.trim(), theme.text_editor),
                        ])
                    }
                };
                let indent = if matches!(row, DisassemblyRow::Instruction { .. }) { 18.0 } else { 4.0 };
                let galley = ui.fonts_mut(|f| f.layout_job(line));
                let y = rect.center().y - galley.size().y / 2.0;
                ui.painter().with_clip_rect(rect).galley(pos2(rect.left() + indent, y), galley, Color32::PLACEHOLDER);
                if let DisassemblyRow::Source { path: Some(path), line: Some(line), .. } = row
                    && resp.double_clicked()
                {
                    cmds.push(("navigate.openLocation", json!({ "path": path, "line": line, "column": 0 })));
                }
            }
        });
    });
}

// ---------------------------------------------------------------- Registers

pub fn registers(ui: &mut Ui, s: &Session, theme: &Theme, cmds: &mut Vec<Cmd>) {
    let r = &s.debug.views.registers;
    let paused = s.debug.is_stopped();
    ui.horizontal(|ui| {
        if ui.add_enabled(paused, egui::Button::new("Refresh")).clicked() {
            cmds.push(("debug.registers.refresh", Value::Null));
        }
        if r.loading {
            ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
        }
        ui.label(
            RichText::new("Values that changed since the previous stop are highlighted.")
                .color(theme.text_muted)
                .size(11.0),
        );
    });
    error_line(ui, theme, &r.error);
    if r.groups.is_empty() {
        if r.error.is_none() && !r.loading {
            hint(ui, theme, "The registers are shown while the program is paused.");
        }
        return;
    }
    Frame::new().fill(theme.editor_bg).inner_margin(Margin::same(4)).show(ui, |ui| {
        ScrollArea::vertical().id_salt("debug-registers").auto_shrink([false, false]).show(ui, |ui| {
            for (gi, group) in r.groups.iter().enumerate() {
                egui::CollapsingHeader::new(RichText::new(&group.name).strong())
                    .id_salt(("debug-register-group", gi))
                    .default_open(gi == 0)
                    .show(ui, |ui| {
                        egui::Grid::new(("debug-register-grid", gi)).num_columns(2).spacing(vec2(16.0, 2.0)).show(
                            ui,
                            |ui| {
                                for reg in &group.registers {
                                    ui.label(RichText::new(&reg.name).font(mono()).color(theme.field));
                                    let color = if reg.changed { theme.warning } else { theme.text_editor };
                                    let mut value = RichText::new(&reg.value).font(mono()).color(color);
                                    if reg.changed {
                                        value = value.strong();
                                    }
                                    ui.label(value).context_menu(|ui| {
                                        if ui.button("Copy Value").clicked() {
                                            ui.ctx().copy_text(reg.value.clone());
                                            ui.close();
                                        }
                                        if let Some(address) =
                                            codecraft_engine::dap::memory::leading_address(&reg.value)
                                            && ui.button("Show in Memory View").clicked()
                                        {
                                            cmds.push((
                                                "debug.memory.show",
                                                json!({ "address": format!("0x{address:x}") }),
                                            ));
                                            ui.close();
                                        }
                                    });
                                    ui.end_row();
                                }
                            },
                        );
                    });
            }
        });
    });
}

// ---------------------------------------------------------------- watchpoints

/// The Watchpoints section of the Breakpoints tab.
pub fn watchpoints(ui: &mut Ui, s: &Session, theme: &Theme, cmds: &mut Vec<Cmd>) {
    let list = &s.debug.views.watchpoints;
    if list.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Watchpoints").color(theme.text_muted));
        if ui.small_button("Remove All").clicked() {
            cmds.push(("debug.watchpoint.removeAll", Value::Null));
        }
    });
    for w in list {
        watchpoint_row(ui, w, theme, cmds);
    }
    ui.separator();
}

fn watchpoint_row(ui: &mut Ui, w: &Watchpoint, theme: &Theme, cmds: &mut Vec<Cmd>) {
    ui.horizontal(|ui| {
        let mut enabled = w.enabled;
        if ui.checkbox(&mut enabled, "").changed() {
            cmds.push(("debug.watchpoint.setEnabled", json!({ "id": w.id, "enabled": enabled })));
        }
        // A watchpoint glyph: a breakpoint-coloured eye.
        let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
        let color = if w.enabled { theme.breakpoint } else { theme.breakpoint_disabled };
        let c = rect.center();
        if w.verified == Some(false) {
            ui.painter().circle_stroke(c, 5.0, Stroke::new(1.5, color));
        } else {
            ui.painter().circle_filled(c, 5.5, color);
            ui.painter().circle_filled(c, 2.0, theme.editor_bg);
        }
        let access = match w.access {
            WatchAccess::Write => "on change",
            WatchAccess::Read => "on read",
            WatchAccess::ReadWrite => "on access",
        };
        let mut text = format!("{}  ({access})", w.name);
        if let Some(d) = &w.description {
            text.push_str(&format!("  {d}"));
        }
        let status = match (w.verified, &w.message) {
            (_, Some(m)) => Some((m.clone(), theme.warning)),
            (None, None) => Some(("not set (no session)".to_owned(), theme.text_muted)),
            _ => None,
        };
        let resp = ui.label(RichText::new(text).color(if w.enabled { theme.text } else { theme.text_muted }));
        if let Some((status, color)) = status {
            ui.label(RichText::new(status).color(color).size(11.0));
        }
        resp.context_menu(|ui| {
            if ui.button("Remove").clicked() {
                cmds.push(("debug.watchpoint.remove", json!({ "id": w.id })));
                ui.close();
            }
        });
        if ui.small_button("✖").on_hover_text("Remove watchpoint").clicked() {
            cmds.push(("debug.watchpoint.remove", json!({ "id": w.id })));
        }
    });
}

// ---------------------------------------------------------------- Core Dump / Remote Debug

/// The chooser's entries for post-mortem and remote debugging, and the open form.
pub fn chooser_forms(ui: &mut Ui, s: &Session, theme: &Theme, view: &mut ViewsUi, cmds: &mut Vec<Cmd>) {
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.selectable_label(s.debug.form == Some(DebugForm::CoreDump), "Debug Core Dump…").clicked() {
            cmds.push(("debug.coreDump", Value::Null));
        }
        if ui
            .selectable_label(s.debug.form == Some(DebugForm::Remote), "Remote Debug (gdbserver / lldb-server)…")
            .clicked()
        {
            cmds.push(("debug.remote", Value::Null));
        }
        if ui
            .selectable_label(s.debug.form == Some(DebugForm::PythonRemote), "Attach to Python Debug Server…")
            .clicked()
        {
            cmds.push(("debug.pythonRemote", Value::Null));
        }
    });
    let Some(form) = s.debug.form else { return };
    ui.add_space(4.0);
    let field = |ui: &mut Ui, text: &mut String, hint: &str| {
        ui.add(TextEdit::singleline(text).desired_width(f32::INFINITY).font(FontId::monospace(13.0)).hint_text(hint))
    };
    egui::Grid::new("debug-form").num_columns(2).spacing(vec2(8.0, 4.0)).show(ui, |ui| {
        match form {
            DebugForm::CoreDump => {
                ui.label(RichText::new("Core file").color(theme.text_muted));
                field(ui, &mut view.core, "/path/to/core");
                ui.end_row();
            }
            DebugForm::Remote => {
                ui.label(RichText::new("Connect to").color(theme.text_muted));
                field(ui, &mut view.address, "host:port (gdbserver, lldb-server gdbserver, debugserver)");
                ui.end_row();
            }
            DebugForm::PythonRemote => {
                ui.label(RichText::new("Connect to").color(theme.text_muted));
                field(ui, &mut view.address, "host:port of `python -m debugpy --listen host:port …`");
                ui.end_row();
                ui.label(RichText::new("Path mappings").color(theme.text_muted));
                field(ui, &mut view.mappings, "/local/dir=/remote/dir; … (optional)");
                ui.end_row();
                return;
            }
        }
        ui.label(RichText::new("Program").color(theme.text_muted));
        field(ui, &mut view.program, "local executable with symbols (optional)");
        ui.end_row();
    });
    let program = view.program.trim();
    let program = (!program.is_empty()).then(|| program.to_owned());
    let (id, params, ready) = match form {
        DebugForm::CoreDump => {
            let core = view.core.trim().to_owned();
            let ready = !core.is_empty();
            ("debug.coreDump", json!({ "core": core, "program": program }), ready)
        }
        DebugForm::Remote => {
            let address = view.address.trim().to_owned();
            let ready = !address.is_empty();
            ("debug.remote", json!({ "address": address, "program": program }), ready)
        }
        DebugForm::PythonRemote => {
            let address = view.address.trim().to_owned();
            let ready = !address.is_empty();
            let mappings: serde_json::Map<String, Value> = view
                .mappings
                .split(';')
                .filter_map(|m| m.split_once('='))
                .filter(|(l, r)| !l.trim().is_empty() && !r.trim().is_empty())
                .map(|(l, r)| (l.trim().to_owned(), Value::String(r.trim().to_owned())))
                .collect();
            ("debug.pythonRemote", json!({ "address": address, "pathMappings": mappings }), ready)
        }
    };
    if ui.add_enabled(ready, egui::Button::new("Debug")).clicked() {
        let params = match params {
            Value::Object(mut map) => {
                map.retain(|_, v| !v.is_null());
                Value::Object(map)
            }
            other => other,
        };
        cmds.push((id, params));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_engine::debug_views::{Register, RegisterGroup};

    fn frame(s: &Session, view: &mut ViewsUi, cmds: &mut Vec<Cmd>) {
        let ctx = egui::Context::default();
        let theme = Theme::dark();
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                memory(ui, s, &theme, view, cmds);
                disassembly(ui, s, &theme, view, cmds);
                registers(ui, s, &theme, cmds);
                watchpoints(ui, s, &theme, cmds);
                chooser_forms(ui, s, &theme, view, cmds);
            });
            out.textures_delta.clear();
        }
    }

    #[test]
    fn views_paint_empty_and_full_state() {
        let mut s = Session::new();
        s.lsp_enabled = false;
        let mut view = ViewsUi::default();
        let mut cmds = Vec::new();
        frame(&s, &mut view, &mut cmds);
        assert!(cmds.is_empty());

        let v = &mut s.debug.views;
        v.memory.address = Some(u64::MAX - 20);
        v.memory.bytes = (0..256).map(|i| if i % 7 == 0 { None } else { Some(i as u8) }).collect();
        v.memory.changed.insert(3);
        v.memory.error = Some("partly unreadable".into());
        v.disassembly.current = Some(0x1004);
        v.disassembly.rows = vec![
            DisassemblyRow::Symbol { name: "main".into() },
            DisassemblyRow::Source { path: Some("/a/t.c".into()), line: Some(usize::MAX), text: "x++;".into() },
            DisassemblyRow::Instruction {
                address: 0x1000,
                bytes: Some("1f 20 03 d5".into()),
                text: "nop".into(),
                offset: None,
            },
            DisassemblyRow::Instruction { address: 0x1004, bytes: None, text: "".into(), offset: Some("<+4>".into()) },
        ];
        v.registers.groups = vec![RegisterGroup {
            name: "General".into(),
            registers: vec![
                Register { name: "pc".into(), value: "0x0000000000001004".into(), changed: true },
                Register { name: "x0".into(), value: "1".into(), changed: false },
            ],
        }];
        v.watchpoints.push(Watchpoint {
            id: 1,
            name: "counter".into(),
            expression: "counter".into(),
            access: WatchAccess::ReadWrite,
            enabled: false,
            description: None,
            verified: Some(false),
            message: Some("no hardware slots".into()),
            data_id: None,
            lldb_id: None,
        });
        s.debug.form = Some(DebugForm::Remote);
        view.edit_at = Some(u64::MAX);
        frame(&s, &mut view, &mut cmds);
        s.debug.form = Some(DebugForm::CoreDump);
        frame(&s, &mut view, &mut cmds);
    }
}
