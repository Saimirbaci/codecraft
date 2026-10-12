//! The Database tool window, data source dialog, SQL console toolbar and result grids. OWNED BY the database track.
//!
//! Everything the user does here goes through `db.*` commands; this module only paints the
//! engine's [`codecraft_engine::database::DbState`] and keeps view state (grid widths, selection).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use egui::{
    Align, Align2, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke, TextEdit,
    vec2,
};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::{
    CONSOLES_DIR, ConnStatus, DataSourceForm, OutputLevel, ResultSet, ResultStatus, TestState, node_key,
};
use codecraft_engine::db::{DbKind, TableKind};
use codecraft_syntax::Language;

use crate::data_grid::{self, GridAction, GridData, GridState};
use crate::{UiState, run_command};

/// View state of the database UI (kept in [`UiState`]).
#[derive(Default)]
pub struct DbUi {
    /// Grid state per result id.
    pub grids: HashMap<u64, GridState>,
    /// The last error from the data source dialog's OK.
    pub dialog_error: Option<String>,
    /// A fetch was requested for this result (avoid asking again every frame).
    pub fetch_requested: Option<(u64, usize)>,
}

fn repaint_while_busy(ui: &egui::Ui, s: &Session) {
    if s.db.busy() {
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }
}

fn small_button(ui: &mut egui::Ui, text: &str, tip: &str, enabled: bool, color: Color32, st: &UiState) -> bool {
    let rich = RichText::new(text).size(14.0).color(if enabled { color } else { st.theme.text_disabled });
    ui.add_enabled(enabled, egui::Button::new(rich).frame(false).min_size(vec2(24.0, 22.0)))
        .on_hover_text(tip)
        .clicked()
}

fn status_color(status: &ConnStatus, st: &UiState) -> Color32 {
    match status {
        ConnStatus::Connected(_) => st.theme.success,
        ConnStatus::Connecting => st.theme.warning,
        ConnStatus::Failed(_) => st.theme.error,
        ConnStatus::Disconnected => st.theme.text_muted,
    }
}

// ---------------------------------------------------------------- tree

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Source,
    Database,
    Schema,
    Table,
    Column,
    Folder,
    Leaf,
    Message,
}

struct Row {
    depth: usize,
    kind: Kind,
    /// Selection/expansion key.
    key: String,
    expandable: bool,
    expanded: bool,
    icon: &'static str,
    icon_color: Color32,
    label: String,
    label_color: Color32,
    detail: String,
    tooltip: String,
    source: String,
    schema: Option<String>,
    table: Option<String>,
}

impl Row {
    fn new(depth: usize, kind: Kind, key: String, source: &str) -> Self {
        Self {
            depth,
            kind,
            key,
            expandable: false,
            expanded: false,
            icon: "",
            icon_color: Color32::TRANSPARENT,
            label: String::new(),
            label_color: Color32::TRANSPARENT,
            detail: String::new(),
            tooltip: String::new(),
            source: source.to_string(),
            schema: None,
            table: None,
        }
    }
}

fn message(depth: usize, source: &str, text: &str, color: Color32) -> Row {
    let mut r = Row::new(depth, Kind::Message, String::new(), source);
    r.label = text.to_string();
    r.label_color = color;
    r
}

fn build_rows(s: &Session, st: &UiState) -> Vec<Row> {
    let t = &st.theme;
    let mut rows = Vec::new();
    for ds in &s.db.sources {
        let id = ds.config.id.as_str();
        let key = node_key(id, None, None);
        let expanded = s.db.expanded.contains(&key);
        let mut r = Row::new(0, Kind::Source, key, id);
        r.expandable = true;
        r.expanded = expanded;
        r.icon = "🗄";
        r.icon_color = status_color(&ds.status, st);
        r.label = ds.config.name.clone();
        r.label_color = t.text;
        r.detail = match &ds.status {
            ConnStatus::Connected(v) => v.clone(),
            _ => ds.config.summary(),
        };
        r.tooltip = format!("{} · {}", ds.config.kind.label(), ds.config.summary());
        rows.push(r);
        if !expanded {
            continue;
        }
        if let ConnStatus::Failed(e) = &ds.status {
            rows.push(message(1, id, e, t.error));
            continue;
        }
        if ds.status == ConnStatus::Connecting || ds.tree.loading {
            rows.push(message(1, id, "Loading…", t.text_muted));
            continue;
        }
        if let Some(e) = &ds.tree.error {
            rows.push(message(1, id, e, t.error));
        }
        let Some(info) = &ds.tree.info else {
            if ds.status == ConnStatus::Disconnected {
                rows.push(message(1, id, "Not connected (double-click to connect)", t.text_muted));
            }
            continue;
        };
        let mut depth = 1;
        if !info.databases.is_empty() {
            let mut shown_current = false;
            // The connected database first, then the others (not browsable through this connection).
            let mut dbs: Vec<&String> = info.databases.iter().collect();
            dbs.sort_by_key(|d| info.current.as_deref() != Some(d.as_str()));
            for db in dbs {
                let current = info.current.as_deref() == Some(db.as_str());
                let dkey = format!("{}\u{1f}#db\u{1f}{db}", node_key(id, None, None));
                let mut d = Row::new(1, Kind::Database, dkey.clone(), id);
                d.icon = "▣";
                d.icon_color = if current { t.keyword } else { t.text_disabled };
                d.label = db.clone();
                d.label_color = if current { t.text } else { t.text_muted };
                d.expandable = current;
                d.expanded = current && !s.db.expanded.contains(&format!("{dkey}\u{1f}collapsed"));
                if !current {
                    d.tooltip = "Edit the data source to browse this database".into();
                }
                let open = d.expanded;
                rows.push(d);
                if current && open {
                    shown_current = true;
                    depth = 2;
                    push_schemas(s, st, ds, depth, &mut rows);
                }
            }
            if !shown_current && info.current.is_none() {
                push_schemas(s, st, ds, depth, &mut rows);
            }
        } else {
            push_schemas(s, st, ds, depth, &mut rows);
        }
    }
    rows
}

fn push_schemas(
    s: &Session,
    st: &UiState,
    ds: &codecraft_engine::database::DataSource,
    depth: usize,
    rows: &mut Vec<Row>,
) {
    let t = &st.theme;
    let id = ds.config.id.as_str();
    let default = ds.tree.info.as_ref().and_then(|i| i.default_schema.clone());
    for (schema, node) in &ds.tree.schemas {
        let key = node_key(id, Some(schema), None);
        let expanded = s.db.expanded.contains(&key);
        let mut r = Row::new(depth, Kind::Schema, key, id);
        r.expandable = true;
        r.expanded = expanded;
        r.icon = "⊞";
        r.icon_color = t.type_;
        r.label = schema.clone();
        r.label_color = t.text;
        if default.as_deref() == Some(schema.as_str()) {
            r.detail = "default".into();
        }
        r.schema = Some(schema.clone());
        rows.push(r);
        if !expanded {
            continue;
        }
        if node.loading {
            rows.push(message(depth + 1, id, "Loading…", t.text_muted));
        }
        if let Some(e) = &node.error {
            rows.push(message(depth + 1, id, e, t.error));
        }
        let Some(tables) = &node.tables else { continue };
        if tables.is_empty() && !node.loading {
            rows.push(message(depth + 1, id, "No tables", t.text_muted));
        }
        for table in tables {
            let tkey = node_key(id, Some(schema), Some(&table.info.name));
            let texp = s.db.expanded.contains(&tkey);
            let mut r = Row::new(depth + 1, Kind::Table, tkey.clone(), id);
            r.expandable = true;
            r.expanded = texp;
            let is_table = table.info.kind == TableKind::Table;
            r.icon = if is_table { "☰" } else { "👁" };
            r.icon_color = if is_table { t.function } else { t.constant };
            r.label = table.info.name.clone();
            r.label_color = t.text;
            r.detail = match (table.row_count, table.counting) {
                (_, true) => "counting…".into(),
                (Some(n), _) => format!("{n} rows"),
                _ if !is_table => "view".into(),
                _ => String::new(),
            };
            r.schema = Some(schema.clone());
            r.table = Some(table.info.name.clone());
            rows.push(r);
            if !texp {
                continue;
            }
            let d = depth + 2;
            if table.loading {
                rows.push(message(d, id, "Loading…", t.text_muted));
            }
            if let Some(e) = &table.error {
                rows.push(message(d, id, e, t.error));
            }
            let Some(det) = &table.details else { continue };
            for c in &det.columns {
                let mut r = Row::new(d, Kind::Column, String::new(), id);
                r.icon = if c.primary_key { "🔑" } else { "▪" };
                r.icon_color = if c.primary_key { t.warning } else { t.field };
                r.label = c.name.clone();
                r.label_color = t.text;
                let mut detail = c.data_type.clone();
                if !c.nullable {
                    detail.push_str(" not null");
                }
                if let Some(def) = &c.default {
                    detail.push_str(&format!(" = {def}"));
                }
                r.detail = detail;
                rows.push(r);
            }
            for (suffix, title, items) in [
                (
                    "#idx",
                    "indexes",
                    det.indexes
                        .iter()
                        .map(|i| {
                            let flags = if i.primary {
                                " primary"
                            } else if i.unique {
                                " unique"
                            } else {
                                ""
                            };
                            (i.name.clone(), format!("({}){flags}", i.columns.join(", ")))
                        })
                        .collect::<Vec<_>>(),
                ),
                (
                    "#fk",
                    "foreign keys",
                    det.foreign_keys
                        .iter()
                        .map(|f| {
                            (
                                f.name.clone(),
                                format!("({}) → {}({})", f.columns.join(", "), f.ref_table, f.ref_columns.join(", ")),
                            )
                        })
                        .collect(),
                ),
            ] {
                if items.is_empty() {
                    continue;
                }
                let fkey = format!("{tkey}\u{1f}{suffix}");
                let fexp = s.db.expanded.contains(&fkey);
                let mut r = Row::new(d, Kind::Folder, fkey, id);
                r.expandable = true;
                r.expanded = fexp;
                r.icon = "🗀";
                r.icon_color = t.text_muted;
                r.label = title.to_string();
                r.label_color = t.text_muted;
                r.detail = items.len().to_string();
                rows.push(r);
                if fexp {
                    for (name, detail) in items {
                        let mut r = Row::new(d + 1, Kind::Leaf, String::new(), id);
                        r.icon = if suffix == "#fk" { "🔗" } else { "⚡" };
                        r.icon_color = if suffix == "#fk" { t.link } else { t.keyword };
                        r.label = name;
                        r.label_color = t.text;
                        r.detail = detail;
                        rows.push(r);
                    }
                }
            }
        }
    }
}

enum TreeAction {
    Cmd(&'static str, Value),
    ToggleLocal(String),
}

/// The Database tool window.
pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    repaint_while_busy(ui, s);
    let selected_source = s.db.selected.as_ref().and_then(|k| k.split('\u{1f}').next()).map(str::to_string);
    let has_source = selected_source.as_ref().is_some_and(|id| s.db.source(id).is_some());
    let mut actions: Vec<TreeAction> = Vec::new();
    // Toolbar.
    ui.horizontal(|ui| {
        let add = ui.menu_button(RichText::new("+").size(16.0).color(st.theme.text), |ui| {
            for kind in DbKind::ALL {
                if ui.button(kind.label()).clicked() {
                    actions.push(TreeAction::Cmd("db.addDataSource", json!({ "kind": kind })));
                    ui.close();
                }
            }
        });
        add.response.on_hover_text("New data source");
        if small_button(ui, "⟳", "Refresh", !s.db.sources.is_empty(), st.theme.text, st) {
            actions.push(TreeAction::Cmd("db.refresh", Value::Null));
        }
        if small_button(ui, "🖹", "Open the query console", has_source, st.theme.text, st) {
            actions.push(TreeAction::Cmd("db.newConsole", Value::Null));
        }
        if small_button(ui, "✏", "Data source properties", has_source, st.theme.text, st) {
            actions.push(TreeAction::Cmd("db.editDataSource", json!({ "dataSource": selected_source })));
        }
        let connected = s.db.sources.iter().any(|d| d.status != ConnStatus::Disconnected);
        if small_button(ui, "🔌", "Disconnect", connected, st.theme.error, st) {
            actions.push(TreeAction::Cmd("db.disconnect", Value::Null));
        }
    });
    ui.add(egui::Separator::default().spacing(4.0));
    if s.db.sources.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("No data sources").color(st.theme.text_muted));
            ui.add_space(6.0);
            for kind in [DbKind::Postgres, DbKind::Sqlite, DbKind::Mysql] {
                if ui.link(format!("Add {}…", kind.label())).clicked() {
                    actions.push(TreeAction::Cmd("db.addDataSource", json!({ "kind": kind })));
                }
            }
        });
    } else {
        let rows = build_rows(s, st);
        let row_h = 22.0;
        let theme = st.theme.clone();
        ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
            for row in rows.get(range).unwrap_or(&[]) {
                let width = ui.available_width().max(220.0);
                let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                let selected = !row.key.is_empty() && s.db.selected.as_deref() == Some(row.key.as_str());
                if selected {
                    ui.painter().rect_filled(rect, CornerRadius::same(4), theme.list_selection);
                } else if resp.hovered() && row.kind != Kind::Message {
                    ui.painter().rect_filled(rect, CornerRadius::same(4), theme.hover);
                }
                let x0 = rect.left() + 4.0 + row.depth as f32 * 16.0;
                let cy = rect.center().y;
                if row.expandable {
                    let arrow = if row.expanded { "⏷" } else { "⏵" };
                    ui.painter().text(
                        egui::pos2(x0 + 5.0, cy),
                        Align2::CENTER_CENTER,
                        arrow,
                        FontId::proportional(10.0),
                        theme.text_muted,
                    );
                }
                let mut x = x0 + 14.0;
                if !row.icon.is_empty() {
                    ui.painter().text(
                        egui::pos2(x + 6.0, cy),
                        Align2::CENTER_CENTER,
                        row.icon,
                        FontId::proportional(12.0),
                        row.icon_color,
                    );
                    x += 16.0;
                }
                let mut job = egui::text::LayoutJob::default();
                job.append(
                    &row.label,
                    0.0,
                    egui::TextFormat {
                        font_id: FontId::proportional(13.0),
                        color: row.label_color,
                        ..Default::default()
                    },
                );
                if !row.detail.is_empty() {
                    job.append(
                        &format!("  {}", row.detail),
                        0.0,
                        egui::TextFormat {
                            font_id: FontId::proportional(12.0),
                            color: theme.text_muted,
                            ..Default::default()
                        },
                    );
                }
                let galley = ui.fonts_mut(|f| f.layout_job(job));
                ui.painter().galley(egui::pos2(x + 2.0, cy - galley.size().y / 2.0), galley, theme.text);
                let resp = if row.tooltip.is_empty() { resp } else { resp.on_hover_text(&row.tooltip) };
                if resp.clicked() && !row.key.is_empty() {
                    s.db.selected = Some(row.key.clone());
                    let on_arrow = resp.interact_pointer_pos().is_some_and(|p| p.x < x0 + 12.0);
                    if row.expandable && on_arrow {
                        actions.push(toggle(row));
                    }
                }
                if resp.double_clicked() {
                    match row.kind {
                        Kind::Table => actions.push(TreeAction::Cmd(
                            "db.openTable",
                            json!({ "dataSource": row.source, "schema": row.schema, "table": row.table }),
                        )),
                        _ if row.expandable => actions.push(toggle(row)),
                        _ => {}
                    }
                }
                resp.context_menu(|ui| context_menu(ui, row, &mut actions));
            }
        });
    }
    for a in actions {
        match a {
            TreeAction::Cmd(id, params) => run_command(s, st, id, &params),
            TreeAction::ToggleLocal(key) => {
                if !s.db.expanded.remove(&key) {
                    s.db.expanded.insert(key);
                }
            }
        }
    }
}

fn toggle(row: &Row) -> TreeAction {
    match row.kind {
        Kind::Source => TreeAction::Cmd("db.expand", json!({ "dataSource": row.source, "expanded": !row.expanded })),
        Kind::Schema => TreeAction::Cmd(
            "db.expand",
            json!({ "dataSource": row.source, "schema": row.schema, "expanded": !row.expanded }),
        ),
        Kind::Table => TreeAction::Cmd(
            "db.expand",
            json!({ "dataSource": row.source, "schema": row.schema, "table": row.table, "expanded": !row.expanded }),
        ),
        // The current database is open by default: its key records "collapsed".
        Kind::Database => TreeAction::ToggleLocal(format!("{}\u{1f}collapsed", row.key)),
        _ => TreeAction::ToggleLocal(row.key.clone()),
    }
}

fn context_menu(ui: &mut egui::Ui, row: &Row, actions: &mut Vec<TreeAction>) {
    let ds = json!({ "dataSource": row.source });
    let mut item = |ui: &mut egui::Ui, label: &str, id: &'static str, params: Value| {
        if ui.button(label).clicked() {
            actions.push(TreeAction::Cmd(id, params));
            ui.close();
        }
    };
    match row.kind {
        Kind::Source => {
            item(ui, "Connect", "db.connect", ds.clone());
            item(ui, "Disconnect", "db.disconnect", ds.clone());
            item(ui, "Refresh", "db.refresh", ds.clone());
            ui.separator();
            item(ui, "Open Query Console", "db.newConsole", ds.clone());
            item(ui, "New Query Console", "db.newConsole", json!({ "dataSource": row.source, "new": true }));
            ui.separator();
            item(ui, "Test Connection", "db.testConnection", ds.clone());
            item(ui, "Properties…", "db.editDataSource", ds.clone());
            item(ui, "Remove", "db.removeDataSource", ds);
        }
        Kind::Schema | Kind::Database => {
            item(ui, "Refresh", "db.refresh", ds.clone());
            item(ui, "Open Query Console", "db.newConsole", ds);
        }
        Kind::Table => {
            let t = json!({ "dataSource": row.source, "schema": row.schema, "table": row.table });
            item(ui, "Open Data", "db.openTable", t.clone());
            item(ui, "Count Rows", "db.rowCount", t);
            if ui.button("Copy Name").clicked() {
                ui.ctx().copy_text(row.label.clone());
                ui.close();
            }
        }
        _ => {
            if ui.button("Copy Name").clicked() {
                ui.ctx().copy_text(row.label.clone());
                ui.close();
            }
        }
    }
}

// ---------------------------------------------------------------- data source dialog

fn dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let root = s.db.project_root().map(std::path::Path::to_path_buf);
    let Some(d) = s.db.dialog.as_mut() else {
        st.db.dialog_error = None;
        return;
    };
    let editing = d.editing.clone();
    let title = if editing.is_some() { "Data Source Properties" } else { "New Data Source" };
    let mut open = true;
    let mut action: Option<&'static str> = None;
    let theme = st.theme.clone();
    let error = st.db.dialog_error.clone();
    egui::Window::new(title)
        .id(egui::Id::new("db-datasource-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(480.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ctx, |ui| {
            let f = &mut d.form;
            egui::Grid::new("db-ds-form").num_columns(2).spacing([12.0, 8.0]).min_col_width(90.0).show(ui, |ui| {
                ui.label("Driver");
                let mut kind = f.kind;
                egui::ComboBox::from_id_salt("db-kind").selected_text(kind.label()).width(220.0).show_ui(ui, |ui| {
                    for k in DbKind::ALL {
                        ui.selectable_value(&mut kind, k, k.label());
                    }
                });
                if kind != f.kind {
                    f.set_kind(kind);
                }
                ui.end_row();
                ui.label("Name");
                ui.add(TextEdit::singleline(&mut f.name).hint_text("optional").desired_width(300.0));
                ui.end_row();
                if f.kind.is_file_based() {
                    ui.label("File");
                    ui.horizontal(|ui| {
                        ui.add(
                            TextEdit::singleline(&mut f.path)
                                .hint_text("data.db (relative to the project)")
                                .desired_width(250.0),
                        );
                        if ui.button("…").on_hover_text("Choose a database file").clicked() {
                            let mut dlg = rfd::FileDialog::new().set_title("SQLite Database File");
                            if let Some(root) = &root {
                                dlg = dlg.set_directory(root);
                            }
                            if let Some(p) = dlg.pick_file() {
                                f.path = p.display().to_string();
                            }
                        }
                    });
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut f.read_only, "Read-only");
                    ui.end_row();
                } else {
                    ui.label("Host");
                    ui.horizontal(|ui| {
                        ui.add(TextEdit::singleline(&mut f.host).desired_width(190.0));
                        ui.label("Port");
                        ui.add(TextEdit::singleline(&mut f.port).desired_width(60.0));
                    });
                    ui.end_row();
                    ui.label("User");
                    ui.add(
                        TextEdit::singleline(&mut f.user)
                            .hint_text(std::env::var("USER").unwrap_or_default())
                            .desired_width(300.0),
                    );
                    ui.end_row();
                    ui.label("Password");
                    let hint = if editing.is_some() { "<unchanged>" } else { "" };
                    ui.add(TextEdit::singleline(&mut f.password).password(true).hint_text(hint).desired_width(300.0));
                    ui.end_row();
                    ui.label("");
                    ui.checkbox(&mut f.save_password, "Save password in the Keychain");
                    ui.end_row();
                    ui.label("Database");
                    ui.add(TextEdit::singleline(&mut f.database).desired_width(300.0));
                    ui.end_row();
                    ui.label("Options");
                    ui.add(
                        TextEdit::singleline(&mut f.options)
                            .hint_text("sslmode=require; connect_timeout=10")
                            .desired_width(300.0),
                    );
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(d.test != TestState::Running, egui::Button::new("Test Connection")).clicked() {
                    action = Some("test");
                }
                match &d.test {
                    TestState::Idle => {}
                    TestState::Running => {
                        ui.spinner();
                        ui.label(RichText::new("Connecting…").color(theme.text_muted));
                    }
                    TestState::Ok(v) => {
                        ui.label(RichText::new(format!("✔ {v}")).color(theme.success));
                    }
                    TestState::Failed(e) => {
                        ui.add(egui::Label::new(RichText::new(format!("✖ {e}")).color(theme.error)).wrap());
                    }
                }
            });
            if let Some(e) = &error {
                ui.add(egui::Label::new(RichText::new(e).color(theme.error)).wrap());
            }
            ui.add_space(6.0);
            let row = vec2(ui.available_width(), 26.0);
            ui.allocate_ui_with_layout(row, Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    action = Some("cancel");
                }
                if ui.add(egui::Button::new("OK").fill(theme.accent)).clicked() {
                    action = Some("ok");
                }
            });
        });
    if !open {
        action = Some("cancel");
    }
    let Some(action) = action else { return };
    let form: DataSourceForm = s.db.dialog.as_ref().map(|d| d.form.clone()).unwrap_or_default();
    match action {
        "test" => run_command(s, st, "db.testConnection", &Value::Null),
        "ok" => {
            let mut params = form.to_params();
            let id = match &editing {
                Some(id) => {
                    if let Some(o) = params.as_object_mut() {
                        o.insert("dataSource".into(), json!(id));
                    }
                    "db.editDataSource"
                }
                None => "db.addDataSource",
            };
            match s.execute(id, &params) {
                Ok(_) => {
                    s.db.dialog = None;
                    st.db.dialog_error = None;
                }
                Err(e) => st.db.dialog_error = Some(e.to_string()),
            }
        }
        _ => {
            run_command(s, st, "db.closeDialog", &Value::Null);
            st.db.dialog_error = None;
        }
    }
}

// ---------------------------------------------------------------- SQL console toolbar

/// The bar above an SQL editor (data source, Execute ▶). Also draws the data source dialog,
/// which floats above the window. Returns quickly when the active document isn't SQL.
pub fn console_toolbar(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    dialog(ui.ctx(), s, st);
    let Some((_, d)) = s.ws.active() else { return };
    if d.language != Language::Sql {
        return;
    }
    repaint_while_busy(ui, s);
    let path: Option<PathBuf> = d.path.clone();
    let source = s.db.source_for_path(path.as_deref());
    let is_console = match (s.db.project_root(), &path) {
        (Some(root), Some(p)) => p.starts_with(root.join(CONSOLES_DIR)),
        _ => false,
    };
    let running = s.enabled("db.cancel").is_ok() && s.db.running();
    let mut cmd: Option<(&'static str, Value)> = None;
    Frame::new()
        .fill(st.theme.header_bg)
        .inner_margin(Margin::symmetric(8, 3))
        .stroke(Stroke::new(1.0, st.theme.border))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let can_run = source.is_some();
                if ui
                    .add_enabled(can_run, egui::Button::new(RichText::new("▶").color(st.theme.success)).frame(false))
                    .on_hover_text("Execute (⌘↩)")
                    .clicked()
                {
                    cmd = Some(("db.execute", Value::Null));
                }
                if small_button(ui, "⏩", "Execute All", can_run, st.theme.success, st) {
                    cmd = Some(("db.executeAll", Value::Null));
                }
                if small_button(ui, "■", "Cancel", running, st.theme.error, st) {
                    cmd = Some(("db.cancel", Value::Null));
                }
                ui.separator();
                let current = source.as_ref().and_then(|id| s.db.source(id));
                let (dot, label) = match current {
                    Some(ds) => (status_color(&ds.status, st), ds.config.name.clone()),
                    None => (st.theme.text_muted, "<choose a data source>".to_string()),
                };
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 4.0, dot);
                if is_console {
                    ui.label(RichText::new(format!("🗄 {label}")).color(st.theme.text));
                } else {
                    let mut chosen = source.clone();
                    egui::ComboBox::from_id_salt("db-console-source").selected_text(label).width(200.0).show_ui(
                        ui,
                        |ui| {
                            for ds in &s.db.sources {
                                ui.selectable_value(&mut chosen, Some(ds.config.id.clone()), &ds.config.name);
                            }
                        },
                    );
                    if chosen != source
                        && let Some(id) = chosen
                    {
                        cmd = Some(("db.setDataSource", json!({ "dataSource": id, "path": path })));
                    }
                }
                if let Some(ConnStatus::Failed(e)) = current.map(|d| &d.status) {
                    ui.label(RichText::new("connection failed").color(st.theme.error)).on_hover_text(e);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if running {
                        ui.spinner();
                    }
                });
            });
        });
    if let Some((id, params)) = cmd {
        run_command(s, st, id, &params);
    }
}

// ---------------------------------------------------------------- Services (results)

fn result_icon(r: &ResultSet, st: &UiState) -> (&'static str, Color32) {
    match r.status {
        ResultStatus::Running | ResultStatus::Fetching => ("⏳", st.theme.warning),
        ResultStatus::Failed => ("✖", st.theme.error),
        ResultStatus::Cancelled => ("■", st.theme.text_muted),
        ResultStatus::Done => ("☰", st.theme.function),
    }
}

/// The Services tool window: the Output log and one tab per result set.
pub fn services(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    repaint_while_busy(ui, s);
    let mut cmd: Vec<(&'static str, Value)> = Vec::new();
    // Tabs.
    ScrollArea::horizontal().id_salt("db-result-tabs").max_height(28.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            let out_sel = s.db.active_result.is_none();
            if ui.selectable_label(out_sel, "Output").clicked() {
                cmd.push(("db.selectResult", Value::Null));
            }
            for r in &s.db.results {
                let sel = s.db.active_result == Some(r.id);
                let (icon, color) = result_icon(r, st);
                let mut job = egui::text::LayoutJob::default();
                job.append(
                    icon,
                    0.0,
                    egui::TextFormat { color, font_id: FontId::proportional(12.0), ..Default::default() },
                );
                job.append(
                    &format!(" {}", r.title),
                    0.0,
                    egui::TextFormat {
                        color: st.theme.text,
                        font_id: FontId::proportional(13.0),
                        ..Default::default()
                    },
                );
                if ui.selectable_label(sel, job).on_hover_text(&r.sql).clicked() {
                    cmd.push(("db.selectResult", json!({ "result": r.id })));
                }
                if ui
                    .add(egui::Button::new(RichText::new("×").color(st.theme.text_muted)).frame(false))
                    .on_hover_text("Close")
                    .clicked()
                {
                    cmd.push(("db.closeResult", json!({ "result": r.id })));
                }
            }
        });
    });
    ui.add(egui::Separator::default().spacing(2.0));
    match s.db.active().cloned() {
        None => output(ui, s, st),
        Some(r) => result_view(ui, s, st, &r, &mut cmd),
    }
    for (id, p) in cmd {
        run_command(s, st, id, &p);
    }
    // Forget grid state of closed results.
    let ids: Vec<u64> = s.db.results.iter().map(|r| r.id).collect();
    st.db.grids.retain(|id, _| ids.contains(id));
}

fn output(ui: &mut egui::Ui, s: &Session, st: &UiState) {
    if s.db.output.is_empty() {
        ui.add_space(12.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Execute a query in an SQL console (⌘↩) to see results here").color(st.theme.text_muted),
            );
        });
        return;
    }
    ScrollArea::both().id_salt("db-output").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        for line in &s.db.output {
            let color = match line.level {
                OutputLevel::Info => st.theme.text_muted,
                OutputLevel::Sql => st.theme.text_editor,
                OutputLevel::Error => st.theme.error,
            };
            let prefix = if line.source.is_empty() { String::new() } else { format!("[{}] ", line.source) };
            ui.label(RichText::new(format!("{prefix}{}", line.text)).monospace().color(color));
        }
    });
}

fn result_view(
    ui: &mut egui::Ui,
    s: &mut Session,
    st: &mut UiState,
    r: &ResultSet,
    cmd: &mut Vec<(&'static str, Value)>,
) {
    let id = json!({ "result": r.id });
    ui.horizontal(|ui| {
        if small_button(ui, "■", "Cancel", r.is_running() || r.has_more, st.theme.error, st) {
            cmd.push(("db.cancel", id.clone()));
        }
        if small_button(ui, "⬇", "Fetch the next page (500 rows)", r.has_more, st.theme.text, st) {
            cmd.push(("db.fetchMore", id.clone()));
        }
        if small_button(ui, "💾", "Export to CSV…", !r.columns.is_empty(), st.theme.text, st) {
            let name = format!("{}.csv", r.title.replace(['/', '\\', ' '], "_"));
            let mut dlg = rfd::FileDialog::new().set_title("Export to CSV").set_file_name(name);
            if let Some(root) = s.db.project_root() {
                dlg = dlg.set_directory(root);
            }
            if let Some(p) = dlg.save_file() {
                cmd.push(("db.exportCsv", json!({ "result": r.id, "path": p })));
            }
        }
        let sql: String = r.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        ui.add(egui::Label::new(RichText::new(sql).monospace().size(11.5).color(st.theme.text_muted)).truncate());
    });
    let status = r.status_line();
    let data = GridData {
        columns: &r.columns,
        rows: &r.rows,
        sort: r.sort.map(|k| (k.column, k.desc)),
        status: &status,
        status_is_error: r.status == ResultStatus::Failed,
    };
    let grid = st.db.grids.entry(r.id).or_default();
    let actions = data_grid::show(ui, egui::Id::new(("db-grid", r.id)), grid, &data, &st.theme);
    for a in actions {
        match a {
            GridAction::Sort(c) if !r.is_running() => {
                cmd.push(("db.sortResult", json!({ "result": r.id, "column": c })))
            }
            GridAction::Sort(_) => {}
            GridAction::NearEnd => {
                let marker = (r.id, r.rows.len());
                if r.has_more && st.db.fetch_requested != Some(marker) {
                    st.db.fetch_requested = Some(marker);
                    cmd.push(("db.fetchMore", id.clone()));
                }
            }
            GridAction::Copy(text) => {
                ui.ctx().copy_text(text);
                s.status = "Copied".into();
            }
        }
    }
}
