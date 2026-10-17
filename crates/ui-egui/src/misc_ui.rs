//! Menu-sweep windows (M10 track F): the Database Changes tool window, the Processes window
//! (Stop Background Processes, Show Running List, Window › Processes), Compare With (file chooser
//! and diff), the File Encoding chooser, Associate with File Type, Show Hidden Tabs and the
//! GitHub/GitLab accounts window. All actions go through engine commands.

use std::path::PathBuf;

use egui::{Align2, Id, Key, RichText, ScrollArea, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::misc::{self, ProcessesMode};
use codecraft_workspace::TextEncoding;

use crate::diff_view::{self, DiffOptions};
use crate::theme::Theme;
use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// Whether one of these windows is open: they are modal and read the keyboard themselves
/// (Escape closes them), so the editor must not get the keys.
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.misc.compare_picker.is_some()
        || s.misc.associate.is_some()
        || s.misc.hidden_tabs
        || s.misc.encoding_dialog
        || s.misc.compare.is_some()
        || s.misc.processes.is_some()
        || s.misc.accounts.is_some()
}

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

/// Every window of this module, plus the per-frame sync of the export palette and the system
/// clipboard for Compare with Clipboard.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    sync_palette(s, &st.theme);
    clipboard_request(s, st);
    processes_window(&ctx, s, st);
    compare_picker(&ctx, s, st);
    compare_window(&ctx, s, st);
    encoding_window(&ctx, s, st);
    associate_window(&ctx, s, st);
    hidden_tabs(&ctx, s, st);
    accounts_window(&ctx, s, st);
}

/// Keep the engine's HTML export palette equal to the theme's colour tokens.
fn sync_palette(s: &mut Session, theme: &Theme) {
    let probe = |name: &str| theme.token(name).map(crate::theme::to_hex);
    let stale = ["editor_bg", "keyword", "text_editor", "comment"]
        .iter()
        .any(|t| s.misc.palette.get(*t).cloned() != probe(t));
    if stale {
        s.misc.palette = Theme::TOKENS.iter().filter_map(|t| Some(((*t).to_string(), probe(t)?))).collect();
    }
}

fn clipboard_request(s: &mut Session, st: &mut UiState) {
    if !s.misc.clipboard_request {
        return;
    }
    s.misc.clipboard_request = false;
    #[cfg(target_os = "macos")]
    if let Some(text) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()) {
        run_command(s, st, "view.compareWithClipboard", &json!({ "text": text }));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = st;
}

fn escape_pressed(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_pressed(Key::Escape))
}

// ---------------------------------------------------------------- Database Changes

/// View › Tool Windows › Database Changes: pending grid edits of every open table result, with
/// Submit / Revert per table and for all.
pub(crate) fn show_database_changes(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let pending = misc::database_changes(s);
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} table(s) with pending changes", pending.len())).color(theme.text_muted).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let any = !pending.is_empty();
            if ui.add_enabled(any, egui::Button::new(RichText::new("Revert All").small())).clicked() {
                cmds.push(("databaseChanges.revertAll", Value::Null));
            }
            if ui.add_enabled(any, egui::Button::new(RichText::new("Submit All").small())).clicked() {
                cmds.push(("databaseChanges.submitAll", Value::Null));
            }
        });
    });
    if pending.is_empty() {
        ui.label(
            RichText::new("No pending changes. Edit cells of a table opened from the Database tool window.")
                .color(theme.text_muted),
        );
    }
    ScrollArea::vertical().id_salt("db-changes").auto_shrink([false, false]).show(ui, |ui| {
        for t in &pending {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&t.title).strong());
                ui.label(
                    RichText::new(format!("{} · {} row(s) changed", t.source, t.pending_rows))
                        .color(theme.text_muted)
                        .small(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!t.submitting, egui::Button::new(RichText::new("Revert").small())).clicked() {
                        cmds.push(("db.revertChanges", json!({ "result": t.result })));
                    }
                    let can_submit = !t.submitting && t.not_editable.is_none();
                    let submit = ui.add_enabled(can_submit, egui::Button::new(RichText::new("Submit").small()));
                    if let Some(e) = &t.not_editable {
                        submit.on_disabled_hover_text(e);
                    } else if submit.clicked() {
                        cmds.push(("db.submitChanges", json!({ "result": t.result })));
                    }
                    if ui.button(RichText::new("Show").small()).on_hover_text("Show the table in Services").clicked() {
                        cmds.push(("db.selectResult", json!({ "result": t.result })));
                    }
                });
            });
            for stmt in t.statements.iter().take(50) {
                let line: String = stmt.lines().next().unwrap_or_default().chars().take(200).collect();
                ui.label(RichText::new(format!("  {line}")).monospace().small().color(theme.text_muted));
                // Bound values follow the statement on their own line.
                if let Some(values) = stmt.lines().find(|l| l.starts_with("-- parameters:")) {
                    let values: String = values.chars().take(200).collect();
                    ui.label(RichText::new(format!("    {values}")).monospace().small().color(theme.text_muted));
                }
            }
            if t.statements.len() > 50 {
                ui.label(RichText::new(format!("  … {} more", t.statements.len() - 50)).small().color(theme.text_muted));
            }
            if let Some(e) = &t.error {
                ui.label(RichText::new(e).color(theme.error).small());
            }
            ui.separator();
        }
    });
    if cmds.iter().any(|(id, _)| *id == "db.selectResult") {
        s.ws.layout.show(codecraft_workspace::ToolWindowId::Services);
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- Processes

fn processes_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(mode) = s.misc.processes else { return };
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut list = misc::processes(s);
    if mode == ProcessesMode::Running {
        list.retain(|p| p.kind == "run" || p.kind == "debug");
    }
    let title = match mode {
        ProcessesMode::Stop => "Stop Background Processes",
        ProcessesMode::Running => "Running",
        ProcessesMode::Tasks => "Processes",
    };
    let mut open = true;
    egui::Window::new(title)
        .id(Id::new("misc-processes"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(520.0, 260.0))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            if mode == ProcessesMode::Tasks {
                if s.ws.tasks.is_empty() {
                    ui.label(RichText::new("No background tasks.").color(theme.text_muted));
                }
                for t in s.ws.tasks.values() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        let mut text = t.title.clone();
                        if let Some(m) = &t.message {
                            text = format!("{text}: {m}");
                        }
                        if let Some(p) = t.percentage {
                            text = format!("{text} ({p}%)");
                        }
                        ui.label(text);
                    });
                }
                ui.separator();
            }
            if list.is_empty() {
                ui.label(RichText::new("Nothing is running.").color(theme.text_muted));
            }
            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for p in &list {
                    ui.horizontal(|ui| {
                        if ui.button("■ Stop").clicked() {
                            cmds.push(("processes.stop", json!({ "kind": p.kind, "id": p.id })));
                        }
                        let resp = ui.selectable_label(false, RichText::new(&p.title).strong());
                        if p.kind == "run"
                            && resp.clicked()
                            && let Ok(id) = p.id.parse::<u64>()
                        {
                            s.ws.active_run = Some(codecraft_workspace::RunTabId(id));
                            s.ws.layout.show(codecraft_workspace::ToolWindowId::Run);
                        }
                        let detail: String = p.detail.chars().take(120).collect();
                        ui.label(RichText::new(format!("{} · {detail}", p.kind)).color(theme.text_muted).small());
                    });
                }
            });
            if mode != ProcessesMode::Running && list.len() > 1 {
                ui.separator();
                if ui.button("Stop All").clicked() {
                    cmds.push(("processes.stopAll", Value::Null));
                }
            }
        });
    if !open || escape_pressed(ctx) {
        cmds.push(("processes.close", Value::Null));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- Compare With

/// Project files matching `query` (cached per query in egui memory).
fn matching_files(ctx: &egui::Context, s: &Session, query: &str) -> Vec<(PathBuf, String)> {
    let Some(p) = &s.ws.project else { return Vec::new() };
    let key = Id::new("misc-compare-files");
    if let Some((q, v)) = ctx.data(|d| d.get_temp::<(String, Vec<(PathBuf, String)>)>(key))
        && q == query
    {
        return v;
    }
    let files: Vec<(PathBuf, String)> =
        p.tree.files().map(|n| (n.path.clone(), p.relative(&n.path).into_owned())).collect();
    let ranked = codecraft_fs::rank(query, files.iter().map(|(_, r)| r.as_str()), 50);
    let out: Vec<(PathBuf, String)> = ranked.into_iter().filter_map(|(i, _)| files.get(i).cloned()).collect();
    ctx.data_mut(|d| d.insert_temp(key, (query.to_string(), out.clone())));
    out
}

fn compare_picker(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(mut query) = s.misc.compare_picker.clone() else { return };
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let files = matching_files(ctx, s, &query);
    egui::Window::new("Compare With…")
        .id(Id::new("misc-compare-picker"))
        .open(&mut open)
        .collapsible(false)
        .default_size(vec2(520.0, 360.0))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            let edit = ui.add(egui::TextEdit::singleline(&mut query).hint_text("File name").desired_width(f32::INFINITY));
            edit.request_focus();
            let enter = ui.input(|i| i.key_pressed(Key::Enter));
            ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                for (i, (path, rel)) in files.iter().enumerate() {
                    let clicked = ui.selectable_label(i == 0, rel).clicked();
                    if clicked || (enter && i == 0) {
                        cmds.push(("view.compareWith", json!({ "path": path })));
                    }
                }
                if files.is_empty() {
                    ui.label(RichText::new("No matching files.").color(theme.text_muted));
                }
            });
            if ui.button("Browse…").clicked()
                && let Some(path) = rfd::FileDialog::new().pick_file()
            {
                cmds.push(("view.compareWith", json!({ "path": path })));
            }
        });
    if s.misc.compare_picker.is_some() {
        s.misc.compare_picker = Some(query);
    }
    if !open || escape_pressed(ctx) {
        cmds.push(("compare.close", Value::Null));
    }
    run_all(s, st, cmds);
}

fn compare_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if s.misc.compare_picker.is_some() {
        return;
    }
    let Some(view) = s.misc.compare.as_ref() else { return };
    let theme = st.theme.clone();
    let mut open = true;
    let screen = ctx.content_rect();
    let font = egui::FontId::monospace((s.ws.settings.editor_font_size - 1.0).max(9.0));
    egui::Window::new(view.title.clone())
        .id(Id::new("misc-compare"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(vec2((screen.width() * 0.75).clamp(480.0, 1300.0), (screen.height() * 0.7).clamp(300.0, 900.0)))
        .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label(RichText::new(format!("{}  →  {}", view.left_title, view.right_title)).color(theme.text_muted));
            let stats = diff_view::stats(ui, ("misc-compare", &view.title), &view.left, &view.right);
            ui.horizontal(|ui| diff_view::stats_label(ui, &theme, &stats));
            let opts = DiffOptions { side_by_side: true, context: 3, max_height: (ui.available_height() - 8.0).max(160.0) };
            diff_view::show(ui, &theme, &font, ("misc-compare", &view.title), &view.left, &view.right, &opts);
        });
    if !open || escape_pressed(ctx) {
        run_command(s, st, "compare.close", &Value::Null);
    }
}

// ---------------------------------------------------------------- File Encoding

fn encoding_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if !s.misc.encoding_dialog {
        return;
    }
    let Some((_, d)) = s.ws.active() else {
        s.misc.encoding_dialog = false;
        return;
    };
    let theme = st.theme.clone();
    let (current, bom, modified, text) = (d.encoding, d.bom, d.doc.is_modified(), d.doc.text());
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new("File Encoding")
        .id(Id::new("misc-encoding"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            ui.label(format!("Current: {}{}", current.label(), if bom { " with BOM" } else { "" }));
            ui.add_space(6.0);
            egui::Grid::new("misc-encoding-grid").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
                for enc in TextEncoding::ALL {
                    ui.label(RichText::new(enc.label()).strong());
                    let reload = ui
                        .add_enabled(!modified && enc != current, egui::Button::new("Reload"))
                        .on_hover_text("Read the file again in this encoding");
                    if reload.clicked() {
                        cmds.push(("file.fileProperties.fileEncoding", json!({ "encoding": enc.label(), "mode": "reload" })));
                    }
                    let fits = misc::encoding::can_encode(&text, enc);
                    let convert = ui
                        .add_enabled(fits && enc != current, egui::Button::new("Convert"))
                        .on_hover_text("Save the file in this encoding");
                    if convert.clicked() {
                        cmds.push(("file.fileProperties.fileEncoding", json!({ "encoding": enc.label(), "mode": "convert" })));
                    }
                    ui.end_row();
                }
            });
            if modified {
                ui.label(RichText::new("Save the file to reload it in another encoding.").color(theme.text_muted).small());
            }
        });
    if !open || escape_pressed(ctx) {
        cmds.push(("fileEncoding.close", Value::Null));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- Associate with File Type

fn associate_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(mut dialog) = s.misc.associate.clone() else { return };
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    egui::Window::new("Register New File Type Association")
        .id(Id::new("misc-associate"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            egui::Grid::new("misc-associate-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                ui.label("File name pattern:");
                ui.add(egui::TextEdit::singleline(&mut dialog.pattern).desired_width(220.0));
                ui.end_row();
                ui.label("File type:");
                egui::ComboBox::from_id_salt("misc-associate-lang").selected_text(dialog.language.clone()).show_ui(
                    ui,
                    |ui| {
                        for l in misc::LANGUAGES {
                            ui.selectable_value(&mut dialog.language, l.name().to_string(), l.name());
                        }
                    },
                );
                ui.end_row();
            });
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                    cmds.push((
                        "file.fileProperties.associateWithFileType",
                        json!({ "pattern": dialog.pattern, "language": dialog.language }),
                    ));
                }
                if ui.button("Remove Association").clicked() {
                    cmds.push((
                        "file.fileProperties.associateWithFileType",
                        json!({ "pattern": dialog.pattern, "language": null }),
                    ));
                }
            });
        });
    if s.misc.associate.is_some() {
        s.misc.associate = Some(dialog);
    }
    if !open || escape_pressed(ctx) {
        cmds.push(("associateFileType.close", Value::Null));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- Show Hidden Tabs

fn hidden_tabs(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if !s.misc.hidden_tabs {
        return;
    }
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let active = s.ws.active_editor();
    let tabs: Vec<(u64, String, bool, bool)> = s
        .ws
        .tabs()
        .iter()
        .filter_map(|id| {
            let e = s.ws.editor(*id)?;
            let d = s.ws.doc(e.doc)?;
            let mut title = if d.doc.is_modified() { format!("{} *", d.title()) } else { d.title() };
            if let (Some(p), Some(path)) = (&s.ws.project, d.path.as_deref()) {
                title = format!("{title} — {}", p.relative(path));
            }
            Some((id.0, title, e.preview, active == Some(*id)))
        })
        .collect();
    let area = egui::Area::new(Id::new("misc-hidden-tabs"))
        .order(egui::Order::Foreground)
        .anchor(Align2::RIGHT_TOP, vec2(-24.0, 90.0))
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(240.0);
                ui.label(RichText::new("Tabs").color(theme.text_muted).small());
                ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                    for (id, title, preview, is_active) in &tabs {
                        let mut text = RichText::new(title);
                        if *preview {
                            text = text.italics();
                        }
                        if ui.selectable_label(*is_active, text).clicked() {
                            cmds.push(("window.editorTabs.activate", json!({ "editor": id })));
                            cmds.push(("hiddenTabs.close", Value::Null));
                        }
                    }
                });
            });
        });
    let clicked_outside = area.response.clicked_elsewhere() && cmds.is_empty();
    if escape_pressed(ctx) || clicked_outside {
        cmds.push(("hiddenTabs.close", Value::Null));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- Manage Accounts

fn accounts_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(view) = s.misc.accounts.clone() else { return };
    let theme = st.theme.clone();
    let mut open = true;
    egui::Window::new(view.title.clone())
        .id(Id::new("misc-accounts"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(520.0, 260.0))
        .anchor(Align2::CENTER_TOP, vec2(0.0, 90.0))
        .show(ctx, |ui| {
            if view.loading {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label("Checking accounts…");
                });
            } else {
                ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                    ui.label(RichText::new(&view.text).monospace());
                });
            }
            ui.separator();
            ui.label(RichText::new(&view.hint).color(theme.text_muted).small());
        });
    if !open || escape_pressed(ctx) {
        run_command(s, st, "accounts.close", &Value::Null);
    }
}
