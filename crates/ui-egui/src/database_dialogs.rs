//! Database dialogs that float above the window: query parameters, the production guard's
//! confirmation, the console's query history (with saved queries) and Import Data from File.
//! OWNED BY the database track. Every action is a `db.*` command; text fields edit the engine's
//! dialog state in place (like the data source dialog).

use egui::{Align, Align2, Layout, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::{ImportState, ParamKind};

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// Draw whichever database dialogs are open.
pub fn show(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let mut cmds: Cmds = Vec::new();
    params(ctx, s, st, &mut cmds);
    confirm(ctx, s, st, &mut cmds);
    history(ctx, s, st, &mut cmds);
    import(ctx, s, st, &mut cmds);
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn first_lines(sql: &str, n: usize) -> String {
    let mut lines: Vec<&str> = sql.lines().take(n).collect();
    if sql.lines().count() > n {
        lines.push("…");
    }
    lines.join("\n")
}

fn params(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(prompt) = s.db.param_prompt.as_mut() else { return };
    let mut open = true;
    let theme = &st.theme;
    egui::Window::new("Query Parameters")
        .id(egui::Id::new("db-params"))
        .collapsible(false)
        .resizable(true)
        .default_width(460.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .open(&mut open)
        .show(ctx, |ui| {
            for stmt in prompt.run.statements.iter().take(3) {
                ui.label(RichText::new(first_lines(stmt, 4)).monospace().size(11.5).color(theme.text_muted));
            }
            ui.label(
                RichText::new(
                    "Values are bound, never inserted into the SQL. Value: NULL, 42, true or text ('quoted' or not); \
                     Text: exactly as typed; SQL expression: inserted as typed.",
                )
                .color(theme.text_muted),
            );
            ui.add_space(4.0);
            let mut submit = false;
            let n = prompt.params.len();
            egui::Grid::new("db-params-grid").num_columns(3).spacing([10.0, 6.0]).show(ui, |ui| {
                for (i, f) in prompt.params.iter_mut().enumerate() {
                    ui.label(RichText::new(&f.name).monospace().color(theme.text));
                    egui::ComboBox::from_id_salt(("db-param-kind", i))
                        .selected_text(f.kind.label())
                        .width(120.0)
                        .show_ui(ui, |ui| {
                            for k in [ParamKind::Auto, ParamKind::Text, ParamKind::Sql] {
                                ui.selectable_value(&mut f.kind, k, k.label());
                            }
                        });
                    let hint = match f.kind {
                        ParamKind::Auto => "NULL, 42, Ann",
                        ParamKind::Text => "text, exactly",
                        ParamKind::Sql => "now(), (select …)",
                    };
                    let resp = ui.add(
                        TextEdit::singleline(&mut f.value)
                            .id(egui::Id::new(("db-param", i)))
                            .hint_text(hint)
                            .desired_width(260.0),
                    );
                    if i == 0 && !ui.memory(|m| m.focused().is_some()) {
                        resp.request_focus();
                    }
                    if i + 1 == n && resp.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                        submit = true;
                    }
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cmds.push(("db.cancelParameters", Value::Null));
                }
                if ui.add(egui::Button::new("Execute").fill(theme.accent)).clicked() || submit {
                    cmds.push(("db.submitParameters", Value::Null));
                }
            });
        });
    if !open {
        cmds.push(("db.cancelParameters", Value::Null));
    }
}

fn confirm(ctx: &egui::Context, s: &Session, st: &UiState, cmds: &mut Cmds) {
    let Some(c) = &s.db.confirm else { return };
    let mut open = true;
    let theme = &st.theme;
    egui::Window::new("Confirm Data Modification")
        .id(egui::Id::new("db-confirm"))
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(format!("{} is a production data source. Run these statements?", c.source_name))
                    .color(theme.warning),
            );
            ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                for w in &c.writes {
                    ui.label(RichText::new(first_lines(w, 8)).monospace().color(theme.text_editor));
                }
            });
            ui.add_space(6.0);
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cmds.push(("db.cancelExecution", Value::Null));
                }
                if ui.add(egui::Button::new("Execute").fill(theme.error)).clicked() {
                    cmds.push(("db.confirmExecution", Value::Null));
                }
            });
        });
    if !open {
        cmds.push(("db.cancelExecution", Value::Null));
    }
}

/// `5 min ago` for a Unix time.
fn ago(at: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let d = now.saturating_sub(at);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

fn history(ctx: &egui::Context, s: &mut Session, st: &mut UiState, cmds: &mut Cmds) {
    let Some(popup) = s.db.history_popup.clone() else { return };
    let source = popup.source.clone();
    let entries = s.db.history.entries(&source, &popup.filter);
    let saved = s.db.history.saved(&source);
    let name = s.db.source(&source).map(|d| d.config.name.clone()).unwrap_or_else(|| source.clone());
    let mut open = true;
    let theme = st.theme.clone();
    let mut filter = popup.filter.clone();
    egui::Window::new(format!("Query History — {name}"))
        .id(egui::Id::new("db-history"))
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(620.0, 440.0))
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("🔍");
                let r = ui.add(TextEdit::singleline(&mut filter).hint_text("Filter").desired_width(260.0));
                if !r.has_focus() && filter.is_empty() && ui.memory(|m| m.focused().is_none()) {
                    r.request_focus();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add_enabled(!entries.is_empty(), egui::Button::new("Clear History")).clicked() {
                        cmds.push(("db.clearHistory", json!({ "dataSource": source })));
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Save the query at the caret as").color(theme.text_muted));
                ui.add(TextEdit::singleline(&mut st.db.save_name).hint_text("name").desired_width(160.0));
                let can = !st.db.save_name.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new("Save")).clicked() {
                    cmds.push(("db.saveQuery", json!({ "dataSource": source, "name": st.db.save_name.trim() })));
                    st.db.save_name.clear();
                }
            });
            ui.separator();
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if !saved.is_empty() {
                    ui.label(RichText::new("Saved queries").strong().color(theme.text));
                    for q in &saved {
                        ui.horizontal(|ui| {
                            if ui.small_button("▶").on_hover_text("Insert and run").clicked() {
                                cmds.push((
                                    "db.useHistory",
                                    json!({ "dataSource": source, "name": q.name, "run": true }),
                                ));
                            }
                            if ui.small_button("⤵").on_hover_text("Insert").clicked() {
                                cmds.push(("db.useHistory", json!({ "dataSource": source, "name": q.name })));
                            }
                            if ui.small_button("×").on_hover_text("Remove").clicked() {
                                cmds.push(("db.removeSavedQuery", json!({ "dataSource": source, "name": q.name })));
                            }
                            ui.label(RichText::new(&q.name).strong().color(theme.keyword));
                            ui.add(
                                egui::Label::new(
                                    RichText::new(q.sql.split_whitespace().collect::<Vec<_>>().join(" "))
                                        .monospace()
                                        .color(theme.text_muted),
                                )
                                .truncate(),
                            )
                            .on_hover_text(&q.sql);
                        });
                    }
                    ui.separator();
                }
                if entries.is_empty() {
                    ui.label(RichText::new("No queries yet").color(theme.text_muted));
                }
                for e in &entries {
                    ui.horizontal(|ui| {
                        if ui.small_button("▶").on_hover_text("Insert and run").clicked() {
                            cmds.push(("db.useHistory", json!({ "dataSource": source, "sql": e.sql, "run": true })));
                        }
                        let one_line = e.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                        let resp = ui
                            .add(
                                egui::Label::new(RichText::new(one_line).monospace().color(theme.text_editor))
                                    .truncate()
                                    .sense(egui::Sense::click()),
                            )
                            .on_hover_text(format!(
                                "{}\n\n{} · ran {} time{} — click to insert",
                                e.sql,
                                ago(e.at),
                                e.count,
                                if e.count == 1 { "" } else { "s" }
                            ));
                        if resp.clicked() {
                            cmds.push(("db.useHistory", json!({ "dataSource": source, "sql": e.sql })));
                        }
                    });
                }
            });
        });
    if let Some(p) = s.db.history_popup.as_mut() {
        p.filter = filter;
    }
    if !open || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        cmds.push(("db.closeHistory", Value::Null));
    }
}

fn import(ctx: &egui::Context, s: &Session, st: &UiState, cmds: &mut Cmds) {
    let Some(d) = &s.db.import else { return };
    let mut open = true;
    let theme = &st.theme;
    egui::Window::new(format!("Import Data into {}", d.table))
        .id(egui::Id::new("db-import"))
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(640.0, 460.0))
        .anchor(Align2::CENTER_CENTER, [0.0, -30.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(RichText::new(d.path.display().to_string()).monospace().color(theme.text_muted));
            let reread = |header: bool| {
                json!({
                    "dataSource": d.source, "schema": d.schema, "table": d.table, "path": d.path,
                    "format": d.options.format, "header": header, "emptyAsNull": d.options.empty_as_null,
                })
            };
            let mut header = d.options.header;
            if ui.checkbox(&mut header, "First row is the header").changed() {
                cmds.push(("db.importData", reread(header)));
            }
            match &d.state {
                ImportState::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading the file…");
                    });
                    return;
                }
                ImportState::Failed(e) if d.table_columns.is_empty() => {
                    ui.add(egui::Label::new(RichText::new(e).color(theme.error)).wrap());
                    return;
                }
                _ => {}
            }
            ui.label(format!("{} rows in the file", d.total_rows));
            ui.separator();
            ui.label(RichText::new("Columns").strong());
            egui::Grid::new("db-import-map").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                for (i, col) in d.table_columns.iter().enumerate() {
                    ui.label(RichText::new(col).monospace());
                    let current = d.mapping.get(i).copied().flatten();
                    let label =
                        current.and_then(|c| d.file_columns.get(c).cloned()).unwrap_or_else(|| "<default>".to_string());
                    let mut chosen = current;
                    egui::ComboBox::from_id_salt(("db-import-col", i)).selected_text(label).width(220.0).show_ui(
                        ui,
                        |ui| {
                            ui.selectable_value(&mut chosen, None, "<default>");
                            for (j, f) in d.file_columns.iter().enumerate() {
                                ui.selectable_value(&mut chosen, Some(j), f);
                            }
                        },
                    );
                    if chosen != current {
                        let mut m = serde_json::Map::new();
                        m.insert(col.clone(), json!(chosen));
                        cmds.push(("db.setImportMapping", json!({ "mapping": m })));
                    }
                    ui.end_row();
                }
            });
            ui.separator();
            ui.label(RichText::new("Preview").strong());
            ScrollArea::both().max_height(160.0).show(ui, |ui| {
                egui::Grid::new("db-import-preview").striped(true).show(ui, |ui| {
                    for c in &d.file_columns {
                        ui.label(RichText::new(c).strong());
                    }
                    ui.end_row();
                    for row in &d.preview {
                        for c in row {
                            ui.label(RichText::new(crate::data_grid::cell_text(c)).monospace().size(11.5));
                        }
                        ui.end_row();
                    }
                });
            });
            ui.separator();
            match &d.state {
                ImportState::Importing { done, total } => {
                    let frac = if *total == 0 { 0.0 } else { *done as f32 / *total as f32 };
                    ui.add(egui::ProgressBar::new(frac).text(format!("{done} / {total} batches")));
                }
                ImportState::Done(n) => {
                    ui.label(RichText::new(format!("✔ Imported {n} rows")).color(theme.success));
                }
                ImportState::Failed(e) => {
                    ui.add(egui::Label::new(RichText::new(format!("✖ {e}")).color(theme.error)).wrap());
                }
                _ => {}
            }
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 26.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    cmds.push(("db.closeImport", Value::Null));
                }
                let can = matches!(d.state, ImportState::Ready | ImportState::Failed(_))
                    && d.mapping.iter().any(Option::is_some);
                if ui.add_enabled(can, egui::Button::new("Import").fill(theme.accent)).clicked() {
                    cmds.push(("db.runImport", Value::Null));
                }
            });
        });
    if !open {
        cmds.push(("db.closeImport", Value::Null));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(first_lines("a\nb\nc", 2), "a\nb\n…");
        assert_eq!(first_lines("a", 2), "a");
        assert!(ago(0).ends_with("days ago"));
        assert_eq!(ago(u64::MAX), "just now");
    }
}
