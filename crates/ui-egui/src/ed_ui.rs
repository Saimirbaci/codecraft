//! M12 track ED windows: Search/Replace Structurally (dialog and replacement preview), Macros
//! (name prompt, Play Saved Macros, Edit Macros), Run Inspection by Name, Configure Current File
//! Analysis, Locate Duplicates (settings and results), Data Flow trees, Window › Layouts, File
//! Templates, Update Copyright, Tasks & Contexts, the status bar widgets (macro recording, active
//! task and timer, highlighting level) and the Presentation Assistant overlay.
//!
//! The UI edits dialog drafts directly; every action goes through an engine command.

use egui::{Align2, Id, Key, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::analysis;
use codecraft_engine::data_flow::{Direction, FlowKind, FlowNode};
use codecraft_engine::file_templates::TemplateDialog;
use codecraft_engine::layouts::LayoutAction;
use codecraft_engine::ssr::{self, Constraint};
use codecraft_engine::tasks_contexts::{self, ChooserKind, TaskDialogKind};
use codecraft_workspace::AnalysisLevel;

use crate::{UiState, keymap, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// How long the Presentation Assistant shows an action.
const PRESENTATION_SECS: f32 = 3.0;

/// Whether a modal dialog of this module is open (it reads the keyboard itself).
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.ssr.dialog.is_some()
        || s.macros.name_prompt.is_some()
        || s.macros.chooser
        || s.macros.edit.is_some()
        || s.analysis.by_name.is_some()
        || s.analysis.level_popup.is_some()
        || s.duplicates.dialog.is_some()
        || s.layouts.dialog.is_some()
        || s.file_templates.dialog.is_some()
        || s.copyright.dialog.is_some()
        || s.tasks.dialog.is_some()
        || s.tasks.chooser.is_some()
}

fn escape(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_pressed(Key::Escape))
}

fn enter(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_pressed(Key::Enter) && (i.modifiers.command || i.modifiers.ctrl))
}

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn dialog<'a>(title: impl Into<egui::WidgetText>, id: &str) -> egui::Window<'a> {
    egui::Window::new(title)
        .id(Id::new(id))
        .collapsible(false)
        .resizable(true)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 80.0))
}

fn error_line(ui: &mut egui::Ui, st: &UiState, error: Option<&String>) {
    if let Some(e) = error {
        ui.label(RichText::new(e).color(st.theme.error));
    }
}

/// Every window of this module.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    ssr_dialog(&ctx, s, st);
    ssr_preview(&ctx, s, st);
    macro_windows(&ctx, s, st);
    by_name(&ctx, s, st);
    level_popup(&ctx, s, st);
    duplicates_windows(&ctx, s, st);
    data_flow_window(&ctx, s, st);
    layouts_dialog(&ctx, s, st);
    templates_dialog(&ctx, s, st);
    copyright_dialog(&ctx, s, st);
    tasks_windows(&ctx, s, st);
    presentation_overlay(&ctx, s, st);
}

// ---------------------------------------------------------------- structural search

fn ssr_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if s.ssr.dialog.is_none() {
        return;
    }
    let templates = s.ssr.all_templates();
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let replace_mode = s.ssr.dialog.as_ref().is_some_and(|d| d.replace_mode);
    let title = if replace_mode { "Replace Structurally" } else { "Search Structurally" };
    let mut open = true;
    dialog(title, "ssr-dialog").open(&mut open).default_size(vec2(640.0, 460.0)).show(ctx, |ui| {
        let Some(d) = s.ssr.dialog.as_mut() else { return };
        ui.horizontal(|ui| {
            ui.menu_button("Templates ▾", |ui| {
                ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for (t, builtin) in &templates {
                        let label = format!("{}  ·  {}{}", t.name, t.language, if *builtin { "" } else { "  (saved)" });
                        if ui.button(label).on_hover_text(&t.search).clicked() {
                            cmds.push(("ssr.dialog.load", json!({ "name": t.name })));
                            ui.close();
                        }
                    }
                });
            });
            ui.label("Language:");
            egui::ComboBox::from_id_salt("ssr-language").selected_text(d.language.clone()).show_ui(ui, |ui| {
                for (id, l) in ssr::LANGUAGES {
                    ui.selectable_value(&mut d.language, (*id).to_string(), l.name());
                }
            });
            ui.label("Scope:");
            egui::ComboBox::from_id_salt("ssr-scope").selected_text(d.scope.clone()).show_ui(ui, |ui| {
                for (v, label) in [("project", "Project"), ("file", "Current File"), ("directory", "Directory"), ("openFiles", "Open Files"), ("uncommitted", "Uncommitted Files")] {
                    ui.selectable_value(&mut d.scope, v.to_string(), label);
                }
            });
        });
        ui.label(RichText::new("Search template (use $name$ for variables):").color(theme.text_muted));
        let search = ui.add(TextEdit::multiline(&mut d.search).code_editor().desired_rows(4).desired_width(f32::INFINITY));
        if search.ctx.memory(|m| m.focused().is_none()) {
            search.request_focus();
        }
        if d.replace_mode {
            ui.label(RichText::new("Replace template:").color(theme.text_muted));
            ui.add(TextEdit::multiline(&mut d.replace).code_editor().desired_rows(3).desired_width(f32::INFINITY));
        }
        let vars = ssr::variables(&d.search);
        if !vars.is_empty() {
            ui.separator();
            ui.label(RichText::new("Variables").strong());
            egui::Grid::new("ssr-vars").num_columns(6).spacing(vec2(8.0, 4.0)).show(ui, |ui| {
                ui.label(RichText::new("Name").small().color(theme.text_muted));
                ui.label(RichText::new("Text (regex)").small().color(theme.text_muted));
                ui.label(RichText::new("Not").small().color(theme.text_muted));
                ui.label(RichText::new("Min").small().color(theme.text_muted));
                ui.label(RichText::new("Max").small().color(theme.text_muted));
                ui.label(RichText::new("∞").small().color(theme.text_muted));
                ui.end_row();
                for v in &vars {
                    if !d.constraints.iter().any(|c| &c.name == v) {
                        d.constraints.push(Constraint { name: v.clone(), ..Constraint::default() });
                    }
                    let Some(c) = d.constraints.iter_mut().find(|c| &c.name == v) else { continue };
                    ui.label(RichText::new(format!("${v}$")).monospace());
                    ui.add(TextEdit::singleline(&mut c.regex).desired_width(160.0).hint_text("any"));
                    ui.checkbox(&mut c.invert, "");
                    let mut min = c.min.unwrap_or(1);
                    if ui.add(egui::DragValue::new(&mut min).range(0..=1000)).changed() {
                        c.min = Some(min);
                    }
                    let mut max = c.max.unwrap_or(1).max(min);
                    if ui.add_enabled(!c.unbounded, egui::DragValue::new(&mut max).range(0..=1000)).changed() {
                        c.max = Some(max);
                    }
                    ui.checkbox(&mut c.unbounded, "");
                    ui.end_row();
                }
            });
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(TextEdit::singleline(&mut d.template_name).hint_text("template name").desired_width(200.0));
            if ui.button("Save Template").clicked() {
                cmds.push(("ssr.templates.save", json!({})));
            }
        });
        error_line(ui, st, d.error.as_ref());
        ui.horizontal(|ui| {
            if ui.button(if d.replace_mode { "Find and Preview" } else { "Find" }).clicked() {
                cmds.push(("ssr.dialog.run", Value::Null));
            }
            if ui.button("Cancel").clicked() {
                cmds.push(("ssr.dialog.close", Value::Null));
            }
            ui.label(RichText::new("⌘⏎ runs").small().color(theme.text_muted));
        });
    });
    if enter(ctx) {
        cmds.push(("ssr.dialog.run", Value::Null));
    }
    if !open || escape(ctx) {
        cmds.push(("ssr.dialog.close", Value::Null));
    }
    run_all(s, st, cmds);
}

fn ssr_preview(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if !s.ssr.preview {
        return;
    }
    let Some(r) = s.ssr.results.clone().filter(|r| r.replace.is_some()) else { return };
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let included = r.files.iter().flat_map(|f| &f.hits).filter(|h| !r.excluded.contains(&h.id)).count();
    dialog("Replace Structurally Preview", "ssr-preview").open(&mut open).default_size(vec2(720.0, 420.0)).show(ctx, |ui| {
        ui.horizontal(|ui| {
            if !r.done {
                ui.add(egui::Spinner::new().size(12.0));
            }
            ui.label(format!("{} occurrence(s) in {} file(s) · {}", r.hit_count(), r.files.len(), r.scope_label));
        });
        ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
            for f in &r.files {
                let name = s.ws.project.as_ref().map(|p| p.relative(&f.path).into_owned()).unwrap_or_else(|| f.path.display().to_string());
                ui.label(RichText::new(name).strong());
                for h in &f.hits {
                    ui.horizontal(|ui| {
                        let mut on = !r.excluded.contains(&h.id);
                        if ui.checkbox(&mut on, "").changed() {
                            cmds.push(("ssr.preview.exclude", json!({ "id": h.id, "exclude": !on })));
                        }
                        let old: String = h.matched.split_whitespace().collect::<Vec<_>>().join(" ");
                        let new: String = h.replacement.as_deref().unwrap_or("").split_whitespace().collect::<Vec<_>>().join(" ");
                        let text = format!("{:>5}  {}  →  {}", h.line + 1, old, new);
                        if ui.add(egui::Label::new(RichText::new(text).monospace().color(theme.text_editor)).sense(egui::Sense::click())).clicked() {
                            cmds.push(("ssr.open", json!({ "id": h.id })));
                        }
                    });
                }
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.add_enabled(r.done && included > 0, egui::Button::new(format!("Replace All ({included})"))).clicked() {
                cmds.push(("ssr.replace.apply", Value::Null));
            }
            if ui.button("Close").clicked() {
                cmds.push(("ssr.preview.close", Value::Null));
            }
        });
    });
    if !open {
        cmds.push(("ssr.preview.close", Value::Null));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- macros

fn macro_windows(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    if let Some(name) = s.macros.name_prompt.as_mut() {
        let mut open = true;
        let steps = s.macros.last.as_ref().map(|m| m.steps.len()).unwrap_or(0);
        dialog("Enter Macro Name", "macro-name").open(&mut open).show(ctx, |ui| {
            ui.label(format!("{steps} step(s) recorded. Name the macro to save it, or leave it unnamed to keep it as the last macro."));
            let r = ui.add(TextEdit::singleline(name).hint_text("macro name").desired_width(280.0));
            r.request_focus();
            let submit = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() || submit {
                    if name.trim().is_empty() {
                        cmds.push(("edit.macros.dismiss", Value::Null));
                    } else {
                        cmds.push(("edit.macros.save", json!({ "name": name.clone() })));
                    }
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("edit.macros.dismiss", Value::Null));
                }
            });
        });
        if !open || escape(ctx) {
            cmds.push(("edit.macros.dismiss", Value::Null));
        }
    }
    if s.macros.chooser {
        let list: Vec<(String, String, Option<String>)> =
            s.macros.list().iter().map(|m| (m.name.clone(), m.summary(), m.shortcut.clone())).collect();
        let mut open = true;
        dialog("Play Saved Macro", "macro-chooser").open(&mut open).default_size(vec2(420.0, 300.0)).show(ctx, |ui| {
            ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for (name, summary, shortcut) in &list {
                    let label = match shortcut {
                        Some(sc) => format!("{name}   {}", keymap::display(sc)),
                        None => name.clone(),
                    };
                    if ui.button(label).on_hover_text(summary).clicked() {
                        cmds.push(("edit.macros.playSavedMacros", json!({ "name": name })));
                    }
                }
            });
        });
        if !open || escape(ctx) {
            cmds.push(("edit.macros.dismiss", Value::Null));
        }
    }
    if s.macros.edit.is_some() {
        let list: Vec<(String, String, Option<String>)> =
            s.macros.list().iter().map(|m| (m.name.clone(), m.summary(), m.shortcut.clone())).collect();
        let mut open = true;
        dialog("Edit Macros", "macro-edit").open(&mut open).default_size(vec2(560.0, 320.0)).show(ctx, |ui| {
            let Some(d) = s.macros.edit.as_mut() else { return };
            if list.is_empty() {
                ui.label(RichText::new("No saved macros. Record one with Edit › Macros › Start Macro Recording.").color(theme.text_muted));
                return;
            }
            ui.columns(2, |cols| {
                ScrollArea::vertical().id_salt("macro-list").max_height(260.0).show(&mut cols[0], |ui| {
                    for (name, _, shortcut) in &list {
                        let selected = d.selected.as_deref() == Some(name.as_str());
                        if ui.selectable_label(selected, name).clicked() {
                            d.selected = Some(name.clone());
                            d.rename = name.clone();
                            d.shortcut = shortcut.clone().unwrap_or_default();
                        }
                    }
                });
                let ui = &mut cols[1];
                let Some(sel) = d.selected.clone() else {
                    ui.label(RichText::new("Select a macro").color(theme.text_muted));
                    return;
                };
                if let Some((_, summary, _)) = list.iter().find(|(n, ..)| *n == sel) {
                    ui.label(RichText::new(summary).small().color(theme.text_muted));
                }
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut d.rename).desired_width(160.0));
                    if ui.button("Rename").clicked() {
                        cmds.push(("edit.macros.editMacros", json!({ "action": "rename", "name": sel, "newName": d.rename })));
                    }
                });
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut d.shortcut).hint_text("Ctrl+Alt+M").desired_width(160.0));
                    if ui.button("Set Shortcut").clicked() {
                        cmds.push(("edit.macros.editMacros", json!({ "action": "setShortcut", "name": sel, "shortcut": d.shortcut })));
                    }
                });
                if ui.button("Delete").clicked() {
                    cmds.push(("edit.macros.editMacros", json!({ "action": "delete", "name": sel })));
                }
                error_line(ui, st, d.error.as_ref());
            });
        });
        if !open || escape(ctx) {
            cmds.push(("edit.macros.dismiss", Value::Null));
        }
    }
    run_all(s, st, cmds);
}

/// A key press bound to a saved macro plays it (called before the keymap).
pub(crate) fn macro_shortcut(ctx: &egui::Context, s: &mut Session, st: &mut UiState) -> bool {
    if s.macros.list().iter().all(|m| m.shortcut.is_none()) {
        return false;
    }
    let events = ctx.input(|i| i.events.clone());
    for ev in events {
        let egui::Event::Key { key, pressed: true, modifiers, .. } = ev else { continue };
        let Some(sc) = keymap::from_event(key, &modifiers) else { continue };
        if let Some(name) = s.macros.for_shortcut(&sc) {
            ctx.input_mut(|i| i.consume_key(modifiers, key));
            run_command(s, st, "edit.macros.play", &json!({ "name": name }));
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------- Analyze Code

fn by_name(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if s.analysis.by_name.is_none() {
        return;
    }
    let all = analysis::catalogue(s);
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    dialog("Run Inspection by Name", "inspection-by-name").open(&mut open).default_size(vec2(520.0, 380.0)).show(ctx, |ui| {
        let Some(d) = s.analysis.by_name.as_mut() else { return };
        let r = ui.add(TextEdit::singleline(&mut d.query).hint_text("Enter inspection name").desired_width(f32::INFINITY));
        r.request_focus();
        let list = analysis::filter(&all, &d.query);
        let (up, down, go) = ui.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter)));
        if down {
            d.selected = (d.selected + 1).min(list.len().saturating_sub(1));
        }
        if up {
            d.selected = d.selected.saturating_sub(1);
        }
        ui.horizontal(|ui| {
            ui.label("Scope:");
            ui.radio_value(&mut d.scope, "file".to_string(), "Current File");
            ui.radio_value(&mut d.scope, "project".to_string(), "Whole Project");
        });
        ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
            for (i, item) in list.iter().enumerate().take(500) {
                let text = format!("{}   ·   {}", item.name, item.tool.label());
                let resp = ui.selectable_label(i == d.selected, text).on_hover_text(&item.description);
                if resp.clicked() {
                    cmds.push(("analysis.byName.run", json!({ "name": item.name })));
                }
                if i == d.selected && (up || down) {
                    resp.scroll_to_me(None);
                }
            }
        });
        if go && let Some(item) = list.get(d.selected) {
            cmds.push(("analysis.byName.run", json!({ "name": item.name })));
        }
        if list.is_empty() {
            ui.label(RichText::new("No inspection matches; Enter runs the name as a lint").color(theme.text_muted));
            if go && !d.query.trim().is_empty() {
                cmds.push(("analysis.byName.run", json!({ "name": d.query.trim() })));
            }
        }
    });
    if !open || escape(ctx) {
        cmds.push(("analysis.dismiss", Value::Null));
    }
    run_all(s, st, cmds);
}

fn level_popup(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(path) = s.analysis.level_popup.clone() else { return };
    let current = s.ws.analysis_level(&path);
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dialog(format!("Highlighting Level: {name}"), "analysis-level").open(&mut open).show(ctx, |ui| {
        for level in AnalysisLevel::ALL {
            let hint = match level {
                AnalysisLevel::None => "No problems are highlighted",
                AnalysisLevel::Syntax => "Only syntax errors",
                AnalysisLevel::Essential => "Errors and warnings",
                AnalysisLevel::All => "Every problem (default)",
            };
            if ui.radio(current == level, level.label()).on_hover_text(hint).clicked() {
                cmds.push(("code.analyzeCode.configureCurrentFileAnalysis", json!({ "level": level.label().split(' ').next().unwrap_or("all"), "path": path })));
            }
        }
    });
    if !open || escape(ctx) {
        cmds.push(("analysis.dismiss", Value::Null));
    }
    run_all(s, st, cmds);
}

fn duplicates_windows(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    if s.duplicates.dialog.is_some() {
        let mut open = true;
        dialog("Locate Duplicates", "duplicates-dialog").open(&mut open).show(ctx, |ui| {
            let Some(d) = s.duplicates.dialog.as_mut() else { return };
            ui.horizontal(|ui| {
                ui.label("Scope:");
                egui::ComboBox::from_id_salt("dup-scope").selected_text(d.scope.clone()).show_ui(ui, |ui| {
                    for v in ["project", "directory", "file", "uncommitted"] {
                        ui.selectable_value(&mut d.scope, v.to_string(), v);
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("Minimal size (tokens):");
                ui.add(egui::DragValue::new(&mut d.min_tokens).range(5..=10_000));
            });
            ui.checkbox(&mut d.anonymize_identifiers, "Anonymize identifiers (find renamed copies)");
            ui.checkbox(&mut d.anonymize_literals, "Anonymize literals");
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    cmds.push(("duplicates.dialog.ok", Value::Null));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("duplicates.close", Value::Null));
                }
            });
        });
        if !open || escape(ctx) {
            cmds.push(("duplicates.close", Value::Null));
        }
    }
    if let Some(r) = s.duplicates.result.clone() {
        let mut open = true;
        dialog("Duplicates", "duplicates-results").open(&mut open).default_size(vec2(720.0, 460.0)).show(ctx, |ui| {
            ui.horizontal(|ui| {
                if !r.done {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(format!("Analyzing {} file(s)…", r.files));
                } else {
                    ui.label(format!("{} duplicated fragment(s) in {} file(s) · {}", r.groups.len(), r.files, r.scope_label));
                }
            });
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (gi, g) in r.groups.iter().enumerate() {
                    let title = format!("{} occurrences · {} lines · {} tokens", g.occurrences.len(), g.lines, g.tokens);
                    egui::CollapsingHeader::new(title).id_salt(("dup", gi)).default_open(gi < 3).show(ui, |ui| {
                        for (oi, o) in g.occurrences.iter().enumerate() {
                            let name = s.ws.project.as_ref().map(|p| p.relative(&o.path).into_owned()).unwrap_or_else(|| o.path.display().to_string());
                            let text = format!("{name}:{}–{}", o.start_line + 1, o.end_line + 1);
                            if ui.link(text).clicked() {
                                cmds.push(("duplicates.open", json!({ "group": gi, "occurrence": oi })));
                            }
                        }
                        ui.label(RichText::new(&g.preview).monospace().small().color(theme.text_muted));
                    });
                }
            });
        });
        if !open {
            cmds.push(("duplicates.close", Value::Null));
        }
    }
    run_all(s, st, cmds);
}

fn flow_icon(k: FlowKind) -> &'static str {
    match k {
        FlowKind::Variable => "◆",
        FlowKind::Declaration => "=",
        FlowKind::Parameter => "ƒ",
        FlowKind::Assignment => "←",
        FlowKind::CallArgument => "→",
        FlowKind::Return => "↩",
        FlowKind::Field => "·",
        FlowKind::Use => "•",
        FlowKind::Recursive => "↻",
    }
}

fn flow_node(ui: &mut egui::Ui, n: &FlowNode, path: &mut Vec<usize>, cmds: &mut Cmds) {
    let text = format!("{} {}   {}:{}", flow_icon(n.kind), n.label, n.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(), n.line + 1);
    if n.children.is_empty() {
        if ui.link(text).clicked() {
            cmds.push(("dataFlow.open", json!({ "node": path.clone() })));
        }
        return;
    }
    let resp = egui::CollapsingHeader::new(text).id_salt(("flow", path.clone())).default_open(path.len() < 3).show(ui, |ui| {
        for (i, c) in n.children.iter().enumerate() {
            path.push(i);
            flow_node(ui, c, path, cmds);
            path.pop();
        }
    });
    if resp.header_response.double_clicked() {
        cmds.push(("dataFlow.open", json!({ "node": path.clone() })));
    }
}

fn data_flow_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(r) = s.data_flow.result.clone() else { return };
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let title = match r.direction {
        Direction::ToHere => "Data Flow to Here",
        Direction::FromHere => "Data Flow from Here",
    };
    dialog(title, "data-flow").open(&mut open).default_size(vec2(640.0, 420.0)).show(ctx, |ui| {
        if !r.done {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label("Analyzing…");
            });
        }
        error_line(ui, st, r.error.as_ref());
        if let Some(root) = &r.root {
            ui.label(RichText::new("Approximated from the code's text (names and calls); double-click a node to open it.").small().color(theme.text_muted));
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut path = Vec::new();
                flow_node(ui, root, &mut path, &mut cmds);
                if root.children.is_empty() {
                    ui.label(RichText::new("Nothing found").color(theme.text_muted));
                }
            });
        }
    });
    if !open {
        cmds.push(("dataFlow.close", Value::Null));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- layouts, templates, copyright

fn layouts_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(action) = s.layouts.dialog.as_ref().map(|d| d.action) else { return };
    let names = s.layouts.names();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    dialog(action.title(), "layouts-dialog").open(&mut open).show(ctx, |ui| {
        let Some(d) = s.layouts.dialog.as_mut() else { return };
        if action != LayoutAction::SaveAsNew {
            for n in &names {
                if ui.radio(d.selected.as_deref() == Some(n.as_str()), n).clicked() {
                    d.selected = Some(n.clone());
                    if action == LayoutAction::Rename {
                        d.name = n.clone();
                    }
                }
            }
        }
        if matches!(action, LayoutAction::SaveAsNew | LayoutAction::Rename) {
            let r = ui.add(TextEdit::singleline(&mut d.name).hint_text("layout name").desired_width(260.0));
            r.request_focus();
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                cmds.push(("window.layouts.dialog.ok", Value::Null));
            }
        }
        error_line(ui, st, d.error.as_ref());
        ui.horizontal(|ui| {
            let label = if action == LayoutAction::Delete { "Delete" } else { "OK" };
            if ui.button(label).clicked() {
                cmds.push(("window.layouts.dialog.ok", Value::Null));
            }
            if ui.button("Cancel").clicked() {
                cmds.push(("window.layouts.dialog.cancel", Value::Null));
            }
        });
    });
    if !open || escape(ctx) {
        cmds.push(("window.layouts.dialog.cancel", Value::Null));
    }
    run_all(s, st, cmds);
}

fn templates_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if s.file_templates.dialog.is_none() {
        return;
    }
    let names: Vec<(String, String)> = s.file_templates.list().iter().map(|t| (t.name.clone(), t.extension.clone())).collect();
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let title = match &s.file_templates.dialog {
        Some(TemplateDialog::Save { .. }) => "Save File as Template",
        _ => "New File from Template",
    };
    dialog(title, "file-template-dialog").open(&mut open).show(ctx, |ui| {
        match s.file_templates.dialog.as_mut() {
            Some(TemplateDialog::Save { name, extension, error }) => {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.add(TextEdit::singleline(name).desired_width(220.0)).request_focus();
                });
                ui.horizontal(|ui| {
                    ui.label("Extension:");
                    ui.add(TextEdit::singleline(extension).desired_width(80.0));
                });
                ui.label(RichText::new("New files from it expand ${NAME}, ${DATE}, ${YEAR}, ${USER}, ${PROJECT_NAME}.").small().color(st.theme.text_muted));
                error_line(ui, st, error.as_ref());
            }
            Some(TemplateDialog::New { template, name, directory, error }) => {
                egui::ComboBox::from_id_salt("tpl-choice").selected_text(template.clone().unwrap_or_default()).show_ui(ui, |ui| {
                    for (n, ext) in &names {
                        ui.selectable_value(template, Some(n.clone()), format!("{n}  (.{ext})"));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("File name:");
                    ui.add(TextEdit::singleline(name).desired_width(220.0)).request_focus();
                });
                ui.label(RichText::new(format!("in {}", directory.display())).small().color(st.theme.text_muted));
                error_line(ui, st, error.as_ref());
            }
            None => {}
        }
        ui.horizontal(|ui| {
            if ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                cmds.push(("file.templates.dialog.ok", Value::Null));
            }
            if ui.button("Cancel").clicked() {
                cmds.push(("file.templates.dialog.cancel", Value::Null));
            }
        });
    });
    if !open || escape(ctx) {
        cmds.push(("file.templates.dialog.cancel", Value::Null));
    }
    run_all(s, st, cmds);
}

fn copyright_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if s.copyright.dialog.is_none() {
        return;
    }
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    dialog("Update Copyright", "copyright-dialog").open(&mut open).default_size(vec2(520.0, 300.0)).show(ctx, |ui| {
        let Some(d) = s.copyright.dialog.as_mut() else { return };
        ui.label(RichText::new("Copyright notice ($year, $file.name, $project.name):").color(st.theme.text_muted));
        ui.add(TextEdit::multiline(&mut d.notice).desired_rows(4).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            ui.label("Scope:");
            for (v, label) in [("file", "Current File"), ("openFiles", "Open Files"), ("project", "Project"), ("uncommitted", "Uncommitted")] {
                ui.radio_value(&mut d.scope, v.to_string(), label);
            }
        });
        error_line(ui, st, d.error.as_ref());
        ui.horizontal(|ui| {
            if ui.button("Update").clicked() {
                cmds.push(("copyright.dialog.ok", Value::Null));
            }
            if ui.button("Cancel").clicked() {
                cmds.push(("copyright.dialog.cancel", Value::Null));
            }
        });
    });
    if !open || escape(ctx) {
        cmds.push(("copyright.dialog.cancel", Value::Null));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- tasks & contexts

fn tasks_windows(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let mut cmds: Cmds = Vec::new();
    if let Some(kind) = s.tasks.dialog.as_ref().map(|d| d.kind) {
        let title = match kind {
            TaskDialogKind::Open => "Open Task",
            TaskDialogKind::Edit => "Edit Task",
            TaskDialogKind::SaveContext => "Save Context",
        };
        let has_repo = s.vcs.repo.is_some();
        let mut open = true;
        dialog(title, "task-dialog").open(&mut open).default_size(vec2(480.0, 280.0)).show(ctx, |ui| {
            let Some(d) = s.tasks.dialog.as_mut() else { return };
            let hint = if kind == TaskDialogKind::SaveContext { "context name" } else { "summary" };
            ui.add(TextEdit::singleline(&mut d.summary).hint_text(hint).desired_width(f32::INFINITY)).request_focus();
            if kind != TaskDialogKind::SaveContext {
                ui.add(TextEdit::multiline(&mut d.description).hint_text("description").desired_rows(4).desired_width(f32::INFINITY));
                ui.add(TextEdit::singleline(&mut d.url).hint_text("URL (optional)").desired_width(f32::INFINITY));
            }
            if kind == TaskDialogKind::Open {
                ui.add_enabled(has_repo, egui::Checkbox::new(&mut d.create_branch, "Create a Git branch for the task"));
            }
            error_line(ui, st, d.error.as_ref());
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    cmds.push(("tasks.dialog.ok", Value::Null));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("tasks.dismiss", Value::Null));
                }
            });
        });
        if enter(ctx) {
            cmds.push(("tasks.dialog.ok", Value::Null));
        }
        if !open || escape(ctx) {
            cmds.push(("tasks.dismiss", Value::Null));
        }
    }
    if let Some(kind) = s.tasks.chooser.as_ref().map(|c| c.kind) {
        let items = tasks_contexts::chooser_items(s);
        let mut open = true;
        let title = if kind == ChooserKind::SwitchTask { "Switch Task" } else { "Load Context" };
        dialog(title, "task-chooser").open(&mut open).default_size(vec2(420.0, 320.0)).show(ctx, |ui| {
            if let Some(c) = s.tasks.chooser.as_mut() {
                ui.add(TextEdit::singleline(&mut c.query).hint_text("filter").desired_width(f32::INFINITY)).request_focus();
            }
            ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for (i, (_, label)) in items.iter().enumerate() {
                    if ui.selectable_label(false, label).clicked() {
                        cmds.push(("tasks.chooser.accept", json!({ "index": i })));
                    }
                }
            });
            if ui.input(|i| i.key_pressed(Key::Enter)) && !items.is_empty() {
                cmds.push(("tasks.chooser.accept", json!({ "index": 0 })));
            }
            if kind == ChooserKind::SwitchTask && ui.button("Create New Task…").clicked() {
                cmds.push(("tasks.dismiss", Value::Null));
                cmds.push(("tools.tasksContexts.openTask", json!({})));
            }
        });
        if !open || escape(ctx) {
            cmds.push(("tasks.dismiss", Value::Null));
        }
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- status bar and overlay

/// Status bar: macro recording, the active task (and its timer), the file's highlighting level.
pub fn status_widget(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let small = |t: String, c: egui::Color32| RichText::new(t).size(12.0).color(c);
    let mut cmds: Cmds = Vec::new();
    if let Some(steps) = s.macros.recording.as_ref().map(Vec::len) {
        let r = ui.add(egui::Label::new(small(format!("● Recording macro ({steps})"), st.theme.error)).sense(egui::Sense::click()));
        if r.on_hover_text("Click to stop recording").clicked() {
            cmds.push(("edit.macros.startMacroRecording", json!({})));
        }
    }
    if let Some(path) = s.ws.active().and_then(|(_, d)| d.path.clone()) {
        let level = s.ws.analysis_level(&path);
        if level != AnalysisLevel::All {
            let r = ui.add(egui::Label::new(small(format!("Highlighting: {}", level.label()), st.theme.warning)).sense(egui::Sense::click()));
            if r.on_hover_text("Configure Current File Analysis").clicked() {
                cmds.push(("code.analyzeCode.configureCurrentFileAnalysis", json!({})));
            }
        }
    }
    if let Some(task) = s.tasks.active().filter(|t| t.id != tasks_contexts::DEFAULT_TASK || s.tasks.timer_started_ms.is_some()) {
        let timer = s.tasks.timer_started_ms.map(|_| format!(" ⏱ {}", tasks_contexts::format_duration(s.tasks.active_time_ms()))).unwrap_or_default();
        let summary: String = task.summary.chars().take(40).collect();
        let r = ui.add(egui::Label::new(small(format!("{summary}{timer}"), st.theme.text_muted)).sense(egui::Sense::click()));
        if r.on_hover_text("Active task: click to switch").clicked() {
            cmds.push(("tools.tasksContexts.switchTask", json!({})));
        }
        if !timer.is_empty() {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
    run_all(s, st, cmds);
}

fn presentation_overlay(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if !s.presentation.enabled {
        return;
    }
    let Some(a) = s.presentation.latest() else { return };
    let age = a.at.elapsed().as_secs_f32();
    if age > PRESENTATION_SECS {
        return;
    }
    ctx.request_repaint_after(std::time::Duration::from_millis(200));
    let shortcut = keymap::active_shortcuts(&a.id).first().map(|sc| keymap::display(sc));
    let text = match &shortcut {
        Some(sc) => format!("{}   {sc}", a.label),
        None => a.label.clone(),
    };
    let theme = st.theme.clone();
    egui::Area::new(Id::new("presentation-assistant"))
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -60.0))
        .interactable(false)
        .order(egui::Order::Tooltip)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme.popup_bg)
                .stroke(egui::Stroke::new(1.0, theme.border))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::symmetric(18, 10))
                .show(ui, |ui| {
                    ui.label(RichText::new(text).size(22.0).color(theme.text));
                    if let Some(menu) = &a.menu {
                        ui.label(RichText::new(menu).size(12.0).color(theme.text_muted));
                    }
                });
        });
}
