//! Settings › Languages & Frameworks › Python (M15 track EA, QA M14 #26): the project's
//! interpreter (detected virtualenvs, uv/Poetry/Conda/pyenv/system Pythons, any path), the
//! language server and its type checking mode, Ruff, and the formatter settings.
//!
//! The interpreter is a project choice applied at once (`python.interpreter.select`, like the
//! status bar widget); the other rows are settings of the dialog's draft (OK / Apply).

use egui::{Id, RichText, Ui};
use serde_json::{Value, json};

use codecraft_engine::{LspStatus, ServerKind, Session, settings};
use codecraft_workspace::Settings;

use crate::UiState;
use crate::settings_ui::{Cmds, setting_grid_keys};

fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).strong());
    ui.add_space(2.0);
}

fn server_label(id: &str) -> &str {
    match id {
        "auto" => "Automatic (virtualenv, managed basedpyright, PATH)",
        "basedpyright" => "basedpyright",
        "pyright" => "Pyright",
        "pylsp" => "python-lsp-server (pylsp)",
        "ty" => "ty",
        "none" => "None (no language server)",
        other => other,
    }
}

fn type_checking_label(id: &str) -> &str {
    if id.is_empty() { "Project default (pyproject.toml / pyrightconfig.json)" } else { id }
}

/// The page.
pub(crate) fn page(ui: &mut Ui, s: &mut Session, st: &mut UiState, draft: &Settings, cmds: &mut Cmds) {
    let theme = st.theme.clone();
    let muted = |t: String| RichText::new(t).color(theme.text_muted).small();

    // ------------------------------------------------------------ interpreter
    heading(ui, "Python Interpreter");
    if s.project_root().is_none() {
        ui.label(muted("Open a project to choose its interpreter.".into()));
    } else {
        let current = s.python.interpreter.clone();
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("python-settings-interpreter")
                .selected_text(s.python.interpreter_label())
                .width(320.0)
                .show_ui(ui, |ui| {
                    for i in &s.python.interpreters {
                        let on = current.as_deref() == Some(i.path.as_path());
                        let text = format!("{}  ·  {}", i.label(), i.kind.label());
                        if ui.add(egui::Button::selectable(on, text)).on_hover_text(i.path.display().to_string()).clicked() {
                            cmds.push(("python.interpreter.select", json!({ "path": i.path })));
                        }
                    }
                    if ui.add(egui::Button::selectable(!s.python.explicit, "Project default (its virtualenv)")).clicked() {
                        cmds.push(("python.interpreter.select", json!({ "path": null })));
                    }
                });
            let detecting = s.python.busy() && !s.python.detected;
            if ui.add_enabled(!detecting, egui::Button::new("Detect")).on_hover_text("Find virtualenvs, uv, Poetry, Conda, pyenv and system Pythons").clicked() {
                cmds.push(("python.interpreter.detect", Value::Null));
            }
            if ui.button("Create Virtualenv…").on_hover_text("uv venv when uv is installed, else python -m venv").clicked() {
                cmds.push(("python.interpreter.createVenv", Value::Null));
            }
        });
        let path_text = current.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "python3 from PATH".into());
        let note = if s.python.interpreters.is_empty() && !s.python.detected {
            format!("{path_text} · click Detect to list the interpreters on this machine")
        } else {
            path_text
        };
        ui.label(muted(note));
        ui.horizontal(|ui| {
            let id = Id::new("python-settings-add-path");
            let mut path: String = ui.data(|d| d.get_temp(id)).unwrap_or_default();
            let r = ui.add(egui::TextEdit::singleline(&mut path).hint_text("/path/to/bin/python").desired_width(320.0));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Interpreter path"));
            let add = ui.add_enabled(!path.trim().is_empty(), egui::Button::new("Add Interpreter")).clicked()
                || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !path.trim().is_empty());
            if add {
                cmds.push(("python.interpreter.select", json!({ "path": path.trim() })));
                path.clear();
            }
            ui.data_mut(|d| d.insert_temp(id, path));
        });
    }

    // ------------------------------------------------------------ language server
    heading(ui, "Language Server");
    ui.horizontal(|ui| {
        ui.label("Server:");
        let cur = draft.python_language_server.clone();
        let mut chosen = cur.clone();
        egui::ComboBox::from_id_salt(("setting", "python_language_server"))
            .selected_text(server_label(&cur))
            .width(320.0)
            .show_ui(ui, |ui| {
                for o in settings::PYTHON_LANGUAGE_SERVERS {
                    ui.selectable_value(&mut chosen, (*o).to_string(), server_label(o));
                }
            });
        if chosen != cur {
            cmds.push(("settings.set", json!({ "key": "python_language_server", "value": chosen })));
        }
    });
    let status = s.server_status(ServerKind::Python);
    let running = s.server_name(ServerKind::Python).map(str::to_string);
    let found = s.python_server_command();
    let line = match (&status, running, &found) {
        (LspStatus::Ready, Some(n), _) => format!("Running: {n}"),
        (LspStatus::Starting, Some(n), _) => format!("Starting {n}…"),
        (LspStatus::Unavailable(m) | LspStatus::Stopped(m), _, _) => m.clone(),
        (_, _, Some(c)) => format!("Will use {} ({})", c.name, c.program.display()),
        (_, _, None) => "No Python language server is installed".into(),
    };
    ui.label(muted(line));
    let managed = codecraft_engine::analysis::install::installed(s, "basedpyright");
    let wants_basedpyright = matches!(draft.python_language_server.as_str(), "auto" | "basedpyright" | "");
    if wants_basedpyright && found.is_none() && managed.is_none() {
        let installing = s.analysis.install.running.as_ref().is_some_and(|j| j.tool == "basedpyright");
        ui.horizontal(|ui| {
            if installing {
                ui.label(muted("Installing basedpyright…".into()));
            } else if ui
                .button("Install basedpyright")
                .on_hover_text("uv tool install basedpyright into CodeCraft's own tools ($CODECRAFT_HOME/tools), never globally")
                .clicked()
            {
                cmds.push(("analysis.installTool", json!({ "tool": "basedpyright", "confirm": true })));
            }
        });
    } else if let Some(p) = managed {
        ui.label(muted(format!("Managed basedpyright: {}", p.display())));
    }
    ui.horizontal(|ui| {
        ui.label("Type checking:");
        let cur = draft.python_type_checking.clone();
        let mut chosen = cur.clone();
        egui::ComboBox::from_id_salt(("setting", "python_type_checking"))
            .selected_text(type_checking_label(&cur))
            .width(320.0)
            .show_ui(ui, |ui| {
                for o in settings::PYTHON_TYPE_CHECKING {
                    ui.selectable_value(&mut chosen, (*o).to_string(), type_checking_label(o));
                }
            });
        if chosen != cur {
            cmds.push(("settings.set", json!({ "key": "python_type_checking", "value": chosen })));
        }
    })
    .response
    .on_hover_text("pyright / basedpyright typeCheckingMode (sent to the server; it restarts)");

    // ------------------------------------------------------------ Ruff
    heading(ui, "Ruff");
    let ruff = codecraft_lsp::which("ruff");
    ui.label(muted(match &ruff {
        Some(p) => format!("Ruff: {}", p.display()),
        None => "Ruff is not installed (uv tool install ruff, or pip install ruff in the project's virtualenv)".into(),
    }));
    let mut on_save = draft.analysis_on_save;
    if ui
        .checkbox(&mut on_save, "Analyze files on save")
        .on_hover_text("Ruff runs on a Python file when it is saved (with the active inspection profile's rules); the findings show in the editor with Alt+Enter fixes")
        .changed()
    {
        cmds.push(("settings.set", json!({ "key": "analysis_on_save", "value": on_save })));
    }
    codecraft_engine::analysis::profiles::ensure_loaded(s);
    let profile = codecraft_engine::analysis::profiles::resolve(s, None).ok();
    if let Some(p) = profile {
        ui.horizontal(|ui| {
            ui.label("Extra rules:");
            let id = Id::new(("python-settings-ruff-select", p.name.clone()));
            let current = p.ruff_select.join(", ");
            let mut v: String = ui.data(|d| d.get_temp(id)).unwrap_or_else(|| current.clone());
            let r = ui.add(egui::TextEdit::singleline(&mut v).hint_text("B, UP, SIM").desired_width(200.0));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Ruff extra rules"));
            if ui.add_enabled(v != current, egui::Button::new("Apply")).clicked() {
                cmds.push(("analysis.profile.setOption", json!({ "option": "ruffSelect", "value": v })));
            }
            ui.data_mut(|d| d.insert_temp(id, v));
            ui.label(muted(format!("profile '{}'", p.name)));
        });
    }
    if ui.link("Rules and severities: Editor › Inspections…").clicked() {
        cmds.push(("settings.dialog.page", json!({ "page": settings::PAGE_INSPECTIONS })));
    }

    // ------------------------------------------------------------ formatting
    heading(ui, "Formatting");
    setting_grid_keys(ui, st, draft, &["python_formatter", "python_reformat_on_save", "python_optimize_imports_on_save"], cmds);
}
