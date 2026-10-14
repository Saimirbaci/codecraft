//! The Jupyter notebook editor (M12 track PY): a toolbar, the cells (code with highlighting,
//! Markdown rendered until edited, raw), their outputs (text, errors, `image/png`), the input
//! field of a cell waiting in `input()`, and the kernel's Variables. Thin: every change is a
//! `notebook.*` command; the engine owns the notebook, the kernel and the run queue.
//!
//! Keys: Shift+Enter runs the cell and moves on, Cmd/Ctrl+Enter runs it in place, Escape leaves
//! the cell; with no cell focused, ↑/↓ select, Enter edits and Shift+Enter runs.

use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, CornerRadius, FontId, Frame, Id, Key, Layout, Margin, Modifiers, RichText, ScrollArea, Stroke, TextEdit, TextureHandle, vec2};
use serde_json::{Map, Value, json};

use codecraft_engine::Session;
use codecraft_engine::jupyter::NotebookDoc;
use codecraft_engine::notebook::{self, CellKind, OutputView};
use codecraft_syntax::Language;
use codecraft_workspace::{DocId, EditorId};

use crate::theme::Theme;
use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// Characters of one output shown (the rest is clipped; the file keeps everything).
const MAX_OUTPUT_CHARS: usize = 100_000;

fn cell_id(doc: DocId, uid: u64) -> Id {
    Id::new(("notebook-cell", doc.0, uid))
}

fn focus_key(doc: DocId) -> Id {
    Id::new(("notebook-focus-selected", doc.0))
}

/// `text` as a highlighted layout job (the text is kept exactly, as TextEdit requires).
pub fn highlight_job(text: &str, language: Language, theme: &Theme, font: &FontId, wrap: f32) -> LayoutJob {
    let hl = codecraft_syntax::highlight(language, text);
    let mut job = LayoutJob::default();
    job.wrap.max_width = wrap;
    let fmt = |color: Color32| TextFormat { font_id: font.clone(), color, ..Default::default() };
    for (li, line) in text.split('\n').enumerate() {
        if li > 0 {
            job.append("\n", 0.0, fmt(theme.text_editor));
        }
        let chars: Vec<char> = line.chars().collect();
        let piece = |a: usize, b: usize| -> String { chars.get(a..b).map(|c| c.iter().collect()).unwrap_or_default() };
        let mut col = 0;
        for span in hl.line(li) {
            let (a, b) = (span.start.min(chars.len()), span.end.min(chars.len()));
            if a > col {
                job.append(&piece(col, a), 0.0, fmt(theme.text_editor));
            }
            if b > a.max(col) {
                job.append(&piece(a.max(col), b), 0.0, fmt(theme.syntax(span.kind)));
            }
            col = col.max(b);
        }
        if col < chars.len() {
            job.append(&piece(col, chars.len()), 0.0, fmt(theme.text_editor));
        }
    }
    job
}

/// A base64 PNG/JPEG as a texture, decoded once and cached in egui's memory.
fn image_texture(ui: &egui::Ui, key: Id, b64: &str) -> Option<TextureHandle> {
    if let Some(cached) = ui.data(|d| d.get_temp::<Option<TextureHandle>>(key)) {
        return cached;
    }
    let texture = notebook::decode_base64(b64).ok().and_then(|bytes| {
        if bytes.len() > 32 << 20 {
            return None;
        }
        let img = image::load_from_memory(&bytes).ok()?;
        if img.width() > 8192 || img.height() > 8192 {
            return None;
        }
        let rgba = img.to_rgba8();
        let size = [usize::try_from(rgba.width()).ok()?, usize::try_from(rgba.height()).ok()?];
        let color = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        Some(ui.ctx().load_texture(format!("{key:?}"), color, egui::TextureOptions::LINEAR))
    });
    ui.data_mut(|d| d.insert_temp(key, texture.clone()));
    texture
}

fn with_path(n: &NotebookDoc, extra: Value) -> Value {
    let mut m = match extra {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    if let Some(p) = &n.path {
        m.insert("path".into(), json!(p));
    }
    Value::Object(m)
}

/// The notebook editor for `doc` (shown by `editor::show` instead of the text).
pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, eid: EditorId, doc: DocId) {
    // The raw JSON editor must never take the keyboard while the notebook shows.
    let raw_id = crate::editor::editor_id(eid);
    if ui.memory(|m| m.has_focus(raw_id)) {
        ui.memory_mut(|m| m.surrender_focus(raw_id));
    }
    let Some(i) = s.jupyter.index_of_doc(doc) else { return };
    let theme = st.theme.clone();
    let mono = FontId::monospace(s.ws.settings.editor_font_size.clamp(8.0, 40.0));
    let mut cmds: Cmds = Vec::new();

    let Some(n) = s.jupyter.notebooks.get(i) else { return };
    toolbar(ui, n, &theme, &mut cmds);
    ui.separator();
    variables_panel(ui, n, doc, &theme, &mut cmds);

    // The answer to input().
    if let Some(n) = s.jupyter.notebooks.get_mut(i)
        && let Some(prompt) = n.awaiting_input.clone()
    {
        egui::Panel::bottom(Id::new(("notebook-input", doc.0))).show(ui, |ui| {
            ui.horizontal(|ui| {
                let label = if prompt.is_empty() { "input:".to_string() } else { prompt };
                ui.label(RichText::new(label).monospace().color(theme.accent));
                let id = Id::new(("notebook-input-field", doc.0));
                let resp = ui.add(TextEdit::singleline(&mut n.input_draft).id(id).font(mono.clone()).desired_width(f32::INFINITY));
                if !resp.has_focus() && !ui.memory(|m| m.focused().is_some()) {
                    resp.request_focus();
                }
                if resp.lost_focus() && ui.input(|inp| inp.key_pressed(Key::Enter)) {
                    cmds.push(("notebook.sendInput", Value::Null));
                }
            });
        });
    }

    let Some(n) = s.jupyter.notebooks.get(i) else { return };
    keyboard(ui, n, doc, &mut cmds);
    let language = match n.nb.as_ref().map(|nb| nb.language()) {
        Some(l) if l == "python" || l.is_empty() => Language::Python,
        Some(l) => Language::from_path(std::path::Path::new(&format!("x.{l}"))),
        None => Language::Python,
    };
    let focus_selected = ui.data_mut(|d| d.remove_temp::<bool>(focus_key(doc))).unwrap_or(false);
    ScrollArea::vertical().id_salt(("notebook-cells", doc.0)).auto_shrink([false, false]).show(ui, |ui| {
        let Some(nb) = &n.nb else { return };
        for (idx, cell) in nb.cells.iter().enumerate() {
            cell_view(ui, n, doc, idx, cell, language, &theme, &mono, focus_selected, &mut cmds);
            ui.add_space(6.0);
        }
        ui.horizontal(|ui| {
            if ui.button("Add Code Cell").clicked() {
                cmds.push(("notebook.addCellBelow", json!({ "index": nb.cells.len().saturating_sub(1), "type": "code" })));
            }
            if ui.button("Add Markdown Cell").clicked() {
                cmds.push(("notebook.addCellBelow", json!({ "index": nb.cells.len().saturating_sub(1), "type": "markdown" })));
            }
        });
        ui.add_space(40.0);
    });

    let focus_after = cmds.iter().any(|(id, p)| {
        matches!(*id, "notebook.addCellAbove" | "notebook.addCellBelow")
            || (*id == "notebook.runCell" && p.get("advance").and_then(Value::as_bool) == Some(true))
    });
    let path = s.jupyter.notebooks.get(i).map(|n| with_path(n, Value::Null));
    for (id, p) in cmds {
        let mut p = if p.is_null() { Value::Object(Map::new()) } else { p };
        if let (Some(m), Some(Value::Object(pm))) = (p.as_object_mut(), &path) {
            for (k, v) in pm {
                m.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        run_command(s, st, id, &p);
    }
    if focus_after {
        ui.data_mut(|d| d.insert_temp(focus_key(doc), true));
    }
}

fn toolbar(ui: &mut egui::Ui, n: &NotebookDoc, theme: &Theme, cmds: &mut Cmds) {
    ui.horizontal(|ui| {
        let busy = n.running.is_some() || !n.queue.is_empty();
        if ui.button("▶").on_hover_text("Run Cell (Shift+Enter)").clicked() {
            cmds.push(("notebook.runCell", json!({ "advance": true })));
        }
        if ui.button("⏩").on_hover_text("Run All").clicked() {
            cmds.push(("notebook.runAll", Value::Null));
        }
        if ui.button("⤒").on_hover_text("Run All Above").clicked() {
            cmds.push(("notebook.runAbove", Value::Null));
        }
        if ui.button("⤓").on_hover_text("Run Cell and Below").clicked() {
            cmds.push(("notebook.runBelow", Value::Null));
        }
        if ui.add_enabled(busy, egui::Button::new("⏸")).on_hover_text("Interrupt Kernel").clicked() {
            cmds.push(("notebook.interruptKernel", Value::Null));
        }
        if ui.button("⟲").on_hover_text("Restart Kernel").clicked() {
            cmds.push(("notebook.restartKernel", Value::Null));
        }
        if ui.add_enabled(n.kernel.is_alive(), egui::Button::new("■")).on_hover_text("Shut Down Kernel").clicked() {
            cmds.push(("notebook.shutdownKernel", Value::Null));
        }
        ui.separator();
        if ui.button("+ Code").on_hover_text("Add a code cell below").clicked() {
            cmds.push(("notebook.addCellBelow", json!({ "type": "code" })));
        }
        if ui.button("+ Markdown").on_hover_text("Add a Markdown cell below").clicked() {
            cmds.push(("notebook.addCellBelow", json!({ "type": "markdown" })));
        }
        if ui.button("Clear Outputs").on_hover_text("Clear All Outputs").clicked() {
            cmds.push(("notebook.clearAllOutputs", Value::Null));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let status = n.kernel_status();
            let color = match status {
                "idle" => theme.success,
                "busy" | "starting" => theme.warning,
                _ => theme.text_muted,
            };
            let version = n.kernel.version.as_deref().map(|v| format!("Python {v}")).unwrap_or_else(|| "Python".into());
            let tip = n.kernel.interpreter.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "starts on the first run".into());
            ui.label(RichText::new(format!("{version} · {status}")).small().color(color)).on_hover_text(tip);
            if let Some(exit) = &n.kernel_exit {
                ui.label(RichText::new(exit).small().color(theme.error));
            }
        });
    });
}

fn variables_panel(ui: &mut egui::Ui, n: &NotebookDoc, doc: DocId, theme: &Theme, cmds: &mut Cmds) {
    egui::Panel::right(Id::new(("notebook-vars", doc.0))).resizable(true).default_size(240.0).show(ui, |ui| {
        ui.label(RichText::new("Variables").strong().small());
        ScrollArea::vertical().id_salt(("notebook-vars-scroll", doc.0)).auto_shrink([false, false]).show(ui, |ui| {
            if n.vars.is_empty() {
                ui.label(RichText::new("Run a cell to see its variables").color(theme.text_muted).small());
            }
            egui::Grid::new(("notebook-vars-grid", doc.0)).num_columns(2).striped(true).show(ui, |ui| {
                for v in &n.vars {
                    ui.label(RichText::new(&v.name).monospace()).on_hover_text(&v.type_name);
                    ui.horizontal(|ui| {
                        if v.viewable
                            && ui.small_button("⊞ View").on_hover_text("View as DataFrame / Array").clicked()
                        {
                            cmds.push(("python.dataframe.view", json!({ "expression": v.name, "source": "notebook" })));
                        }
                        let shape = v.shape.as_deref().map(|s| format!(" {s}")).unwrap_or_default();
                        ui.label(
                            RichText::new(format!("{{{}{shape}}} {}", v.type_name, v.value)).monospace().color(theme.text_muted),
                        )
                        .on_hover_text(&v.value);
                    });
                    ui.end_row();
                }
            });
        });
    });
}

/// Command-mode keys (no cell focused) and the run keys of a focused cell.
fn keyboard(ui: &mut egui::Ui, n: &NotebookDoc, doc: DocId, cmds: &mut Cmds) {
    let Some(nb) = &n.nb else { return };
    let focused_cell = nb.cells.iter().position(|c| ui.memory(|m| m.has_focus(cell_id(doc, c.uid))));
    match focused_cell {
        Some(idx) => {
            if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter)) {
                cmds.push(("notebook.runCell", json!({ "index": idx, "advance": true })));
                if let Some(c) = nb.cells.get(idx) {
                    ui.memory_mut(|m| m.surrender_focus(cell_id(doc, c.uid)));
                }
            } else if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) {
                cmds.push(("notebook.runCell", json!({ "index": idx })));
            } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
                && let Some(c) = nb.cells.get(idx)
            {
                ui.memory_mut(|m| m.surrender_focus(cell_id(doc, c.uid)));
            }
            if n.selected != idx {
                cmds.push(("notebook.selectCell", json!({ "index": idx })));
            }
        }
        None if ui.memory(|m| m.focused().is_none()) && ui.ui_contains_pointer() => {
            if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter)) {
                cmds.push(("notebook.runCell", json!({ "advance": true })));
            } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                if nb.cells.get(n.selected).is_some_and(|c| c.kind == CellKind::Markdown) {
                    cmds.push(("notebook.editMarkdown", Value::Null));
                }
                ui.data_mut(|d| d.insert_temp(focus_key(doc), true));
            } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) && n.selected > 0 {
                cmds.push(("notebook.selectCell", json!({ "index": n.selected - 1 })));
            } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) && n.selected + 1 < nb.cells.len() {
                cmds.push(("notebook.selectCell", json!({ "index": n.selected + 1 })));
            }
        }
        None => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn cell_view(
    ui: &mut egui::Ui,
    n: &NotebookDoc,
    doc: DocId,
    idx: usize,
    cell: &notebook::Cell,
    language: Language,
    theme: &Theme,
    mono: &FontId,
    focus_selected: bool,
    cmds: &mut Cmds,
) {
    let selected = n.selected == idx;
    let stroke = if selected { Stroke::new(1.5, theme.accent) } else { Stroke::new(1.0, theme.border) };
    ui.horizontal_top(|ui| {
        // Execution count gutter.
        let count = if n.is_pending(cell.uid) {
            "[*]".to_string()
        } else if cell.kind == CellKind::Code {
            cell.execution_count.map(|c| format!("[{c}]")).unwrap_or_else(|| "[ ]".into())
        } else {
            String::new()
        };
        ui.allocate_ui_with_layout(vec2(44.0, 20.0), Layout::right_to_left(Align::TOP), |ui| {
            let color = if n.is_running(cell.uid) { theme.warning } else { theme.line_number };
            ui.label(RichText::new(count).monospace().color(color));
        });
        let frame = Frame::new()
            .fill(theme.editor_bg)
            .stroke(stroke)
            .corner_radius(CornerRadius::same(4))
            .inner_margin(Margin::same(6));
        let resp = frame
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                if selected {
                    cell_actions(ui, idx, cell, cmds);
                }
                cell_source(ui, n, doc, idx, cell, language, theme, mono, selected && focus_selected, cmds);
                if cell.kind == CellKind::Code && !cell.outputs.is_empty() {
                    ui.separator();
                    for (oi, o) in cell.outputs.iter().enumerate() {
                        output_view(ui, doc, cell.uid, oi, o, theme, mono);
                    }
                }
            })
            .response;
        if !selected && resp.interact(egui::Sense::click()).clicked() {
            cmds.push(("notebook.selectCell", json!({ "index": idx })));
        }
    });
}

fn cell_actions(ui: &mut egui::Ui, idx: usize, cell: &notebook::Cell, cmds: &mut Cmds) {
    ui.horizontal(|ui| {
        let at = json!({ "index": idx });
        if ui.small_button("▶").on_hover_text("Run Cell").clicked() {
            cmds.push(("notebook.runCell", at.clone()));
        }
        if ui.small_button("↑").on_hover_text("Move Cell Up").clicked() {
            cmds.push(("notebook.moveCellUp", at.clone()));
        }
        if ui.small_button("↓").on_hover_text("Move Cell Down").clicked() {
            cmds.push(("notebook.moveCellDown", at.clone()));
        }
        if ui.small_button("+ Above").on_hover_text("Add Cell Above").clicked() {
            cmds.push(("notebook.addCellAbove", at.clone()));
        }
        if ui.small_button("+ Below").on_hover_text("Add Cell Below").clicked() {
            cmds.push(("notebook.addCellBelow", at.clone()));
        }
        let mut kind = cell.kind;
        egui::ComboBox::from_id_salt(("notebook-cell-type", cell.uid))
            .selected_text(match kind {
                CellKind::Code => "Code",
                CellKind::Markdown => "Markdown",
                CellKind::Raw => "Raw",
            })
            .width(90.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut kind, CellKind::Code, "Code");
                ui.selectable_value(&mut kind, CellKind::Markdown, "Markdown");
                ui.selectable_value(&mut kind, CellKind::Raw, "Raw");
            });
        if kind != cell.kind {
            cmds.push(("notebook.setCellType", json!({ "index": idx, "type": kind.as_str() })));
        }
        if cell.kind == CellKind::Code
            && !cell.outputs.is_empty()
            && ui.small_button("Clear").on_hover_text("Clear Outputs").clicked()
        {
            cmds.push(("notebook.clearOutputs", at.clone()));
        }
        if ui.small_button("🗑").on_hover_text("Delete Cell").clicked() {
            cmds.push(("notebook.deleteCell", at));
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn cell_source(
    ui: &mut egui::Ui,
    n: &NotebookDoc,
    doc: DocId,
    idx: usize,
    cell: &notebook::Cell,
    language: Language,
    theme: &Theme,
    mono: &FontId,
    focus: bool,
    cmds: &mut Cmds,
) {
    let id = cell_id(doc, cell.uid);
    let rendered_markdown = cell.kind == CellKind::Markdown && !n.editing.contains(&cell.uid) && !cell.source.trim().is_empty();
    if rendered_markdown {
        let resp = ui
            .scope(|ui| crate::markdown::render(ui, &cell.source, theme, mono.size))
            .response
            .interact(egui::Sense::click());
        if resp.double_clicked() {
            cmds.push(("notebook.editMarkdown", json!({ "index": idx })));
        } else if resp.clicked() {
            cmds.push(("notebook.selectCell", json!({ "index": idx })));
        }
        return;
    }
    let lang = match cell.kind {
        CellKind::Code => language,
        CellKind::Markdown => Language::Markdown,
        CellKind::Raw => Language::PlainText,
    };
    let mut text = cell.source.clone();
    let font = mono.clone();
    let t = theme.clone();
    let mut layouter = move |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap: f32| -> Arc<egui::Galley> {
        let job = highlight_job(buf.as_str(), lang, &t, &font, wrap);
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let rows = text.lines().count().max(1);
    let hint = match cell.kind {
        CellKind::Code => "Code (Shift+Enter runs)",
        CellKind::Markdown => "Markdown (Shift+Enter renders)",
        CellKind::Raw => "Raw text",
    };
    let resp = ui.add(
        TextEdit::multiline(&mut text)
            .id(id)
            .code_editor()
            .desired_rows(rows)
            .desired_width(f32::INFINITY)
            .frame(Frame::NONE)
            .hint_text(hint)
            .layouter(&mut layouter),
    );
    if focus {
        resp.request_focus();
    }
    if resp.changed() && text != cell.source {
        cmds.push(("notebook.setCellSource", json!({ "index": idx, "source": text })));
    }
}

fn output_view(ui: &mut egui::Ui, doc: DocId, uid: u64, oi: usize, o: &Value, theme: &Theme, mono: &FontId) {
    let text_block = |ui: &mut egui::Ui, text: &str, color: Color32| {
        let (shown, cut) = notebook::clip(text, MAX_OUTPUT_CHARS);
        let shown = shown.strip_suffix('\n').unwrap_or(shown);
        ui.add(egui::Label::new(RichText::new(shown).font(mono.clone()).color(color)).selectable(true).wrap());
        if cut {
            ui.label(RichText::new("… output clipped (the notebook keeps all of it)").small().color(theme.text_muted));
        }
    };
    match notebook::output_view(o) {
        OutputView::Stream { stderr, text } => {
            let color = if stderr { theme.console_stderr } else { theme.text_editor };
            text_block(ui, text, color);
        }
        OutputView::Data { execution_count, data } => {
            if let Some(c) = execution_count {
                ui.label(RichText::new(format!("Out[{c}]:")).small().color(theme.line_number));
            }
            let image = ["image/png", "image/jpeg"].iter().find_map(|m| data.get(*m).and_then(Value::as_str));
            if let Some(b64) = image {
                let key = Id::new(("notebook-image", doc.0, uid, oi, b64.len()));
                match image_texture(ui, key, b64) {
                    Some(t) => {
                        let size = t.size_vec2();
                        let scale = (ui.available_width() / size.x.max(1.0)).min(1.0);
                        ui.image((t.id(), size * scale));
                    }
                    None => {
                        ui.label(RichText::new("<image could not be decoded>").color(theme.text_muted));
                    }
                }
                return;
            }
            if let Some(md) = notebook::mime_text(data, "text/markdown") {
                crate::markdown::render(ui, &md, theme, mono.size);
                return;
            }
            // HTML is shown as text: its plain-text form when there is one, else the source.
            let text = notebook::mime_text(data, "text/plain")
                .or_else(|| notebook::mime_text(data, "text/html"))
                .or_else(|| data.keys().next().and_then(|k| notebook::mime_text(data, k)))
                .unwrap_or_default();
            text_block(ui, &text, theme.text_editor);
        }
        OutputView::Error { ename, evalue, traceback } => {
            ui.label(RichText::new(format!("{ename}: {evalue}")).font(mono.clone()).color(theme.error).strong());
            let tb = notebook::strip_ansi(&traceback.join("\n"));
            if !tb.trim().is_empty() && tb.trim() != format!("{ename}: {evalue}") {
                text_block(ui, &tb, theme.console_stderr);
            }
        }
        OutputView::Other { output_type } => {
            ui.label(RichText::new(format!("<{output_type} output>")).color(theme.text_muted));
        }
    }
}
