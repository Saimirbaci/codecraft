//! DataGrip's schema tools in the window (track DA): the Create/Modify Table dialog (columns,
//! keys and indexes, foreign keys, checks, live SQL preview), the Drop/Rename/Truncate
//! confirmation, the SQL Generator, Compare Schemas (choose the target, diff tree, migration
//! script) and the Database tool window's context menu entries for them. The ER diagram is
//! [`crate::er_diagram`]. Text fields edit the engine's dialog state in place; actions are
//! `db.*` commands.

use egui::{Align, Align2, Color32, Layout, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::schema_tools::{SchemaOp, ToolState};
use codecraft_engine::db::DbKind;
use codecraft_engine::db::ddl::{
    CheckDef, ColumnDef, FkAction, ForeignKeyDef, IndexDef, ObjectKind, TableDef, type_suggestions,
};
use codecraft_engine::db::schema_diff::{Change, DiffItem};

use crate::theme::Theme;
use crate::{UiState, run_command};

pub(crate) type Cmds = Vec<(&'static str, Value)>;

/// Draw the schema tools' windows (and the ER diagram).
pub fn show(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let mut cmds: Cmds = Vec::new();
    table_editor(ctx, s, st, &mut cmds);
    prompt(ctx, s, st, &mut cmds);
    generated(ctx, s, st, &mut cmds);
    compare_chooser(ctx, s, st, &mut cmds);
    compare(ctx, s, st, &mut cmds);
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
    crate::er_diagram::show(ctx, s, st);
}

// ---------------------------------------------------------------- tree context menu

/// A node of the Database tool window the menu acts on.
pub(crate) struct NodeTarget<'a> {
    /// `source`, `schema`, `table`, `view`, `materializedView`, `column`, `index`, `foreignKey`.
    pub kind: &'a str,
    pub source: &'a str,
    pub schema: Option<&'a str>,
    pub table: Option<&'a str>,
    /// The node's own name (a column, index, foreign key, table or schema).
    pub name: &'a str,
}

/// The schema tools' entries of a tree node's context menu.
pub(crate) fn tree_menu(ui: &mut egui::Ui, t: &NodeTarget<'_>, out: &mut Cmds) {
    let mut item = |ui: &mut egui::Ui, label: &str, id: &'static str, p: Value| {
        if ui.button(label).clicked() {
            out.push((id, p));
            ui.close();
        }
    };
    let ds = t.source;
    match t.kind {
        "source" => {
            ui.separator();
            item(ui, "New Table…", "db.createTable", json!({ "dataSource": ds }));
            item(ui, "Show Visualization", "db.diagram.show", json!({ "dataSource": ds }));
            if ui.button("Compare With…").clicked() {
                open_chooser(ui.ctx(), ds, None);
                ui.close();
            }
        }
        "schema" => {
            let schema = t.schema.unwrap_or(t.name);
            ui.separator();
            item(ui, "New Table…", "db.createTable", json!({ "dataSource": ds, "schema": schema }));
            ui.menu_button("SQL Scripts", |ui| {
                let o = json!({ "dataSource": ds, "kind": "schema", "name": schema });
                item(ui, "SQL Generator…", "db.generateDdl", o.clone());
                item(ui, "Copy DDL", "db.copyDdl", o);
            });
            item(ui, "Show Visualization", "db.diagram.show", json!({ "dataSource": ds, "schema": schema }));
            if ui.button("Compare With…").clicked() {
                open_chooser(ui.ctx(), ds, Some(schema));
                ui.close();
            }
            ui.separator();
            let o = json!({ "dataSource": ds, "kind": "schema", "name": schema });
            item(ui, "Rename…", "db.renameObject", with(&o, "newName", schema));
            item(ui, "Drop…", "db.dropObject", o);
        }
        "table" | "view" | "materializedView" => {
            let schema = t.schema.unwrap_or_default();
            let o = json!({ "dataSource": ds, "kind": t.kind, "schema": schema, "name": t.name });
            ui.separator();
            if t.kind == "table" {
                item(ui, "New Table…", "db.createTable", json!({ "dataSource": ds, "schema": schema }));
                item(
                    ui,
                    "Modify Table…",
                    "db.modifyTable",
                    json!({ "dataSource": ds, "schema": schema, "table": t.name }),
                );
            }
            ui.menu_button("SQL Scripts", |ui| {
                item(ui, "SQL Generator…", "db.generateDdl", o.clone());
                item(ui, "Copy DDL", "db.copyDdl", o.clone());
            });
            if t.kind == "table" {
                item(
                    ui,
                    "Show Visualization",
                    "db.diagram.show",
                    json!({ "dataSource": ds, "schema": schema, "tables": [t.name] }),
                );
            }
            ui.separator();
            item(ui, "Rename…", "db.renameObject", with(&o, "newName", t.name));
            if t.kind == "table" {
                item(
                    ui,
                    "Truncate…",
                    "db.truncateTable",
                    json!({ "dataSource": ds, "schema": schema, "table": t.name }),
                );
            }
            item(ui, "Drop…", "db.dropObject", o);
        }
        "column" | "index" | "foreignKey" => {
            let o = json!({
                "dataSource": ds, "kind": t.kind, "schema": t.schema.unwrap_or_default(),
                "table": t.table.unwrap_or_default(), "name": t.name,
            });
            ui.separator();
            item(ui, "Copy DDL", "db.copyDdl", o.clone());
            if t.kind != "foreignKey" {
                item(ui, "Rename…", "db.renameObject", with(&o, "newName", t.name));
            }
            item(ui, "Drop…", "db.dropObject", o);
        }
        _ => {}
    }
}

fn with(v: &Value, key: &str, value: &str) -> Value {
    let mut v = v.clone();
    if let Some(o) = v.as_object_mut() {
        o.insert(key.into(), json!(value));
    }
    v
}

// ---------------------------------------------------------------- Create / Modify Table

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Columns,
    Keys,
    ForeignKeys,
    Checks,
}

fn mono(text: impl Into<String>, color: Color32) -> RichText {
    RichText::new(text).monospace().size(12.0).color(color)
}

fn sql_view(ui: &mut egui::Ui, theme: &Theme, sql: &str, height: f32) {
    ScrollArea::both().id_salt(ui.next_auto_id()).max_height(height).auto_shrink([false, true]).show(ui, |ui| {
        for line in sql.lines() {
            let color = if line.starts_with("--") { theme.comment } else { theme.text_editor };
            ui.label(mono(line, color));
        }
    });
}

/// A `a, b, c` text field over a list (the typed text survives while the list is unchanged).
fn list_field(ui: &mut egui::Ui, id: egui::Id, list: &mut Vec<String>, width: f32, hint: &str) {
    let joined = list.join(", ");
    let mut buf = ui
        .data(|d| d.get_temp::<(String, String)>(id))
        .filter(|(src, _)| *src == joined)
        .map(|(_, b)| b)
        .unwrap_or_else(|| joined.clone());
    if ui.add(TextEdit::singleline(&mut buf).id(id).desired_width(width).hint_text(hint)).changed() {
        *list = buf.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
        let src = list.join(", ");
        ui.data_mut(|d| d.insert_temp(id, (src, buf)));
    }
}

/// A menu of checkboxes choosing some of `options` (in their order).
fn columns_menu(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    chosen: &mut Vec<String>,
    options: &[String],
) {
    let label = if chosen.is_empty() { "choose…".to_string() } else { chosen.join(", ") };
    ui.push_id(id, |ui| {
        ui.menu_button(label, |ui| {
            for o in options {
                let mut on = chosen.contains(o);
                if ui.checkbox(&mut on, o).changed() {
                    if on {
                        chosen.push(o.clone());
                    } else {
                        chosen.retain(|c| c != o);
                    }
                }
            }
            if options.is_empty() {
                ui.label("No columns");
            }
        });
    });
}

fn fk_action(ui: &mut egui::Ui, id: impl std::hash::Hash + std::fmt::Debug, value: &mut FkAction) {
    egui::ComboBox::from_id_salt(id).width(110.0).selected_text(value.sql()).show_ui(ui, |ui| {
        for a in FkAction::ALL {
            ui.selectable_value(value, a, a.sql());
        }
    });
}

fn table_editor(ctx: &egui::Context, s: &mut Session, st: &mut UiState, cmds: &mut Cmds) {
    let theme = st.theme.clone();
    // Columns of other tables of the schema the tree has loaded (foreign key targets).
    let known: Vec<(String, Vec<String>, Vec<String>)> = match s.db.schema.table_editor.as_ref() {
        Some(e) => {
            s.db.source(&e.source)
                .and_then(|d| d.tree.schemas.get(&e.schema))
                .and_then(|n| n.tables.as_ref())
                .map(|ts| {
                    ts.iter()
                        .filter_map(|t| {
                            let det = t.details.as_ref()?;
                            Some((
                                t.info.name.clone(),
                                det.columns.iter().map(|c| c.name.clone()).collect(),
                                det.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        None => return,
    };
    let Some(e) = s.db.schema.table_editor.as_mut() else { return };
    let tab_id = egui::Id::new("db-table-editor-tab");
    let mut tab = ctx.data(|d| d.get_temp::<Tab>(tab_id)).unwrap_or(Tab::Columns);
    let mut open = true;
    let kind = e.kind;
    egui::Window::new(e.title())
        .id(egui::Id::new("db-table-editor"))
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(820.0, 620.0))
        .anchor(Align2::CENTER_CENTER, [0.0, -20.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(
                RichText::new(format!(
                    "{} · {}",
                    e.source_name,
                    if e.schema.is_empty() { "(default schema)" } else { &e.schema }
                ))
                .color(theme.text_muted),
            );
            if e.state == ToolState::Loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading the table…");
                });
                return;
            }
            egui::Grid::new("db-te-head").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Name:");
                ui.add(TextEdit::singleline(&mut e.table.name).id(egui::Id::new("db-te-name")).desired_width(320.0));
                ui.end_row();
                ui.label("Comment:");
                ui.add_enabled(kind != DbKind::Sqlite, TextEdit::singleline(&mut e.table.comment).desired_width(480.0));
                ui.end_row();
            });
            ui.add_space(4.0);
            let t = &mut e.table;
            ui.horizontal(|ui| {
                for (which, label) in [
                    (Tab::Columns, format!("Columns ({})", t.columns.len())),
                    (
                        Tab::Keys,
                        format!("Keys and Indexes ({})", t.indexes.len() + usize::from(!t.primary_key.is_empty())),
                    ),
                    (Tab::ForeignKeys, format!("Foreign Keys ({})", t.foreign_keys.len())),
                    (Tab::Checks, format!("Checks ({})", t.checks.len())),
                ] {
                    if ui.selectable_label(tab == which, label).clicked() {
                        tab = which;
                    }
                }
            });
            ui.separator();
            let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            ScrollArea::both().id_salt("db-te-body").max_height(250.0).auto_shrink([false, true]).show(ui, |ui| {
                match tab {
                    Tab::Columns => columns_tab(ui, &theme, kind, t),
                    Tab::Keys => keys_tab(ui, &theme, t, &cols),
                    Tab::ForeignKeys => fks_tab(ui, &theme, t, &cols, &known),
                    Tab::Checks => checks_tab(ui, &theme, t),
                }
            });
            ui.separator();
            ui.label(RichText::new("SQL preview").strong());
            let preview = e.preview();
            sql_view(ui, &theme, &preview, 170.0);
            match &e.state {
                ToolState::Confirm => {
                    ui.label(
                        RichText::new(format!(
                            "{} is a production data source. Execute these statements?",
                            e.source_name
                        ))
                        .color(theme.warning),
                    );
                }
                ToolState::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Executing…");
                    });
                }
                ToolState::Failed(err) => {
                    ScrollArea::vertical().id_salt("db-te-err").max_height(90.0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(err).color(theme.error)).wrap());
                    });
                }
                _ => {}
            }
            let valid = e.script().is_ok_and(|s| !s.is_empty());
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    cmds.push(("db.tableEditor.close", Value::Null));
                }
                if ui.add_enabled(valid, egui::Button::new("Open in Console")).clicked() {
                    cmds.push(("db.tableEditor.openInConsole", Value::Null));
                }
                let confirm = e.state == ToolState::Confirm;
                let label = if confirm { "Execute Anyway" } else { "Execute" };
                let fill = if confirm { theme.error } else { theme.accent };
                let busy = e.state.is_busy();
                if ui.add_enabled(valid && !busy, egui::Button::new(label).fill(fill)).clicked() {
                    cmds.push(("db.tableEditor.execute", json!({ "confirm": confirm })));
                }
            });
        });
    ctx.data_mut(|d| d.insert_temp(tab_id, tab));
    if !open {
        cmds.push(("db.tableEditor.close", Value::Null));
    }
}

fn columns_tab(ui: &mut egui::Ui, theme: &Theme, kind: DbKind, t: &mut TableDef) {
    let mut remove = None;
    let mut swap = None;
    let n = t.columns.len();
    egui::Grid::new("db-te-cols").num_columns(9).striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
        for h in ["Key", "Name", "Type", "Not null", "Auto inc.", "Default", "Comment", "", ""] {
            ui.label(RichText::new(h).color(theme.text_muted).size(11.5));
        }
        ui.end_row();
        for (i, c) in t.columns.iter_mut().enumerate() {
            let mut pk = t.primary_key.contains(&c.name);
            if ui.checkbox(&mut pk, "").on_hover_text("Primary key").changed() {
                if pk {
                    t.primary_key.push(c.name.clone());
                } else {
                    t.primary_key.retain(|k| k != &c.name);
                }
            }
            let old = c.name.clone();
            if ui
                .add(TextEdit::singleline(&mut c.name).id(egui::Id::new(("db-te-col", i))).desired_width(130.0))
                .changed()
            {
                // Keys follow the rename.
                for k in t.primary_key.iter_mut().filter(|k| **k == old) {
                    *k = c.name.clone();
                }
                for idx in &mut t.indexes {
                    for k in idx.columns.iter_mut().filter(|k| **k == old) {
                        *k = c.name.clone();
                    }
                }
                for f in &mut t.foreign_keys {
                    for k in f.columns.iter_mut().filter(|k| **k == old) {
                        *k = c.name.clone();
                    }
                }
            }
            ui.horizontal(|ui| {
                ui.add(
                    TextEdit::singleline(&mut c.data_type).id(egui::Id::new(("db-te-type", i))).desired_width(120.0),
                );
                ui.menu_button("▾", |ui| {
                    for ty in type_suggestions(kind) {
                        if ui.button(*ty).clicked() {
                            c.data_type = (*ty).to_string();
                            ui.close();
                        }
                    }
                });
            });
            let mut not_null = !c.nullable;
            if ui.checkbox(&mut not_null, "").changed() {
                c.nullable = !not_null;
            }
            ui.checkbox(&mut c.auto_increment, "");
            ui.add(TextEdit::singleline(&mut c.default).desired_width(110.0).hint_text("none"));
            ui.add_enabled(kind != DbKind::Sqlite, TextEdit::singleline(&mut c.comment).desired_width(120.0));
            ui.horizontal(|ui| {
                if ui.add_enabled(i > 0, egui::Button::new("↑").small()).clicked() {
                    swap = Some((i - 1, i));
                }
                if ui.add_enabled(i + 1 < n, egui::Button::new("↓").small()).clicked() {
                    swap = Some((i, i + 1));
                }
            });
            if ui.button("✖").on_hover_text("Remove the column").clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some((a, b)) = swap
        && a < t.columns.len()
        && b < t.columns.len()
    {
        t.columns.swap(a, b);
    }
    if let Some(i) = remove.filter(|i| *i < t.columns.len()) {
        let name = t.columns.remove(i).name;
        t.primary_key.retain(|k| *k != name);
    }
    if ui.button("+ Column").clicked() {
        let mut k = t.columns.len() + 1;
        while t.column(&format!("column_{k}")).is_some() {
            k += 1;
        }
        let ty = type_suggestions(kind).first().copied().unwrap_or("text");
        t.columns.push(ColumnDef { name: format!("column_{k}"), data_type: ty.to_string(), ..Default::default() });
    }
}

fn keys_tab(ui: &mut egui::Ui, theme: &Theme, t: &mut TableDef, cols: &[String]) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Primary key:").strong());
        columns_menu(ui, "db-te-pk", &mut t.primary_key, cols);
        ui.label(RichText::new("name").color(theme.text_muted));
        ui.add(TextEdit::singleline(&mut t.primary_key_name).desired_width(160.0).hint_text("default"));
    });
    ui.separator();
    let mut remove = None;
    egui::Grid::new("db-te-idx").num_columns(4).striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
        for h in ["Index", "Columns", "Unique", ""] {
            ui.label(RichText::new(h).color(theme.text_muted).size(11.5));
        }
        ui.end_row();
        for (i, idx) in t.indexes.iter_mut().enumerate() {
            ui.add(TextEdit::singleline(&mut idx.name).desired_width(180.0).hint_text("generated"));
            if idx.columns.is_empty() && !idx.definition.is_empty() {
                ui.label(mono(idx.definition.clone(), theme.text_muted))
                    .on_hover_text("An expression or partial index");
            } else {
                columns_menu(ui, ("db-te-idx-cols", i), &mut idx.columns, cols);
            }
            let unique = idx.unique;
            ui.checkbox(&mut idx.unique, "");
            if unique != idx.unique {
                idx.constraint = false;
            }
            if ui.button("✖").clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove.filter(|i| *i < t.indexes.len()) {
        t.indexes.remove(i);
    }
    if ui.button("+ Index").clicked() {
        t.indexes.push(IndexDef::default());
    }
}

fn fks_tab(
    ui: &mut egui::Ui,
    theme: &Theme,
    t: &mut TableDef,
    cols: &[String],
    known: &[(String, Vec<String>, Vec<String>)],
) {
    let mut remove = None;
    let own_name = t.name.clone();
    egui::Grid::new("db-te-fks").num_columns(7).striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
        for h in ["Name", "Columns", "References", "Columns", "On delete", "On update", ""] {
            ui.label(RichText::new(h).color(theme.text_muted).size(11.5));
        }
        ui.end_row();
        for (i, f) in t.foreign_keys.iter_mut().enumerate() {
            ui.add(TextEdit::singleline(&mut f.name).desired_width(140.0).hint_text("generated"));
            columns_menu(ui, ("db-te-fk-cols", i), &mut f.columns, cols);
            ui.horizontal(|ui| {
                ui.add(TextEdit::singleline(&mut f.ref_table).desired_width(120.0).hint_text("table"));
                ui.menu_button("▾", |ui| {
                    for (name, _, pk) in known {
                        if ui.button(name).clicked() {
                            f.ref_table = name.clone();
                            if f.ref_columns.is_empty() {
                                f.ref_columns = pk.clone();
                            }
                            ui.close();
                        }
                    }
                    if known.is_empty() {
                        ui.label("Expand tables in the tree to list them");
                    }
                });
            });
            let ref_cols: Vec<String> = if f.ref_table == own_name {
                cols.to_vec()
            } else {
                known.iter().find(|(n, _, _)| *n == f.ref_table).map(|(_, c, _)| c.clone()).unwrap_or_default()
            };
            if ref_cols.is_empty() {
                list_field(ui, egui::Id::new(("db-te-fk-ref", i)), &mut f.ref_columns, 120.0, "id");
            } else {
                columns_menu(ui, ("db-te-fk-ref-menu", i), &mut f.ref_columns, &ref_cols);
            }
            fk_action(ui, ("db-te-fk-del", i), &mut f.on_delete);
            fk_action(ui, ("db-te-fk-upd", i), &mut f.on_update);
            if ui.button("✖").clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove.filter(|i| *i < t.foreign_keys.len()) {
        t.foreign_keys.remove(i);
    }
    if ui.button("+ Foreign Key").clicked() {
        t.foreign_keys.push(ForeignKeyDef::default());
    }
}

fn checks_tab(ui: &mut egui::Ui, theme: &Theme, t: &mut TableDef) {
    let mut remove = None;
    egui::Grid::new("db-te-checks").num_columns(3).striped(true).spacing([8.0, 4.0]).show(ui, |ui| {
        for h in ["Name", "Condition", ""] {
            ui.label(RichText::new(h).color(theme.text_muted).size(11.5));
        }
        ui.end_row();
        for (i, c) in t.checks.iter_mut().enumerate() {
            ui.add(TextEdit::singleline(&mut c.name).desired_width(160.0).hint_text("unnamed"));
            ui.add(TextEdit::singleline(&mut c.expression).desired_width(380.0).hint_text("price > 0"));
            if ui.button("✖").clicked() {
                remove = Some(i);
            }
            ui.end_row();
        }
    });
    if let Some(i) = remove.filter(|i| *i < t.checks.len()) {
        t.checks.remove(i);
    }
    if ui.button("+ Check").clicked() {
        t.checks.push(CheckDef::default());
    }
}

// ---------------------------------------------------------------- Drop / Rename / Truncate

fn prompt(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(p) = s.db.schema.prompt.as_mut() else { return };
    let theme = &st.theme;
    let mut open = true;
    egui::Window::new(p.title())
        .id(egui::Id::new("db-schema-op"))
        .collapsible(false)
        .resizable(true)
        .default_width(560.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .open(&mut open)
        .show(ctx, |ui| {
            if p.production {
                ui.label(RichText::new(format!("{} is a production data source.", p.source_name)).color(theme.warning));
            }
            let postgres = p.kind == DbKind::Postgres;
            let column = p.object.kind == ObjectKind::Column;
            let mut submit = false;
            match &mut p.op {
                SchemaOp::Rename { new_name, update_sql_files } => {
                    ui.horizontal(|ui| {
                        ui.label("New name:");
                        let r =
                            ui.add(TextEdit::singleline(new_name).id(egui::Id::new("db-op-name")).desired_width(300.0));
                        if !ui.memory(|m| m.focused().is_some()) {
                            r.request_focus();
                        }
                        submit = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    });
                    if column {
                        ui.checkbox(update_sql_files, "Also rename it in the project's SQL files (preview first)");
                    }
                }
                SchemaOp::Drop { cascade } => {
                    if postgres {
                        ui.checkbox(cascade, "Cascade: drop the objects that depend on it too");
                    }
                }
                SchemaOp::Truncate { cascade } => {
                    if postgres {
                        ui.checkbox(cascade, "Cascade: truncate the tables that reference it too");
                    }
                }
            }
            ui.add_space(4.0);
            let sql = p.preview();
            sql_view(ui, theme, &sql, 200.0);
            match &p.state {
                ToolState::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Executing…");
                    });
                }
                ToolState::Failed(e) => {
                    ui.add(egui::Label::new(RichText::new(e).color(theme.error)).wrap());
                }
                _ => {}
            }
            let valid = p.script().is_ok();
            let busy = p.state.is_busy();
            let destructive = !matches!(p.op, SchemaOp::Rename { .. });
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                    cmds.push(("db.schemaOp.cancel", Value::Null));
                }
                let label = match p.op {
                    SchemaOp::Drop { .. } => "Drop",
                    SchemaOp::Rename { .. } => "Rename",
                    SchemaOp::Truncate { .. } => "Truncate",
                };
                let fill = if destructive || p.production { theme.error } else { theme.accent };
                if ui.add_enabled(valid && !busy, egui::Button::new(label).fill(fill)).clicked()
                    || (submit && valid && !busy)
                {
                    cmds.push(("db.schemaOp.confirm", Value::Null));
                }
            });
        });
    if !open {
        cmds.push(("db.schemaOp.cancel", Value::Null));
    }
}

// ---------------------------------------------------------------- SQL Generator

fn generated(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(g) = s.db.schema.generated.as_ref().filter(|g| g.visible) else { return };
    let theme = &st.theme;
    let mut open = true;
    egui::Window::new(format!("SQL Generator: {}", g.title))
        .id(egui::Id::new("db-sql-generator"))
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(640.0, 480.0))
        .open(&mut open)
        .show(ctx, |ui| {
            match &g.state {
                ToolState::Loading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading the catalog…");
                    });
                }
                ToolState::Failed(e) => {
                    ui.add(egui::Label::new(RichText::new(e).color(theme.error)).wrap());
                }
                _ => {
                    let h = (ui.available_height() - 40.0).max(120.0);
                    ScrollArea::both().id_salt("db-gen-sql").max_height(h).auto_shrink([false, false]).show(ui, |ui| {
                        let mut text = g.sql.as_str();
                        ui.add(
                            TextEdit::multiline(&mut text)
                                .font(egui::TextStyle::Monospace)
                                .text_color(theme.text_editor)
                                .desired_width(f32::INFINITY),
                        );
                    });
                }
            }
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    cmds.push(("db.generateDdl.close", Value::Null));
                }
                let ready = g.state == ToolState::Ready;
                if ui.add_enabled(ready, egui::Button::new("Open in Console")).clicked() {
                    cmds.push(("db.generateDdl.openInConsole", Value::Null));
                }
                if ui.add_enabled(ready, egui::Button::new("Copy")).clicked() {
                    ui.ctx().copy_text(g.sql.clone());
                }
            });
        });
    if !open {
        cmds.push(("db.generateDdl.close", Value::Null));
    }
}

// ---------------------------------------------------------------- Compare

/// The Compare With… form: the source is fixed, the target is chosen.
#[derive(Clone, Debug, Default)]
struct Chooser {
    source: String,
    schema: String,
    target: String,
    target_schema: String,
}

fn chooser_id() -> egui::Id {
    egui::Id::new("db-compare-chooser")
}

fn open_chooser(ctx: &egui::Context, source: &str, schema: Option<&str>) {
    let c = Chooser {
        source: source.to_string(),
        schema: schema.unwrap_or_default().to_string(),
        target: String::new(),
        target_schema: String::new(),
    };
    ctx.data_mut(|d| d.insert_temp(chooser_id(), c));
}

fn compare_chooser(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(mut c) = ctx.data(|d| d.get_temp::<Chooser>(chooser_id())) else { return };
    let theme = &st.theme;
    if c.schema.is_empty() {
        c.schema = s.db.schema_default_schema(&c.source);
    }
    let mut open = true;
    let mut close = false;
    let name = |id: &str| s.db.source(id).map(|d| d.config.name.clone()).unwrap_or_else(|| id.to_string());
    egui::Window::new("Compare Schemas")
        .id(egui::Id::new("db-compare-chooser-window"))
        .collapsible(false)
        .resizable(false)
        .default_width(480.0)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(RichText::new("Make the target like the source:").color(theme.text_muted));
            egui::Grid::new("db-cmp-form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Source:");
                ui.label(RichText::new(format!("{} / {}", name(&c.source), c.schema)).strong());
                ui.end_row();
                ui.label("Target data source:");
                let selected = if c.target.is_empty() { "choose…".to_string() } else { name(&c.target) };
                egui::ComboBox::from_id_salt("db-cmp-target").width(260.0).selected_text(selected).show_ui(ui, |ui| {
                    for ds in &s.db.sources {
                        if ui.selectable_label(c.target == ds.config.id, &ds.config.name).clicked() {
                            c.target = ds.config.id.clone();
                            c.target_schema = s.db.schema_default_schema(&ds.config.id);
                        }
                    }
                });
                ui.end_row();
                ui.label("Target schema:");
                ui.horizontal(|ui| {
                    ui.add(TextEdit::singleline(&mut c.target_schema).desired_width(200.0));
                    let schemas: Vec<String> =
                        s.db.source(&c.target)
                            .and_then(|d| d.tree.info.as_ref())
                            .map(|i| i.schemas.clone())
                            .unwrap_or_default();
                    if !schemas.is_empty() {
                        ui.menu_button("▾", |ui| {
                            for sc in &schemas {
                                if ui.button(sc).clicked() {
                                    c.target_schema = sc.clone();
                                    ui.close();
                                }
                            }
                        });
                    }
                });
                ui.end_row();
            });
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                let ok = !c.target.is_empty() && !(c.target == c.source && c.target_schema == c.schema);
                if ui.add_enabled(ok, egui::Button::new("Compare").fill(theme.accent)).clicked() {
                    cmds.push((
                        "db.compareSchemas",
                        json!({
                            "source": { "dataSource": c.source, "schema": c.schema },
                            "target": { "dataSource": c.target, "schema": c.target_schema },
                        }),
                    ));
                    close = true;
                }
            });
        });
    if close || !open {
        ctx.data_mut(|d| d.remove::<Chooser>(chooser_id()));
    } else {
        ctx.data_mut(|d| d.insert_temp(chooser_id(), c));
    }
}

fn change_color(theme: &Theme, c: Change) -> Color32 {
    match c {
        Change::Added => theme.success,
        Change::Removed => theme.error,
        Change::Changed => theme.warning,
    }
}

fn diff_item(ui: &mut egui::Ui, theme: &Theme, item: &DiffItem, depth: usize) {
    let text = |i: &DiffItem| {
        let mut t = format!("{} {} {}", i.change.symbol(), i.object, i.name);
        if !i.detail.is_empty() {
            t.push_str(&format!("  ({})", i.detail));
        }
        t
    };
    if item.children.is_empty() {
        ui.label(RichText::new(text(item)).color(change_color(theme, item.change)));
        return;
    }
    egui::CollapsingHeader::new(RichText::new(text(item)).color(change_color(theme, item.change)))
        .id_salt(("db-diff", depth, &item.object, &item.name))
        .default_open(true)
        .show(ui, |ui| {
            for c in &item.children {
                diff_item(ui, theme, c, depth + 1);
            }
        });
}

fn compare(ctx: &egui::Context, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let Some(c) = s.db.schema.compare.as_ref() else { return };
    let theme = &st.theme;
    let mut open = true;
    egui::Window::new("Compare Schemas")
        .id(egui::Id::new("db-compare"))
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(860.0, 600.0))
        .open(&mut open)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&c.source.label).strong());
                ui.label(RichText::new("→").color(theme.text_muted));
                ui.label(RichText::new(&c.target.label).strong());
            });
            if c.state == ToolState::Loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Reading both schemas…");
                });
            }
            for n in &c.diff.notes {
                ui.label(RichText::new(n).color(theme.text_muted).italics());
            }
            let (a, r, ch) = c.diff.counts();
            if c.state != ToolState::Loading {
                ui.label(
                    RichText::new(format!("{a} only in the source · {r} only in the target · {ch} changed"))
                        .color(theme.text_muted),
                );
            }
            ui.separator();
            let h = ((ui.available_height() - 80.0) / 2.0).max(120.0);
            ui.label(RichText::new("Differences").strong());
            ScrollArea::vertical().id_salt("db-cmp-tree").max_height(h).auto_shrink([false, true]).show(ui, |ui| {
                if c.diff.is_empty() && c.state != ToolState::Loading {
                    ui.label(RichText::new("The schemas are the same").color(theme.success));
                }
                for item in &c.diff.items {
                    diff_item(ui, theme, item, 0);
                }
            });
            ui.separator();
            ui.label(RichText::new(format!("Migration ({})", c.target_kind.label())).strong());
            sql_view(ui, theme, &c.sql, h);
            match &c.state {
                ToolState::Confirm => {
                    ui.label(
                        RichText::new("The target is a production data source. Apply the migration?")
                            .color(theme.warning),
                    );
                }
                ToolState::Running => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Applying…");
                    });
                }
                ToolState::Done(m) => {
                    ui.label(RichText::new(format!("✔ {m}")).color(theme.success));
                }
                ToolState::Failed(e) => {
                    ScrollArea::vertical().id_salt("db-cmp-err").max_height(90.0).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(e).color(theme.error)).wrap());
                    });
                }
                _ => {}
            }
            let has_script = c.script.as_ref().is_some_and(|s| !s.is_empty());
            let busy = c.state.is_busy();
            ui.allocate_ui_with_layout(vec2(ui.available_width(), 28.0), Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Close").clicked() {
                    cmds.push(("db.compare.close", Value::Null));
                }
                let confirm = c.state == ToolState::Confirm;
                let label = if confirm { "Apply Anyway" } else { "Apply" };
                if ui
                    .add_enabled(has_script && !busy, egui::Button::new(label).fill(theme.error))
                    .on_hover_text("Run the migration on the target")
                    .clicked()
                {
                    cmds.push(("db.compare.apply", json!({ "confirm": confirm })));
                }
                if ui.add_enabled(has_script && !busy, egui::Button::new("Open in Console")).clicked() {
                    cmds.push(("db.compare.openScript", Value::Null));
                }
                if ui.add_enabled(!busy, egui::Button::new("Compare Again")).clicked() {
                    cmds.push(("db.compare.refresh", Value::Null));
                }
            });
        });
    if !open {
        cmds.push(("db.compare.close", Value::Null));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_adds_a_key() {
        assert_eq!(with(&json!({ "a": 1 }), "b", "x"), json!({ "a": 1, "b": "x" }));
    }
}
