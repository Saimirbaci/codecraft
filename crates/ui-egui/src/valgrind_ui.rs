//! Memory analysis in the Run tool window (track CL): a run's Valgrind Memcheck errors and its
//! sanitizer reports as trees (kind → error → stacks → frames linked to source) beside the
//! console; plus the dialogs Import Valgrind XML Results, Attach to an Unstarted Process and
//! Manage Targets. Every action is an engine command.

use egui::{CollapsingHeader, Id, RichText, ScrollArea, TextEdit};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_workspace::RunTabId;

use crate::{UiState, run_command};

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Console,
    Memcheck,
    Sanitizer,
}

/// The Memcheck/Sanitizer views of a run tab. Returns true when one of them is shown (the shell
/// then skips the console).
pub fn show_run_extras(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, tab: RunTabId) -> bool {
    let theme = st.theme.clone();
    let memcheck = s.cpp.valgrind.result(tab).map(|r| r.errors.len());
    let sanitizer = s.cpp.sanitizers.reports(tab).len();
    if memcheck.is_none() && sanitizer == 0 {
        return false;
    }
    let view_id = Id::new(("run-analysis-view", tab.0));
    let default = if memcheck.is_some() && s.cpp.valgrind.show_results.get(&tab.0).copied().unwrap_or(true) {
        View::Memcheck
    } else if sanitizer > 0 {
        View::Sanitizer
    } else {
        View::Console
    };
    let mut view = ui.data_mut(|d| d.get_temp::<u8>(view_id)).map_or(default, |v| match v {
        1 => View::Memcheck,
        2 => View::Sanitizer,
        _ => View::Console,
    });
    if view == View::Memcheck && memcheck.is_none() || view == View::Sanitizer && sanitizer == 0 {
        view = View::Console;
    }
    ui.horizontal(|ui| {
        if ui.selectable_label(view == View::Console, "Console").clicked() {
            view = View::Console;
        }
        if let Some(n) = memcheck
            && ui.selectable_label(view == View::Memcheck, format!("Memcheck {n}")).clicked()
        {
            view = View::Memcheck;
        }
        if sanitizer > 0 && ui.selectable_label(view == View::Sanitizer, format!("Sanitizer {sanitizer}")).clicked() {
            view = View::Sanitizer;
        }
    });
    let code = match view {
        View::Console => 0u8,
        View::Memcheck => 1,
        View::Sanitizer => 2,
    };
    ui.data_mut(|d| d.insert_temp(view_id, code));
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    match view {
        View::Console => return false,
        View::Memcheck => memcheck_view(ui, s, &theme, tab, &mut cmds),
        View::Sanitizer => sanitizer_view(ui, s, &theme, tab, &mut cmds),
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
    true
}

fn frame_line(function: Option<&str>, location: Option<String>, module: Option<&str>) -> String {
    let mut text = function.filter(|f| !f.is_empty()).unwrap_or("??").to_string();
    if let Some(l) = location {
        text.push_str(&format!("  {l}"));
    } else if let Some(m) = module {
        text.push_str(&format!("  ({m})"));
    }
    text
}

fn memcheck_view(
    ui: &mut egui::Ui,
    s: &Session,
    theme: &crate::theme::Theme,
    tab: RunTabId,
    cmds: &mut Vec<(&'static str, Value)>,
) {
    let Some(result) = s.cpp.valgrind.result(tab) else { return };
    if let Some(sig) = &result.fatal_signal {
        ui.label(RichText::new(sig).color(theme.error));
    }
    if result.errors.is_empty() {
        ui.label(RichText::new("Valgrind found no errors.").color(theme.success));
        return;
    }
    ScrollArea::both().id_salt(("memcheck", tab.0)).auto_shrink([false, false]).show(ui, |ui| {
        for (kind, errors) in result.by_kind() {
            CollapsingHeader::new(RichText::new(format!("{kind} ({})", errors.len())).strong())
                .id_salt(("memcheck-kind", tab.0, &kind))
                .default_open(true)
                .show(ui, |ui| {
                    for ei in errors.into_iter().take(1000) {
                        let Some(e) = result.errors.get(ei) else { continue };
                        let mut title = e.what.clone();
                        if e.count > 1 {
                            title.push_str(&format!("  ×{}", e.count));
                        }
                        let header = CollapsingHeader::new(RichText::new(title).color(theme.error))
                            .id_salt(("memcheck-error", tab.0, ei))
                            .default_open(false)
                            .show(ui, |ui| {
                                for (si, stack) in e.stacks.iter().enumerate() {
                                    if !stack.title.is_empty() && si > 0 {
                                        ui.label(RichText::new(&stack.title).color(theme.text_muted));
                                    }
                                    for (fi, f) in stack.frames.iter().enumerate() {
                                        let loc = f.path.as_ref().map(|p| {
                                            let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                            format!("{name}:{}", f.line.map_or(0, |l| l + 1))
                                        });
                                        let text = frame_line(f.function.as_deref(), loc, f.object.as_deref());
                                        let color = if f.project { theme.link } else { theme.text_muted };
                                        let clickable = f.path.is_some() && f.line.is_some();
                                        let resp = ui.add(egui::Label::new(RichText::new(text).monospace().color(color)).sense(egui::Sense::click()));
                                        if clickable && resp.clicked() {
                                            cmds.push(("valgrind.navigate", json!({ "tab": tab.0, "error": ei, "stack": si, "frame": fi })));
                                        }
                                    }
                                }
                            });
                        if header.header_response.double_clicked() {
                            cmds.push(("valgrind.navigate", json!({ "tab": tab.0, "error": ei })));
                        }
                    }
                });
        }
    });
}

fn sanitizer_view(
    ui: &mut egui::Ui,
    s: &Session,
    theme: &crate::theme::Theme,
    tab: RunTabId,
    cmds: &mut Vec<(&'static str, Value)>,
) {
    let reports = s.cpp.sanitizers.reports(tab);
    ScrollArea::both().id_salt(("sanitizer", tab.0)).auto_shrink([false, false]).show(ui, |ui| {
        for (ri, r) in reports.iter().enumerate() {
            let title = format!("{}: {}", r.sanitizer.label(), r.kind);
            let header = CollapsingHeader::new(RichText::new(title).color(theme.error).strong())
                .id_salt(("sanitizer-report", tab.0, ri))
                .default_open(ri == 0)
                .show(ui, |ui| {
                    ui.label(RichText::new(&r.message).color(theme.text));
                    for d in r.details.iter().take(20) {
                        ui.label(RichText::new(d).color(theme.text_muted).size(11.0));
                    }
                    for (si, sec) in r.sections.iter().enumerate() {
                        ui.label(RichText::new(&sec.title).strong());
                        for (fi, f) in sec.frames.iter().enumerate() {
                            let loc = f.location.as_ref().map(|l| format!("{}:{}", l.written, l.line + 1));
                            let text = format!("#{} {}", f.index, frame_line(Some(&f.function), loc, f.module.as_deref()));
                            let color = if f.project { theme.link } else { theme.text_muted };
                            let resp = ui.add(egui::Label::new(RichText::new(text).monospace().color(color)).sense(egui::Sense::click()));
                            if f.location.is_some() && resp.clicked() {
                                cmds.push(("sanitizers.navigate", json!({ "tab": tab.0, "report": ri, "section": si, "frame": fi })));
                            }
                        }
                    }
                    if let Some(sum) = &r.summary {
                        ui.label(RichText::new(sum).color(theme.text_muted).size(11.0));
                    }
                });
            if header.header_response.double_clicked() && r.location.is_some() {
                cmds.push(("sanitizers.navigate", json!({ "tab": tab.0, "report": ri })));
            }
        }
    });
}

/// Dialogs: Import Valgrind XML Results' file picker, Attach to an Unstarted Process, Manage
/// Targets.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if s.cpp.valgrind.import_requested {
        s.cpp.valgrind.import_requested = false;
        if let Some(path) =
            rfd::FileDialog::new().set_title("Import Valgrind XML Results").add_filter("Valgrind XML", &["xml"]).pick_file()
        {
            run_command(s, st, "run.importValgrindXmlResults", &json!({ "path": path }));
        }
    }
    attach_wait_window(ui, s, st);
    if s.cpp.debugging.targets_open {
        targets_window(ui, s, st);
    }
}

fn attach_wait_window(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let Some(dialog) = s.cpp.debugging.attach_wait.clone() else { return };
    let theme = st.theme.clone();
    let mut open = true;
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut process = dialog.process.clone();
    egui::Window::new("Attach to an Unstarted Process")
        .id(Id::new("attach-wait"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, -60.0])
        .show(ui.ctx(), |ui| {
            ui.label("The debugger attaches when a process with this name starts:");
            let resp = ui.add(TextEdit::singleline(&mut process).hint_text("process name or executable path").desired_width(360.0));
            if let Some(e) = &dialog.error {
                ui.label(RichText::new(e).color(theme.error));
            }
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                if ui.button("Attach").clicked() || enter {
                    cmds.push(("run.attachToAnUnstartedProcess", json!({ "process": process })));
                }
                if ui.button("Cancel").clicked() {
                    cmds.push(("debug.attachWait.cancel", Value::Null));
                }
            });
        });
    if process != dialog.process
        && let Some(d) = &mut s.cpp.debugging.attach_wait
    {
        d.process = process;
    }
    if !open {
        cmds.push(("debug.attachWait.cancel", Value::Null));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn targets_window(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let rows = codecraft_engine::cpp::debugging::target_rows(s);
    let user: Vec<String> = s.run_store.user.iter().map(|c| c.name.clone()).collect();
    let mut open = true;
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let selected_id = Id::new("manage-targets-config");
    let mut selected: usize = ui.data_mut(|d| d.get_temp(selected_id)).unwrap_or(0);
    crate::windows::resizable(egui::Window::new("Run Targets").default_size([560.0, 340.0]), st, "manage-targets")
        .id(Id::new("manage-targets"))
        .open(&mut open)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ui.ctx(), |ui| {
            ui.label(RichText::new("Where run configurations run. SSH servers come from Tools › Deployment and ~/.ssh/config.").color(theme.text_muted));
            if !user.is_empty() {
                ui.horizontal(|ui| {
                    ui.label("Configuration:");
                    egui::ComboBox::from_id_salt("manage-targets-combo")
                        .selected_text(user.get(selected).cloned().unwrap_or_default())
                        .show_ui(ui, |ui| {
                            for (i, n) in user.iter().enumerate() {
                                ui.selectable_value(&mut selected, i, n);
                            }
                        });
                });
            }
            ScrollArea::vertical().auto_shrink([false, false]).max_height(260.0).show(ui, |ui| {
                egui::Grid::new("manage-targets-grid").num_columns(4).striped(true).show(ui, |ui| {
                    for r in &rows {
                        ui.label(RichText::new(&r.name).strong());
                        ui.label(RichText::new(format!("{} {}", r.kind, r.detail)).color(theme.text_muted));
                        ui.label(if r.used_by.is_empty() { String::new() } else { r.used_by.join(", ") });
                        if !user.is_empty() && ui.small_button("Use for configuration").clicked() {
                            cmds.push(("run.manageTargets.assign", json!({ "config": selected, "target": r.target })));
                        }
                        ui.end_row();
                    }
                });
            });
            add_target_form(ui, &theme, &mut cmds);
        });
    ui.data_mut(|d| d.insert_temp(selected_id, selected));
    if !open {
        cmds.push(("run.manageTargets.close", Value::Null));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

/// Manage Targets › Add (M16 track RT): a named SSH or Docker target and its Python interpreter
/// (`run.manageTargets.add`). The draft is view state.
fn add_target_form(ui: &mut egui::Ui, theme: &crate::theme::Theme, cmds: &mut Vec<(&'static str, Value)>) {
    const KINDS: [(&str, &str, &str); 4] = [
        ("ssh", "SSH server", "a ~/.ssh/config host or a Deployment server"),
        ("docker", "Docker image", "python:3.12"),
        ("container", "Docker container", "a running container's name"),
        ("compose", "Compose service", "web"),
    ];
    let id = Id::new("manage-targets-add");
    let (mut name, mut kind, mut value, mut python): (String, usize, String, String) =
        ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    ui.separator();
    ui.label(RichText::new("Add a target").strong());
    egui::Grid::new("manage-targets-add-grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
        ui.label("Name:");
        ui.add(TextEdit::singleline(&mut name).hint_text("build box").desired_width(260.0));
        ui.end_row();
        ui.label("Kind:");
        let current = KINDS.get(kind).map_or("SSH server", |k| k.1);
        egui::ComboBox::from_id_salt("manage-targets-add-kind").selected_text(current).show_ui(ui, |ui| {
            for (i, k) in KINDS.iter().enumerate() {
                ui.selectable_value(&mut kind, i, k.1);
            }
        });
        ui.end_row();
        ui.label("Server / image / container / service:");
        let hint = KINDS.get(kind).map_or("", |k| k.2);
        ui.add(TextEdit::singleline(&mut value).hint_text(hint).desired_width(260.0));
        ui.end_row();
        ui.label("Python interpreter there:");
        ui.add(TextEdit::singleline(&mut python).hint_text("(python3)").desired_width(260.0));
        ui.end_row();
    });
    ui.label(
        RichText::new("Run, Debug, Test and Profile of a configuration using it run there (debugpy, gdbserver/lldb-server, cProfile, perf).")
            .color(theme.text_muted)
            .small(),
    );
    let ready = !name.trim().is_empty() && !value.trim().is_empty();
    if ui.add_enabled(ready, egui::Button::new("Add Target")).clicked() {
        let prefix = KINDS.get(kind).map_or("ssh", |k| k.0);
        cmds.push((
            "run.manageTargets.add",
            json!({ "name": name.trim(), "target": format!("{prefix}:{}", value.trim()), "python": python.trim() }),
        ));
        name.clear();
        value.clear();
        python.clear();
    }
    ui.data_mut(|d| d.insert_temp(id, (name, kind, value, python)));
}
