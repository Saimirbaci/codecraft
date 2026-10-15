//! Settings › Languages & Frameworks › SQL (track DX): the SQL dialect of files and folders,
//! the default data source of files, and SQL in Rust strings (its data source, dialect and
//! code style). Project settings, applied at once through `sql.fileSettings`,
//! `sql.chooseDataSource` and `sql.settings` (not part of the dialog's Apply/Cancel draft).

use egui::{RichText, TextEdit};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::sql_settings::{DIALECTS, label};

use crate::UiState;

type Cmds = Vec<(&'static str, Value)>;

/// What is typed into the page's "add" rows (kept in egui's temporary memory).
#[derive(Clone, Default)]
struct Draft {
    dialect_path: String,
    dialect: String,
    source_path: String,
    source: String,
}

fn dialect_combo(ui: &mut egui::Ui, salt: impl std::hash::Hash + std::fmt::Debug, value: &mut String) {
    let shown = if value.is_empty() { "<choose>".to_string() } else { label(value).to_string() };
    egui::ComboBox::from_id_salt(salt).selected_text(shown).width(170.0).show_ui(ui, |ui| {
        for (id, l) in DIALECTS {
            ui.selectable_value(value, id.to_string(), *l);
        }
    });
}

fn source_combo(ui: &mut egui::Ui, s: &Session, salt: impl std::hash::Hash + std::fmt::Debug, value: &mut String) {
    let shown = s.db.source(value).map(|d| d.config.name.clone()).unwrap_or_else(|| "<choose>".into());
    egui::ComboBox::from_id_salt(salt).selected_text(shown).width(170.0).show_ui(ui, |ui| {
        for ds in &s.db.sources {
            ui.selectable_value(value, ds.config.id.clone(), &ds.config.name);
        }
    });
}

fn shown_path(p: &str) -> String {
    if p.is_empty() { "<whole project>".into() } else { p.to_string() }
}

/// The page.
pub(crate) fn page(ui: &mut egui::Ui, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let theme = &st.theme;
    if s.db.project_root().is_none() {
        ui.label(RichText::new("Open a project to set its SQL dialects and data sources.").color(theme.text_muted));
        return;
    }
    ui.label(
        RichText::new("Project settings: they apply at once (saved in .codecraft/sql-files.json and sql.json).")
            .color(theme.text_muted),
    );
    // Read the project's SQL settings once per project (sql.settings loads them).
    let root = s.db.project_root().map(|r| r.display().to_string()).unwrap_or_default();
    let loaded_id = egui::Id::new("sql-settings-loaded");
    if ui.data_mut(|d| d.get_temp::<String>(loaded_id)).as_deref() != Some(root.as_str()) {
        ui.data_mut(|d| d.insert_temp(loaded_id, root));
        cmds.push(("sql.settings", Value::Null));
    }
    let id = egui::Id::new("sql-settings-draft");
    let mut draft: Draft = ui.data_mut(|d| d.get_temp::<Draft>(id).unwrap_or_default());

    // ------------------------------------------------ dialects
    ui.add_space(6.0);
    ui.label(RichText::new("SQL Dialects").strong());
    ui.label(
        RichText::new(
            "How statements of a file are split and parameters found; a file's entry wins over its folder's.",
        )
        .color(theme.text_muted)
        .size(11.0),
    );
    let dialects = s.db.sql_files.dialects.clone();
    egui::Grid::new("sql-dialects").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
        for (path, d) in &dialects {
            ui.label(shown_path(path));
            let mut v = d.clone();
            dialect_combo(ui, ("sql-dialect", path), &mut v);
            if &v != d {
                cmds.push(("sql.fileSettings", json!({ "dialects": { path.as_str(): v } })));
            }
            if ui.small_button("✕").on_hover_text("Use the data source's dialect").clicked() {
                cmds.push(("sql.fileSettings", json!({ "dialects": { path.as_str(): null } })));
            }
            ui.end_row();
        }
        ui.add(
            TextEdit::singleline(&mut draft.dialect_path)
                .hint_text("folder or file (empty: project)")
                .desired_width(220.0),
        );
        dialect_combo(ui, "sql-dialect-new", &mut draft.dialect);
        if ui.add_enabled(!draft.dialect.is_empty(), egui::Button::new("Add")).clicked() {
            cmds.push(("sql.fileSettings", json!({ "dialects": { draft.dialect_path.trim(): draft.dialect } })));
            draft.dialect_path.clear();
            draft.dialect.clear();
        }
        ui.end_row();
    });

    // ------------------------------------------------ data sources
    ui.add_space(10.0);
    ui.label(RichText::new("Default Data Sources").strong());
    ui.label(
        RichText::new(
            "The data source of .sql files that have none of their own (consoles keep theirs; Data Source for File… \
             sets one file's).",
        )
        .color(theme.text_muted)
        .size(11.0),
    );
    let sources = s.db.sql_files.data_sources.clone();
    egui::Grid::new("sql-sources").num_columns(3).spacing([12.0, 6.0]).show(ui, |ui| {
        for (path, d) in &sources {
            ui.label(shown_path(path));
            let mut v = d.clone();
            source_combo(ui, s, ("sql-source", path), &mut v);
            if &v != d {
                cmds.push(("sql.fileSettings", json!({ "dataSources": { path.as_str(): v } })));
            }
            if ui.small_button("✕").on_hover_text("Remove").clicked() {
                cmds.push(("sql.fileSettings", json!({ "dataSources": { path.as_str(): null } })));
            }
            ui.end_row();
        }
        ui.add(
            TextEdit::singleline(&mut draft.source_path)
                .hint_text("folder or file (empty: project)")
                .desired_width(220.0),
        );
        source_combo(ui, s, "sql-source-new", &mut draft.source);
        if ui.add_enabled(!draft.source.is_empty(), egui::Button::new("Add")).clicked() {
            cmds.push(("sql.fileSettings", json!({ "dataSources": { draft.source_path.trim(): draft.source } })));
            draft.source_path.clear();
            draft.source.clear();
        }
        ui.end_row();
    });
    ui.data_mut(|d| d.insert_temp(id, draft));

    // ------------------------------------------------ SQL in Rust strings
    ui.add_space(10.0);
    ui.label(RichText::new("SQL in Rust Strings").strong());
    let set = s.sql_lang.settings.clone();
    egui::Grid::new("sql-injected").num_columns(2).spacing([18.0, 6.0]).show(ui, |ui| {
        ui.label("Data source");
        let mut v = set.data_source.clone().unwrap_or_default();
        let shown = s.db.source(&v).map(|d| d.config.name.clone()).unwrap_or_else(|| "Automatic".into());
        egui::ComboBox::from_id_salt("sql-injected-source").selected_text(shown).width(200.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut v, String::new(), "Automatic (DATABASE_URL or the only connected one)");
            for ds in &s.db.sources {
                ui.selectable_value(&mut v, ds.config.id.clone(), &ds.config.name);
            }
        });
        if v != set.data_source.clone().unwrap_or_default() {
            let value = if v.is_empty() { Value::Null } else { json!(v) };
            cmds.push(("sql.chooseDataSource", json!({ "dataSource": value })));
        }
        ui.end_row();
        ui.label("Dialect");
        let current = set.dialect.map(|d| serde_json::to_value(d).unwrap_or_default()).unwrap_or(Value::Null);
        let mut picked = current.as_str().unwrap_or_default().to_string();
        let shown = if picked.is_empty() { "The data source's".to_string() } else { picked.clone() };
        egui::ComboBox::from_id_salt("sql-injected-dialect").selected_text(shown).width(200.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut picked, String::new(), "The data source's");
            for d in ["generic", "sqlite", "postgres", "mysql"] {
                ui.selectable_value(&mut picked, d.to_string(), d);
            }
        });
        if picked != current.as_str().unwrap_or_default() {
            let value = if picked.is_empty() { Value::Null } else { json!(picked) };
            cmds.push(("sql.settings", json!({ "dialect": value })));
        }
        ui.end_row();
        ui.label("Keyword case");
        let case = serde_json::to_value(set.format.keyword_case).unwrap_or_default();
        let mut picked = case.as_str().unwrap_or("upper").to_string();
        egui::ComboBox::from_id_salt("sql-keyword-case").selected_text(picked.clone()).width(120.0).show_ui(ui, |ui| {
            for c in ["upper", "lower", "preserve"] {
                ui.selectable_value(&mut picked, c.to_string(), c);
            }
        });
        if Some(picked.as_str()) != case.as_str() {
            cmds.push(("sql.settings", json!({ "keywordCase": picked })));
        }
        ui.end_row();
        ui.label("Indent");
        let mut n = set.format.indent as i64;
        if ui.add(egui::DragValue::new(&mut n).range(1..=16)).changed() {
            cmds.push(("sql.settings", json!({ "indent": n })));
        }
        ui.end_row();
    });
}
