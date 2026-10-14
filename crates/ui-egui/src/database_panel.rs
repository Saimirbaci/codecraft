//! The Database tool window, data source dialog, SQL console toolbar and result grids. OWNED BY the database track.
//!
//! Everything the user does here goes through `db.*` commands; this module only paints the
//! engine's [`codecraft_engine::database::DbState`] and keeps view state (grid widths, selection,
//! the filter row's text, the value viewer). Query plans show as a tree; the console toolbar has
//! Explain, the query history and the transaction mode with Commit/Rollback.

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
    CONSOLES_DIR, ConnStatus, DataSourceForm, OutputLevel, Plan, PlanNode, ResultFilter, ResultSet, ResultStatus,
    TestState, console_worker, node_key,
};
use codecraft_engine::db::value::{ValueKind, format_number};
use codecraft_engine::db::{Cell, Column, DATA_SOURCE_COLORS, DbKind, TableKind};
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
    /// The filter row's text per result.
    pub filters: HashMap<u64, FilterEdit>,
    /// The value viewer panel is open.
    pub value_viewer: bool,
    /// The value viewer's decoded image: (result, row, column) → texture (`None`: not decodable).
    viewer_image: Option<((u64, usize, usize), Option<egui::TextureHandle>)>,
    /// A transposed result: (result, rows, columns) → columns and rows.
    transposed: Option<TransposedCache>,
    /// The name typed in the history popup's Save field.
    pub save_name: String,
}

type TransposedCache = ((u64, usize, usize), Vec<Column>, Vec<Vec<Cell>>);

/// The filter row being edited, and the applied filter it started from.
#[derive(Clone, Debug, Default)]
pub struct FilterEdit {
    pub where_: String,
    pub order_by: String,
    applied: ResultFilter,
}

/// Records shown in a transposed grid.
const MAX_TRANSPOSED: usize = 1000;

/// A data source colour name as a theme colour.
fn source_color(name: Option<&str>, st: &UiState) -> Option<Color32> {
    let t = &st.theme;
    Some(match name? {
        "red" => t.error,
        "orange" => t.warning,
        "yellow" => t.attribute,
        "green" => t.success,
        "blue" => t.link,
        "violet" => t.constant,
        "gray" => t.text_muted,
        _ => return None,
    })
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
    /// The data source colour, drawn as a bar at the row's left edge.
    stripe: Option<Color32>,
    /// What the schema tools' menu sees: `table`, `view`, `column`, `index`, `foreignKey` (track DA).
    object: &'static str,
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
            stripe: None,
            object: "",
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
        if ds.config.read_only {
            r.detail.push_str(" · read-only");
        }
        if ds.config.confirm_writes {
            r.detail.push_str(" · production");
        }
        r.stripe = source_color(ds.config.color_name(), st);
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
            r.object = match table.info.kind {
                TableKind::Table | TableKind::Foreign => "table",
                TableKind::View => "view",
                TableKind::MaterializedView => "materializedView",
            };
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
                r.schema = Some(schema.clone());
                r.table = Some(table.info.name.clone());
                r.object = "column";
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
                        r.schema = Some(schema.clone());
                        r.table = Some(table.info.name.clone());
                        r.object = if suffix == "#fk" { "foreignKey" } else { "index" };
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
                if let Some(c) = row.stripe {
                    let bar = egui::Rect::from_min_size(rect.min, vec2(3.0, rect.height()));
                    ui.painter().rect_filled(bar, CornerRadius::same(1), c);
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
    base_context_menu(ui, row, actions);
    // Schema tools (track DA): New Table, Modify, SQL Generator, Diagram, Compare, Rename, Drop…
    let kind = match row.kind {
        Kind::Source => "source",
        Kind::Schema => "schema",
        _ => row.object,
    };
    let target = crate::database_schema_ui::NodeTarget {
        kind,
        source: &row.source,
        schema: row.schema.as_deref(),
        table: row.table.as_deref(),
        name: &row.label,
    };
    let mut cmds = Vec::new();
    crate::database_schema_ui::tree_menu(ui, &target, &mut cmds);
    actions.extend(cmds.into_iter().map(|(id, p)| TreeAction::Cmd(id, p)));
}

fn base_context_menu(ui: &mut egui::Ui, row: &Row, actions: &mut Vec<TreeAction>) {
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
            item(ui, "Query History", "db.showHistory", ds.clone());
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
            item(ui, "Count Rows", "db.rowCount", t.clone());
            if ui.button("Import Data from File…").clicked() {
                ui.close();
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Import Data")
                    .add_filter("CSV, TSV, JSON", &["csv", "tsv", "tab", "txt", "json"])
                    .pick_file()
                {
                    let mut p = t;
                    if let Some(o) = p.as_object_mut() {
                        o.insert("path".into(), json!(path));
                    }
                    actions.push(TreeAction::Cmd("db.importData", p));
                }
            }
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
    crate::windows::resizable(egui::Window::new(title).default_width(480.0), st, "db-datasource")
        .id(egui::Id::new("db-datasource-dialog"))
        .open(&mut open)
        .collapsible(false)
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
                    ui.label("");
                    ui.checkbox(&mut f.read_only, "Read-only");
                    ui.end_row();
                    ui.label("SSH tunnel");
                    ui.add(
                        TextEdit::singleline(&mut f.ssh_host)
                            .hint_text("host, user@host:port or ~/.ssh/config host (empty: none)")
                            .desired_width(300.0),
                    )
                    .on_hover_text(
                        "Connect through `ssh -L`: an IP or host name, user@host:port, or a Host of ~/.ssh/config \
                         or Tools › Deployment. The database host and port above are as seen from the SSH server.",
                    );
                    ui.end_row();
                    if !f.ssh_host.trim().is_empty() {
                        ui.label("SSH port");
                        ui.add(TextEdit::singleline(&mut f.ssh_port).hint_text("22").desired_width(80.0));
                        ui.end_row();
                        ui.label("SSH user");
                        ui.add(TextEdit::singleline(&mut f.ssh_user).hint_text("from ~/.ssh/config or your login").desired_width(300.0));
                        ui.end_row();
                        ui.label("SSH auth");
                        egui::ComboBox::from_id_salt("db-ssh-auth").selected_text(f.ssh_auth.label()).width(220.0).show_ui(ui, |ui| {
                            for a in codecraft_engine::db::SshAuth::ALL {
                                ui.selectable_value(&mut f.ssh_auth, a, a.label());
                            }
                        });
                        ui.end_row();
                        if f.ssh_auth == codecraft_engine::db::SshAuth::Key {
                            ui.label("Private key");
                            ui.horizontal(|ui| {
                                ui.add(TextEdit::singleline(&mut f.ssh_key_file).hint_text("~/.ssh/id_ed25519").desired_width(240.0));
                                if ui.button("…").on_hover_text("Choose the private key file").clicked()
                                    && let Some(path) = rfd::FileDialog::new()
                                        .set_directory(std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".ssh")).unwrap_or_default())
                                        .pick_file()
                                {
                                    f.ssh_key_file = path.display().to_string();
                                }
                            });
                            ui.end_row();
                        }
                        if f.ssh_auth != codecraft_engine::db::SshAuth::Agent {
                            let what = if f.ssh_auth == codecraft_engine::db::SshAuth::Password { "SSH password" } else { "Key passphrase" };
                            ui.label(what);
                            let hint = if editing.is_some() { "<unchanged>" } else if f.ssh_auth == codecraft_engine::db::SshAuth::Key { "empty if the key has none" } else { "" };
                            ui.add(TextEdit::singleline(&mut f.ssh_password).password(true).hint_text(hint).desired_width(300.0));
                            ui.end_row();
                            ui.label("");
                            ui.checkbox(&mut f.save_ssh_password, format!("Save {} in the Keychain", what.to_lowercase()));
                            ui.end_row();
                        }
                    }
                }
                ui.label("Colour");
                let current = if f.color.is_empty() { "None".to_string() } else { f.color.clone() };
                egui::ComboBox::from_id_salt("db-color").selected_text(current).width(120.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut f.color, String::new(), "None");
                    for c in DATA_SOURCE_COLORS {
                        ui.selectable_value(&mut f.color, c.to_string(), c);
                    }
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut f.confirm_writes, "Production: confirm data modifications")
                    .on_hover_text("Ask before running statements that change data or schema");
                ui.end_row();
                ui.label("Transactions");
                ui.checkbox(&mut f.manual_commit, "Manual commit (Commit / Rollback in consoles)");
                ui.end_row();
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
    crate::database_dialogs::show(ui.ctx(), s, st);
    crate::database_schema_ui::show(ui.ctx(), s, st);
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
    let worker = console_worker(path.as_deref(), d.id.0);
    let tx_open = s.db.transaction_open(&worker);
    let mut cmd: Option<(&'static str, Value)> = None;
    let frame = Frame::new()
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
                ui.add_enabled_ui(can_run, |ui| {
                    ui.menu_button(RichText::new("Explain").size(12.5).color(st.theme.text), |ui| {
                        if ui.button("Explain Plan").clicked() {
                            cmd = Some(("db.explainPlan", Value::Null));
                            ui.close();
                        }
                        if ui.button("Explain Analyze").on_hover_text("Runs the statement").clicked() {
                            cmd = Some(("db.explainAnalyze", Value::Null));
                            ui.close();
                        }
                    })
                    .response
                    .on_hover_text("Explain Plan for the statement at the caret");
                });
                if small_button(ui, "🕘", "Query History", can_run, st.theme.text, st) {
                    cmd = Some(("db.showHistory", Value::Null));
                }
                ui.separator();
                if let Some(ds) = source.as_ref().and_then(|id| s.db.source(id)) {
                    let manual = ds.config.manual_commit;
                    let mut chosen = manual;
                    egui::ComboBox::from_id_salt("db-tx-mode")
                        .selected_text(if manual { "Tx: Manual" } else { "Tx: Auto" })
                        .width(92.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut chosen, false, "Auto-commit");
                            ui.selectable_value(&mut chosen, true, "Manual commit");
                        });
                    if chosen != manual {
                        cmd = Some(("db.setTransactionMode", json!({ "manual": chosen, "dataSource": ds.config.id })));
                    }
                    if small_button(ui, "✔", "Commit", tx_open, st.theme.success, st) {
                        cmd = Some(("db.commit", json!({ "connection": worker })));
                    }
                    if small_button(ui, "↶", "Rollback", tx_open, st.theme.error, st) {
                        cmd = Some(("db.rollback", json!({ "connection": worker })));
                    }
                    if tx_open {
                        ui.label(RichText::new("● pending transaction").size(12.0).color(st.theme.warning));
                    }
                    ui.separator();
                }
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
                if current.is_some_and(|d| d.config.read_only) {
                    ui.label(RichText::new("read-only").size(12.0).color(st.theme.text_muted));
                }
                if current.is_some_and(|d| d.config.confirm_writes) {
                    ui.label(RichText::new("production").size(12.0).color(st.theme.warning))
                        .on_hover_text("Data modifications ask for confirmation");
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if running {
                        ui.spinner();
                    }
                });
            });
        });
    let color = source.as_ref().and_then(|id| s.db.source(id)).and_then(|d| source_color(d.config.color_name(), st));
    if let Some(c) = color {
        let r = frame.response.rect;
        ui.painter().rect_filled(egui::Rect::from_min_size(r.min, vec2(r.width(), 2.0)), CornerRadius::ZERO, c);
    }
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
                let (icon, color) = if r.plan.is_some() { ("⑂", st.theme.keyword) } else { (icon, color) };
                let mut job = egui::text::LayoutJob::default();
                if let Some(c) = s.db.source(&r.source).and_then(|d| source_color(d.config.color_name(), st)) {
                    job.append(
                        "● ",
                        0.0,
                        egui::TextFormat { color: c, font_id: FontId::proportional(10.0), ..Default::default() },
                    );
                }
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
    if let Some(plan) = &r.plan {
        plan_view(ui, st, r, plan, cmd);
        return;
    }
    ui.horizontal(|ui| {
        if small_button(ui, "■", "Cancel", r.is_running() || r.has_more, st.theme.error, st) {
            cmd.push(("db.cancel", id.clone()));
        }
        if small_button(ui, "⬇", "Fetch the next page (500 rows)", r.has_more, st.theme.text, st) {
            cmd.push(("db.fetchMore", id.clone()));
        }
        let has_rows = !r.columns.is_empty();
        ui.add_enabled_ui(has_rows, |ui| {
            ui.menu_button(RichText::new("💾").size(14.0).color(st.theme.text), |ui| {
                ui.label(RichText::new("Export to file").color(st.theme.text_muted));
                for (format, label) in data_grid::COPY_FORMATS {
                    if ui.button(format!("{label}…")).clicked() {
                        ui.close();
                        let ext = match format {
                            "markdown" => "md",
                            other => other,
                        };
                        let name = format!("{}.{ext}", r.title.replace(['/', '\\', ' '], "_"));
                        let mut dlg = rfd::FileDialog::new().set_title("Export Data").set_file_name(name);
                        if let Some(root) = s.db.project_root() {
                            dlg = dlg.set_directory(root);
                        }
                        if let Some(p) = dlg.save_file() {
                            cmd.push(("db.exportResult", json!({ "result": r.id, "path": p, "format": format })));
                        }
                    }
                }
                ui.separator();
                ui.label(RichText::new("Copy all rows as").color(st.theme.text_muted));
                for (format, label) in data_grid::COPY_FORMATS {
                    if ui.button(label).clicked() {
                        cmd.push(("db.copyResult", json!({ "result": r.id, "format": format })));
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Export / Copy As");
        });
        let t_color = if r.transposed { st.theme.accent } else { st.theme.text };
        if small_button(ui, "⇄", "Transpose (rows as columns)", has_rows, t_color, st) {
            cmd.push(("db.transposeResult", id.clone()));
        }
        let v_color = if st.db.value_viewer { st.theme.accent } else { st.theme.text };
        if small_button(ui, "🔍", "Value viewer", has_rows, v_color, st) {
            st.db.value_viewer = !st.db.value_viewer;
        }
        let editable = s.db.editable(r.id);
        if let Some(t) = &r.table {
            ui.separator();
            let can = editable.is_ok() && !r.transposed;
            let pending = !r.edits.is_empty() && !r.edits.submitting;
            if small_button(ui, "+", "Add Row", can, st.theme.success, st) {
                cmd.push(("db.addRow", id.clone()));
            }
            let selected_row = match st.db.grids.get(&r.id).and_then(|g| g.selection) {
                Some(data_grid::Selection::Row(row) | data_grid::Selection::Cell { row, .. }) => Some(row),
                None => None,
            };
            if small_button(ui, "−", "Delete Row", can && selected_row.is_some(), st.theme.error, st)
                && let Some(row) = selected_row
            {
                cmd.push(("db.deleteRows", json!({ "result": r.id, "rows": [row] })));
            }
            if small_button(ui, "↶", "Revert All Changes", pending, st.theme.text, st) {
                cmd.push(("db.revertChanges", id.clone()));
            }
            if small_button(ui, "👁", "Preview Pending Changes (SQL)", pending && can, st.theme.text, st) {
                cmd.push(("db.previewChanges", id.clone()));
            }
            if small_button(ui, "⬆", "Submit Changes", pending && can, st.theme.success, st) {
                cmd.push(("db.submitChanges", id.clone()));
            }
            let read_only = s.db.source(&r.source).is_some_and(|d| d.config.read_only);
            if small_button(ui, "⤓", "Import Data from File…", !read_only, st.theme.text, st)
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Import Data")
                    .add_filter("CSV, TSV, JSON", &["csv", "tsv", "tab", "txt", "json"])
                    .pick_file()
            {
                cmd.push((
                    "db.importData",
                    json!({ "dataSource": r.source, "schema": t.schema, "table": t.table, "path": path }),
                ));
            }
            if r.edits.submitting {
                ui.label(RichText::new("Submitting…").color(st.theme.warning));
            } else if let Some(e) = &r.edits.error {
                ui.label(RichText::new("✖ Submit failed").color(st.theme.error)).on_hover_text(e);
            } else if !r.edits.is_empty() {
                let n = r.edits.pending_rows();
                ui.label(
                    RichText::new(format!("{n} pending row{}", if n == 1 { "" } else { "s" }))
                        .color(st.theme.vcs_modified),
                );
            } else if let Err(why) = &editable {
                ui.label(RichText::new("read-only").color(st.theme.text_muted)).on_hover_text(why);
            }
        }
        let sql: String = r.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        ui.add(egui::Label::new(RichText::new(sql).monospace().size(11.5).color(st.theme.text_muted)).truncate());
    });
    if !r.columns.is_empty() || !r.filter.is_empty() {
        filter_row(ui, st, r, cmd);
    }
    edit_preview(ui.ctx(), s, st, cmd);
    if st.db.value_viewer && !r.columns.is_empty() {
        egui::Panel::right(egui::Id::new(("db-value-viewer", r.id)))
            .resizable(true)
            .default_size(300.0)
            .size_range(160.0..=1200.0)
            .frame(Frame::new().inner_margin(Margin::same(6)))
            .show(ui, |ui| value_viewer(ui, st, r));
    }
    let status = r.status_line();
    // A transposed grid shows each column as a row (records as columns, read-only).
    if r.transposed {
        let key = (r.id, r.rows.len(), r.columns.len());
        if st.db.transposed.as_ref().is_none_or(|(k, _, _)| *k != key) {
            let (cols, rows) = transpose(&r.columns, &r.rows);
            st.db.transposed = Some((key, cols, rows));
        }
    }
    let transposed = st.db.transposed.as_ref().filter(|_| r.transposed).map(|(_, c, rows)| (c, rows));
    let data = if let Some((t_cols, t_rows)) = transposed {
        GridData {
            columns: t_cols,
            rows: t_rows,
            sort: None,
            status: &status,
            status_is_error: r.status == ResultStatus::Failed,
            edits: None,
            editable: false,
        }
    } else {
        GridData {
            columns: &r.columns,
            rows: &r.rows,
            sort: r.sort.map(|k| (k.column, k.desc)),
            status: &status,
            status_is_error: r.status == ResultStatus::Failed,
            edits: r.table.as_ref().map(|_| &r.edits),
            editable: s.db.editable(r.id).is_ok(),
        }
    };
    let grid_id = egui::Id::new(("db-grid", r.id, r.transposed));
    let grid = st.db.grids.entry(r.id).or_default();
    let actions = data_grid::show(ui, grid_id, grid, &data, &st.theme);
    for a in actions {
        match a {
            GridAction::Sort(c) if !r.is_running() && !r.transposed => {
                cmd.push(("db.sortResult", json!({ "result": r.id, "column": c })))
            }
            GridAction::Sort(_) => {}
            GridAction::NearEnd => {
                let marker = (r.id, r.rows.len());
                if r.has_more && st.db.fetch_requested != Some(marker) && !r.transposed {
                    st.db.fetch_requested = Some(marker);
                    cmd.push(("db.fetchMore", id.clone()));
                }
            }
            GridAction::Copy(text) => {
                ui.ctx().copy_text(text);
                s.status = "Copied".into();
            }
            GridAction::Edit { row, col, value } => {
                cmd.push(("db.editCell", json!({ "result": r.id, "row": row, "column": col, "value": value })));
            }
            GridAction::AddRow => cmd.push(("db.addRow", id.clone())),
            GridAction::DeleteRow(row) => cmd.push(("db.deleteRows", json!({ "result": r.id, "rows": [row] }))),
            GridAction::RevertRow(row) => cmd.push(("db.revertChanges", json!({ "result": r.id, "rows": [row] }))),
            GridAction::CopyAs { format, rect } => {
                // In a transposed grid rows are columns and columns (after the first) are records.
                let (rows, cols) = if r.transposed {
                    if rect.cols.1 == 0 {
                        continue;
                    }
                    ((rect.cols.0.max(1) - 1, rect.cols.1 - 1), rect.rows)
                } else {
                    (rect.rows, rect.cols)
                };
                let last_row = r.rows.len().saturating_sub(1);
                let p = json!({
                    "result": r.id,
                    "format": format,
                    "rowRange": [rows.0.min(last_row), rows.1.min(last_row)],
                    "columnRange": [cols.0, cols.1.min(r.columns.len().saturating_sub(1))],
                });
                cmd.push(("db.copyResult", p));
            }
            GridAction::ShowValue => st.db.value_viewer = true,
        }
    }
}

/// Columns become rows: the first column names the original column, then one per record.
fn transpose(columns: &[Column], rows: &[Vec<Cell>]) -> (Vec<Column>, Vec<Vec<Cell>>) {
    let records = rows.len().min(MAX_TRANSPOSED);
    let mut cols = vec![Column { name: "column".into(), type_name: String::new() }];
    cols.extend((1..=records).map(|i| Column { name: format!("#{i}"), type_name: String::new() }));
    let out = columns
        .iter()
        .enumerate()
        .map(|(c, col)| {
            let mut line = Vec::with_capacity(records + 1);
            line.push(Cell::Text(col.name.clone()));
            line.extend(rows.iter().take(records).map(|row| row.get(c).cloned().unwrap_or(Cell::Null)));
            line
        })
        .collect();
    (cols, out)
}

/// WHERE and ORDER BY typed above the grid; Enter re-queries.
fn filter_row(ui: &mut egui::Ui, st: &mut UiState, r: &ResultSet, cmd: &mut Vec<(&'static str, Value)>) {
    let edit = st.db.filters.entry(r.id).or_default();
    if edit.applied != r.filter {
        *edit = FilterEdit {
            where_: r.filter.where_.clone(),
            order_by: r.filter.order_by.clone(),
            applied: r.filter.clone(),
        };
    }
    let theme = &st.theme;
    let mut apply = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("WHERE").monospace().size(11.5).color(theme.keyword));
        let w = ui.add(
            TextEdit::singleline(&mut edit.where_)
                .id(egui::Id::new(("db-filter-where", r.id)))
                .font(egui::TextStyle::Monospace)
                .hint_text("condition")
                .desired_width((ui.available_width() * 0.55).max(120.0)),
        );
        ui.label(RichText::new("ORDER BY").monospace().size(11.5).color(theme.keyword));
        let o = ui.add(
            TextEdit::singleline(&mut edit.order_by)
                .id(egui::Id::new(("db-filter-order", r.id)))
                .font(egui::TextStyle::Monospace)
                .hint_text("columns")
                .desired_width((ui.available_width() - 30.0).max(80.0)),
        );
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        apply = (w.lost_focus() || o.lost_focus()) && enter;
        let dirty = edit.where_ != r.filter.where_ || edit.order_by != r.filter.order_by;
        if ui
            .add_enabled(dirty && !r.is_running(), egui::Button::new(RichText::new("↵").color(theme.text)).frame(false))
            .on_hover_text("Apply the filter (Enter)")
            .clicked()
        {
            apply = true;
        }
    });
    if apply && !r.is_running() {
        cmd.push(("db.filterResult", json!({ "result": r.id, "where": edit.where_, "orderBy": edit.order_by })));
    }
}

/// The selected cell, mapped back from a transposed grid.
fn selected_cell(st: &UiState, r: &ResultSet) -> Option<(usize, usize)> {
    let grid = st.db.grids.get(&r.id)?;
    let (row, col) = match grid.selection? {
        data_grid::Selection::Cell { row, col } => (row, col),
        data_grid::Selection::Row(row) => (row, 0),
    };
    if r.transposed { Some((col.checked_sub(1)?, row)) } else { Some((row, col)) }
}

/// The value viewer: the selected cell as text, pretty JSON, a hex dump or an image.
fn value_viewer(ui: &mut egui::Ui, st: &mut UiState, r: &ResultSet) {
    let theme = st.theme.clone();
    let Some((row, col)) = selected_cell(st, r) else {
        ui.label(RichText::new("Select a cell to see its value").color(theme.text_muted));
        return;
    };
    let (Some(cell), Some(column)) = (r.rows.get(row).and_then(|x| x.get(col)), r.columns.get(col)) else {
        ui.label(RichText::new("No value").color(theme.text_muted));
        return;
    };
    let view = codecraft_engine::db::value::view(cell);
    let kind = match view.kind {
        ValueKind::Null => "NULL",
        ValueKind::Text => "text",
        ValueKind::Number => "number",
        ValueKind::Json => "JSON",
        ValueKind::Binary => "binary",
        ValueKind::Image => "image",
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(&column.name).strong().color(theme.text));
        ui.label(RichText::new(format!("{} · {kind} · {} bytes", column.type_name, view.len)).color(theme.text_muted));
    });
    if let (Some(info), Cell::Bytes(bytes)) = (&view.image, cell) {
        ui.label(
            RichText::new(format!("{} image, {}×{}", info.format.to_uppercase(), info.width, info.height))
                .color(theme.text_muted),
        );
        let key = (r.id, row, col);
        if st.db.viewer_image.as_ref().is_none_or(|(k, _)| *k != key) {
            let texture = decode_image(bytes).map(|img| {
                ui.ctx().load_texture(format!("db-value-{}-{row}-{col}", r.id), img, egui::TextureOptions::LINEAR)
            });
            st.db.viewer_image = Some((key, texture));
        }
        match st.db.viewer_image.as_ref().and_then(|(_, t)| t.as_ref()) {
            Some(t) => {
                let size = t.size_vec2();
                let scale = (ui.available_width() / size.x.max(1.0)).min(1.0);
                ui.image((t.id(), size * scale));
            }
            None => {
                ui.label(RichText::new("Preview is available for PNG images").color(theme.text_muted));
            }
        }
    }
    if view.kind == ValueKind::Null {
        ui.label(RichText::new("<null>").monospace().color(theme.text_disabled));
        return;
    }
    let mut text = view.text.clone();
    ScrollArea::both().id_salt(("db-value-text", r.id)).auto_shrink([false, false]).show(ui, |ui| {
        ui.add(
            TextEdit::multiline(&mut text)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .interactive(true)
                .code_editor(),
        );
    });
}

/// PNG bytes as an egui image (other formats show their size only).
fn decode_image(bytes: &[u8]) -> Option<egui::ColorImage> {
    if bytes.len() > 32 * 1024 * 1024 {
        return None;
    }
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    if img.width() > 8192 || img.height() > 8192 {
        return None;
    }
    let rgba = img.to_rgba8();
    let size = [usize::try_from(rgba.width()).ok()?, usize::try_from(rgba.height()).ok()?];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// An Explain Plan: steps as an indented tree with cost, rows and actual times.
fn plan_view(ui: &mut egui::Ui, st: &UiState, r: &ResultSet, plan: &Plan, cmd: &mut Vec<(&'static str, Value)>) {
    let theme = &st.theme;
    ui.horizontal(|ui| {
        if small_button(ui, "⎘", "Copy the plan", true, theme.text, st) {
            cmd.push(("db.copyResult", json!({ "result": r.id })));
        }
        let mut info = vec![if plan.analyzed { "Explain Analyze".to_string() } else { "Explain Plan".to_string() }];
        if let Some(ms) = plan.planning_ms {
            info.push(format!("planning {} ms", format_number(ms)));
        }
        if let Some(ms) = plan.execution_ms {
            info.push(format!("execution {} ms", format_number(ms)));
        }
        ui.label(RichText::new(info.join(" · ")).color(theme.text_muted));
        if let Some(n) = &plan.note {
            ui.label(RichText::new(n).color(theme.warning));
        }
        let sql: String = r.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        ui.add(egui::Label::new(RichText::new(sql).monospace().size(11.5).color(theme.text_muted)).truncate());
    });
    ui.add(egui::Separator::default().spacing(2.0));
    let mut lines: Vec<(usize, &PlanNode)> = Vec::new();
    fn walk<'a>(n: &'a PlanNode, depth: usize, out: &mut Vec<(usize, &'a PlanNode)>) {
        out.push((depth, n));
        if depth < 64 {
            for c in &n.children {
                walk(c, depth + 1, out);
            }
        }
    }
    for root in &plan.roots {
        walk(root, 0, &mut lines);
    }
    let num = |v: Option<f64>| v.map(format_number).unwrap_or_default();
    let cost = |n: &PlanNode| match (n.startup_cost, n.total_cost) {
        (Some(a), Some(b)) => format!("{}..{}", format_number(a), format_number(b)),
        (None, Some(b)) => format_number(b),
        _ => String::new(),
    };
    ScrollArea::both().id_salt(("db-plan", r.id)).auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new(("db-plan-grid", r.id)).striped(true).spacing([14.0, 4.0]).show(ui, |ui| {
            for h in ["Operation", "Relation", "Cost", "Rows"] {
                ui.label(RichText::new(h).strong());
            }
            if plan.analyzed {
                for h in ["Actual ms", "Actual rows", "Loops"] {
                    ui.label(RichText::new(h).strong());
                }
            }
            ui.label(RichText::new("Details").strong());
            ui.end_row();
            for (depth, n) in &lines {
                let indent = "   ".repeat(*depth);
                let arrow = if *depth == 0 { "" } else { "└ " };
                ui.label(RichText::new(format!("{indent}{arrow}{}", n.operation)).monospace().color(theme.keyword));
                ui.label(RichText::new(n.relation.clone().unwrap_or_default()).monospace().color(theme.text));
                ui.label(RichText::new(cost(n)).monospace().color(theme.number));
                ui.label(RichText::new(num(n.rows)).monospace().color(theme.number));
                if plan.analyzed {
                    ui.label(RichText::new(num(n.actual_time_ms)).monospace().color(theme.number));
                    ui.label(RichText::new(num(n.actual_rows)).monospace().color(theme.number));
                    ui.label(RichText::new(num(n.loops)).monospace().color(theme.number));
                }
                ui.add(egui::Label::new(RichText::new(&n.detail).size(12.0).color(theme.text_muted)).truncate())
                    .on_hover_text(&n.detail);
                ui.end_row();
            }
        });
    });
}

/// The SQL that Submit will run, with Submit/Cancel.
fn edit_preview(ctx: &egui::Context, s: &Session, st: &UiState, cmd: &mut Vec<(&'static str, Value)>) {
    let Some(p) = &s.db.edit_preview else { return };
    let mut open = true;
    egui::Window::new("Pending Changes")
        .id(egui::Id::new("db-edit-preview"))
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(RichText::new("These statements run in one transaction:").color(st.theme.text_muted));
            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for sql in &p.statements {
                    ui.label(RichText::new(format!("{sql};")).monospace().color(st.theme.text_editor));
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Submit").clicked() {
                    cmd.push(("db.submitChanges", json!({ "result": p.result })));
                }
                if ui.button("Cancel").clicked() {
                    cmd.push(("db.closePreview", Value::Null));
                }
            });
        });
    if !open {
        cmd.push(("db.closePreview", Value::Null));
    }
}
