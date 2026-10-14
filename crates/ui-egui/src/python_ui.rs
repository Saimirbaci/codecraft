//! Python: the Python Console and Python Packages tool windows, the status bar interpreter
//! widget and the Python dialogs (Create Virtualenv, Uninstall confirmation, Run setup.py Task)
//! (M11 track Y). Thin: every action is an engine command.

use egui::{Align, Align2, FontId, Id, Key, Layout, Modifiers, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::python::{self, Package};
use codecraft_engine::python_console::ConsoleKind;
use codecraft_engine::python_project::SETUP_TASKS;

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

// ---------------------------------------------------------------- Python Console

/// The Python Console tool window: transcript, input, Variables.
pub fn show_console(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let c = &s.python.console;
    let alive = c.is_alive();
    let running = c.running.is_some();
    ui.horizontal(|ui| {
        let status = if !alive {
            ("Not running", theme.text_muted)
        } else if running {
            ("Running…", theme.warning)
        } else if c.ready {
            ("Ready", theme.success)
        } else {
            ("Starting…", theme.text_muted)
        };
        ui.label(RichText::new(status.0).color(status.1).small());
        ui.label(RichText::new(s.python.interpreter_label()).color(theme.text_muted).small());
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("Clear").on_hover_text("Clear the console output").clicked() {
                cmds.push(("python.console.clear", Value::Null));
            }
            if ui.add_enabled(alive, egui::Button::new("■").small()).on_hover_text("Stop the console").clicked() {
                cmds.push(("python.console.stop", Value::Null));
            }
            if ui
                .add_enabled(running, egui::Button::new("⏸").small())
                .on_hover_text("Interrupt (KeyboardInterrupt)")
                .clicked()
            {
                cmds.push(("python.console.interrupt", Value::Null));
            }
            let (label, tip) = if alive { ("⟲", "Restart the console") } else { ("▶", "Start the console") };
            if ui.small_button(label).on_hover_text(tip).clicked() {
                cmds.push(("python.console.restart", Value::Null));
            }
        });
    });
    ui.separator();

    // Variables on the right.
    egui::Panel::right("python-console-vars").resizable(true).default_size(240.0).show(ui, |ui| {
        ui.label(RichText::new("Variables").strong().small());
        ScrollArea::vertical().id_salt("python-console-vars-scroll").auto_shrink([false, false]).show(ui, |ui| {
            if s.python.console.vars.is_empty() {
                ui.label(RichText::new("No variables").color(theme.text_muted).small());
            }
            egui::Grid::new("python-console-vars-grid").num_columns(2).striped(true).show(ui, |ui| {
                for v in &s.python.console.vars {
                    ui.label(RichText::new(&v.name).monospace()).on_hover_text(&v.type_name);
                    ui.horizontal(|ui| {
                        if v.viewable && ui.small_button("⊞ View").on_hover_text("View as DataFrame / Array").clicked() {
                            cmds.push(("python.dataframe.view", json!({ "expression": v.name, "source": "console" })));
                        }
                        let shape = v.shape.as_deref().map(|s| format!(" {s}")).unwrap_or_default();
                        ui.label(
                            RichText::new(format!("{{{}{shape}}} {}", v.type_name, v.value))
                                .monospace()
                                .color(theme.text_muted),
                        )
                        .on_hover_text(&v.value);
                    });
                    ui.end_row();
                }
            });
        });
    });

    // Input at the bottom.
    let input_id = Id::new("python-console-input");
    egui::Panel::bottom("python-console-input-panel").show_separator_line(true).show(ui, |ui| {
        let focused = ui.memory(|m| m.has_focus(input_id));
        let single_line = !s.python.console.draft.contains('\n');
        if focused {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                cmds.push(("python.console.execute", Value::Null));
            }
            if single_line && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                cmds.push(("python.console.historyPrevious", Value::Null));
            }
            if single_line && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                cmds.push(("python.console.historyNext", Value::Null));
            }
        }
        let prompt = match &s.python.console.awaiting_input {
            Some(p) if p.is_empty() => "input:".to_string(),
            Some(p) => p.clone(),
            None if single_line => ">>>".into(),
            None => "...".into(),
        };
        ui.horizontal_top(|ui| {
            ui.label(RichText::new(prompt).monospace().color(theme.accent));
            let rows = s.python.console.draft.lines().count().clamp(1, 12);
            let edit = TextEdit::multiline(&mut s.python.console.draft)
                .id(input_id)
                .font(FontId::monospace(13.0))
                .desired_rows(rows)
                .desired_width(f32::INFINITY)
                .hint_text("Python code (Enter runs, Shift+Enter adds a line)");
            ui.add(edit);
        });
    });

    // Transcript.
    ScrollArea::both().id_salt("python-console-out").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
        if s.python.console.transcript.is_empty() {
            ui.label(RichText::new("Press ▶ or type code below to start the Python Console.").color(theme.text_muted));
        }
        for (kind, text) in &s.python.console.transcript {
            let color = match kind {
                ConsoleKind::Input => theme.text,
                ConsoleKind::Output => theme.text_editor,
                ConsoleKind::Error => theme.console_stderr,
                ConsoleKind::System => theme.console_system,
            };
            let text = text.strip_suffix('\n').unwrap_or(text);
            let mut rt = RichText::new(text).monospace().color(color);
            if *kind == ConsoleKind::Input {
                rt = rt.strong();
            }
            ui.add(egui::Label::new(rt).selectable(true));
        }
    });
    let start_typing = !cmds.is_empty() && cmds.iter().any(|(id, _)| id.starts_with("python.console.history"));
    run_all(s, st, cmds);
    if start_typing {
        ui.memory_mut(|m| m.request_focus(input_id));
    }
}

// ---------------------------------------------------------------- Python Packages

fn package_row(
    ui: &mut egui::Ui,
    p: &Package,
    selected: bool,
    busy: bool,
    theme: &crate::theme::Theme,
    cmds: &mut Cmds,
) {
    let resp = ui.add(egui::Button::selectable(selected, RichText::new(&p.name).monospace()));
    if resp.clicked() {
        cmds.push(("python.packages.select", json!({ "name": p.name })));
    }
    ui.label(RichText::new(&p.version).monospace());
    match &p.latest {
        Some(latest) => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("→ {latest}")).monospace().color(theme.success));
                if ui.add_enabled(!busy, egui::Button::new("Upgrade").small()).clicked() {
                    cmds.push(("python.packages.upgrade", json!({ "name": p.name })));
                }
            });
        }
        None => {
            ui.label("");
        }
    }
    ui.end_row();
}

/// The Python Packages tool window.
pub fn show_packages(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let busy = s.python.changing();
    let listing = !s.python.jobs.is_empty() && !busy;
    // First open: list the packages.
    if s.python.packages_for.is_none() && s.python.jobs.is_empty() && s.python.packages_error.is_none() {
        cmds.push(("python.packages.refresh", Value::Null));
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(s.python.interpreter_label()).strong().small());
        ui.add(
            TextEdit::singleline(&mut s.python.filter).hint_text("🔍 Search installed packages").desired_width(200.0),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add_enabled(!busy && !listing, egui::Button::new("⟳").small()).on_hover_text("Refresh").clicked() {
                cmds.push(("python.packages.refresh", Value::Null));
            }
            if let Some(name) = s.python.selected_package.clone()
                && ui.add_enabled(!busy, egui::Button::new(format!("Uninstall {name}")).small()).clicked()
            {
                cmds.push(("python.packages.uninstall", json!({ "name": name })));
            }
            if busy || listing {
                ui.spinner();
                if let Some(j) = s.python.jobs.first() {
                    ui.label(RichText::new(&j.title).color(theme.text_muted).small());
                }
            }
        });
    });
    ui.horizontal(|ui| {
        ui.label("Install:");
        let name = ui.add(TextEdit::singleline(&mut s.python.install_name).hint_text("package").desired_width(180.0));
        ui.add(
            TextEdit::singleline(&mut s.python.install_version).hint_text("version (optional)").desired_width(120.0),
        );
        let enter = name.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        let can = !busy && !s.python.install_name.trim().is_empty();
        if ui.add_enabled(can, egui::Button::new("Install")).clicked() || (enter && can) {
            cmds.push(("python.packages.install", Value::Null));
        }
    });
    // A failed run's missing module.
    if let Some(module) = python::missing_modules_in_runs(s).into_iter().next() {
        let package = python::package_for_module(&module);
        if !s.python.packages.iter().any(|p| python::normalize_name(&p.name) == python::normalize_name(&package)) {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("No module named '{module}'")).color(theme.warning));
                if ui.add_enabled(!busy, egui::Button::new(format!("Install {package}")).small()).clicked() {
                    cmds.push(("python.packages.installMissing", json!({ "module": module })));
                }
            });
        }
    }
    if let Some(e) = &s.python.packages_error {
        ui.label(RichText::new(e).color(theme.error).small());
    }
    ui.separator();
    // Log of package operations at the bottom.
    if !s.python.log.is_empty() {
        egui::Panel::bottom("python-packages-log").resizable(true).default_size(110.0).show(ui, |ui| {
            ui.label(RichText::new("Output").strong().small());
            ScrollArea::vertical()
                .id_salt("python-packages-log-scroll")
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for l in &s.python.log {
                        ui.label(RichText::new(l).monospace().color(theme.text_muted).size(12.0));
                    }
                });
        });
    }
    let selected = s.python.selected_package.clone();
    ScrollArea::vertical().id_salt("python-packages-list").auto_shrink([false, false]).show(ui, |ui| {
        let visible = s.python.visible_packages();
        if visible.is_empty() && s.python.packages_error.is_none() {
            let text = if listing { "Listing packages…" } else { "No packages" };
            ui.label(RichText::new(text).color(theme.text_muted));
        }
        egui::Grid::new("python-packages-grid").num_columns(3).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
            if !visible.is_empty() {
                ui.label(RichText::new("Package").strong().small());
                ui.label(RichText::new("Version").strong().small());
                ui.label(RichText::new("Latest").strong().small());
                ui.end_row();
            }
            for p in visible {
                package_row(ui, p, selected.as_deref() == Some(p.name.as_str()), busy, &theme, &mut cmds);
            }
        });
    });
    // Selection is view state.
    cmds.retain(|(id, p)| {
        if *id == "python.packages.select" {
            s.python.selected_package = p.get("name").and_then(Value::as_str).map(str::to_owned);
            false
        } else {
            true
        }
    });
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- status bar widget

/// The interpreter widget in the status bar (Python projects and files): click to switch.
pub fn status_widget(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if !python::python_context(s) {
        return;
    }
    let muted = st.theme.text_muted;
    let label = s.python.interpreter_label();
    let mut cmds: Cmds = Vec::new();
    let resp = ui.menu_button(RichText::new(label).size(12.0).color(muted), |ui| {
        if !s.python.detected {
            cmds.push(("python.interpreter.detect", Value::Null));
        }
        ui.label(RichText::new("Python Interpreter").strong());
        let current = s.python.interpreter.clone();
        if s.python.interpreters.is_empty() {
            let text = if s.python.detected { "No interpreters found" } else { "Detecting interpreters…" };
            ui.label(RichText::new(text).color(muted));
        }
        for i in &s.python.interpreters {
            let on = current.as_deref() == Some(i.path.as_path());
            let text = format!("{}  ·  {}", i.label(), i.kind.label());
            if ui.add(egui::Button::selectable(on, text)).on_hover_text(i.path.display().to_string()).clicked() {
                cmds.push(("python.interpreter.select", json!({ "path": i.path })));
                ui.close();
            }
        }
        ui.separator();
        if s.python.explicit && ui.button("Use the Project's Virtualenv").clicked() {
            cmds.push(("python.interpreter.select", json!({ "path": null })));
            ui.close();
        }
        if ui.button("Create Virtualenv…").clicked() {
            cmds.push(("python.interpreter.createVenv", Value::Null));
            ui.close();
        }
        if ui.button("Manage Packages…").clicked() {
            cmds.push(("tools.managePythonPackages", Value::Null));
            ui.close();
        }
        if ui.button("Python Console").clicked() {
            cmds.push(("tools.pythonOrDebugConsole", Value::Null));
            ui.close();
        }
        if ui.button("Rescan").clicked() {
            cmds.push(("python.interpreter.detect", Value::Null));
        }
    });
    resp.response.on_hover_text("Python interpreter: click to switch");
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- dialogs

fn escape_pressed(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.key_pressed(Key::Escape))
}

/// Dialogs: Create Virtualenv, uninstall confirmation, Run setup.py Task.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    if let Some(name) = s.python.confirm_uninstall.clone() {
        let mut open = true;
        egui::Window::new("Uninstall Package")
            .id(Id::new("python-uninstall"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, vec2(0.0, 120.0))
            .show(&ctx, |ui| {
                ui.label(format!("Uninstall {name} from {}?", s.python.interpreter_label()));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Uninstall").clicked() {
                        cmds.push(("python.packages.uninstall", json!({ "name": name, "confirm": true })));
                    }
                    if ui.button("Cancel").clicked() {
                        s.python.confirm_uninstall = None;
                    }
                });
            });
        if !open || escape_pressed(&ctx) {
            s.python.confirm_uninstall = None;
        }
    }
    if s.python.create_venv.is_some() {
        let mut open = true;
        let interpreters = s.python.interpreters.clone();
        if !s.python.detected {
            cmds.push(("python.interpreter.detect", Value::Null));
        }
        egui::Window::new("Create Virtualenv")
            .id(Id::new("python-create-venv"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, vec2(0.0, 120.0))
            .show(&ctx, |ui| {
                let Some(d) = s.python.create_venv.as_mut() else { return };
                egui::Grid::new("python-create-venv-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Location:");
                    ui.add(TextEdit::singleline(&mut d.dir).desired_width(260.0));
                    ui.end_row();
                    ui.label("Base interpreter:");
                    let shown = d.base.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "Default".into());
                    egui::ComboBox::from_id_salt("python-venv-base").selected_text(shown).width(260.0).show_ui(
                        ui,
                        |ui| {
                            if ui.selectable_label(d.base.is_none(), "Default").clicked() {
                                d.base = None;
                            }
                            for i in interpreters
                                .iter()
                                .filter(|i| i.kind != codecraft_engine::python_env::InterpreterKind::Venv)
                            {
                                let on = d.base.as_deref() == Some(i.path.as_path());
                                if ui
                                    .selectable_label(on, i.label())
                                    .on_hover_text(i.path.display().to_string())
                                    .clicked()
                                {
                                    d.base = Some(i.path.clone());
                                }
                            }
                        },
                    );
                    ui.end_row();
                });
                ui.label(
                    RichText::new("Uses `uv venv` when uv is installed, else `python -m venv`.")
                        .color(theme.text_muted)
                        .small(),
                );
                ui.add_space(6.0);
                if ui.button("Create").clicked() {
                    cmds.push((
                        "python.interpreter.createVenv",
                        json!({ "dir": d.dir, "base": d.base, "select": true }),
                    ));
                }
            });
        if !open || escape_pressed(&ctx) {
            s.python.create_venv = None;
        }
    }
    if s.python.setup_task.is_some() {
        let mut open = true;
        egui::Window::new("Run setup.py Task")
            .id(Id::new("python-setup-task"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, vec2(0.0, 120.0))
            .show(&ctx, |ui| {
                let Some(task) = s.python.setup_task.as_mut() else { return };
                for t in SETUP_TASKS {
                    if ui.add(egui::Button::selectable(false, RichText::new(t).monospace())).clicked() {
                        cmds.push(("tools.runSetupPyTask", json!({ "task": t })));
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Other:");
                    let r = ui.add(TextEdit::singleline(task).hint_text("command [args]").desired_width(200.0));
                    if (r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))) || ui.button("Run").clicked() {
                        let mut words = task.trim().splitn(2, ' ');
                        let t = words.next().unwrap_or("").to_string();
                        let args = words.next().unwrap_or("").to_string();
                        if !t.is_empty() {
                            cmds.push(("tools.runSetupPyTask", json!({ "task": t, "args": args })));
                        }
                    }
                });
            });
        if !open || escape_pressed(&ctx) {
            s.python.setup_task = None;
        }
    }
    run_all(s, st, cmds);
}

/// Whether a Python dialog reads the keyboard.
/// View › Tool Windows › Python Process Output: the running Python processes and their log.
pub fn show_process_output(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    use codecraft_engine::python_output::{self, LineKind};
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let procs = python_output::processes(s);
    ui.horizontal_wrapped(|ui| {
        if procs.is_empty() {
            ui.label(RichText::new("No Python processes are running").color(theme.text_muted).small());
        }
        for p in &procs {
            let pid = p.pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            ui.label(RichText::new(format!("● {}{pid}: {}", p.name, p.status)).small().color(theme.success))
                .on_hover_text(p.interpreter.clone().unwrap_or_default());
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("Clear").clicked() {
                cmds.push(("python.processOutput.clear", Value::Null));
            }
        });
    });
    ui.separator();
    let lines = &s.python.process_output.lines;
    let row_h = 16.0;
    ScrollArea::both()
        .id_salt("python-process-output")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show_rows(ui, row_h, lines.len(), |ui, range| {
            for l in lines.range(range) {
                let color = match l.kind {
                    LineKind::Info => theme.console_system,
                    LineKind::Command => theme.accent,
                    LineKind::Stdout => theme.text_editor,
                    LineKind::Stderr => theme.console_stderr,
                };
                ui.add(
                    egui::Label::new(RichText::new(format!("[{}] {}", l.source, l.text)).monospace().color(color))
                        .selectable(true)
                        .extend(),
                );
            }
        });
    run_all(s, st, cmds);
}

pub fn wants_keyboard(s: &Session) -> bool {
    s.python.confirm_uninstall.is_some() || s.python.create_venv.is_some() || s.python.setup_task.is_some()
}
