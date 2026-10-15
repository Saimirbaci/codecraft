//! M13 track RC views: the Zig and TypeScript tool windows and the Modify Selected Code popup
//! (prompt over the selection, then the proposal as a diff with Accept / Reject). Thin: every
//! action is an engine command (`zig.*`, `typescript.*`, `editor.modifyCode.*`).

use egui::{Align, Align2, FontId, Id, Key, Layout, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent::diff::LineTag;

use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

fn run_all(s: &mut Session, st: &mut UiState, cmds: Cmds) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

/// The Modify Selected Code popup takes the keyboard while it is shown.
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.modify.active()
}

// ---------------------------------------------------------------- gutter

type MenuState = (egui::Pos2, usize, std::path::PathBuf);
const GUTTER_MENU: &str = "gutter-run-menu";

/// The gutter run menu is open (it reads Escape itself; IDE shortcuts wait).
pub(crate) fn gutter_menu_open(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<MenuState>(Id::new(GUTTER_MENU)).is_some())
}

/// The menu of a ▶ gutter icon (right click): Run, Debug, and every test of the file.
pub(crate) fn gutter_run_menu(
    ui: &mut egui::Ui,
    s: &mut Session,
    st: &mut UiState,
    path: Option<&std::path::Path>,
    open_at: Option<(egui::Pos2, usize)>,
) {
    let id = Id::new(GUTTER_MENU);
    if let (Some((pos, line)), Some(p)) = (open_at, path) {
        ui.data_mut(|d| d.insert_temp::<MenuState>(id, (pos, line, p.to_path_buf())));
    }
    let Some((pos, line, file)) = ui.data(|d| d.get_temp::<MenuState>(id)) else { return };
    let mut cmds: Cmds = Vec::new();
    let mut close = false;
    let at = json!({ "path": file, "line": line });
    let area = egui::Area::new(id.with("area")).order(egui::Order::Foreground).fixed_pos(pos).show(ui.ctx(), |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_min_width(200.0);
            if ui.button("▶  Run").on_hover_text("Run Context Configuration (⌃⇧R)").clicked() {
                cmds.push(("run.contextConfiguration", at.clone()));
                close = true;
            }
            if ui.button("🐞  Debug").on_hover_text("Debug Context Configuration (⌃⇧D)").clicked() {
                cmds.push(("debug.atCaret", at.clone()));
                close = true;
            }
            ui.separator();
            if ui.button("Run All Tests in File").clicked() {
                cmds.push(("run.testsInFile", json!({ "path": file })));
                close = true;
            }
            if ui.button("Debug All Tests in File").clicked() {
                cmds.push(("debug.testsInFile", json!({ "path": file })));
                close = true;
            }
        });
    });
    if (open_at.is_none() && area.response.clicked_elsewhere()) || ui.input(|i| i.key_pressed(Key::Escape)) {
        close = true;
    }
    if close {
        ui.data_mut(|d| d.remove::<MenuState>(id));
    }
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- Edit Configurations

/// "Before launch": the tasks in order, with move up/down and remove, and + to add one.
pub(crate) fn before_launch_section(
    ui: &mut egui::Ui,
    theme: &crate::theme::Theme,
    f: &mut crate::settings_ui::RunForm,
    config_names: &[String],
) {
    use codecraft_engine::run_config_store::BeforeLaunchTask;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Before launch").strong());
        ui.menu_button("+", |ui| {
            if ui
                .button("Build")
                .on_hover_text("The configuration's build (cargo build, the CMake target), else Build Project")
                .clicked()
            {
                f.before_launch.push(BeforeLaunchTask::Build);
                ui.close();
            }
            ui.menu_button("Run Another Configuration", |ui| {
                ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                    for name in config_names.iter().filter(|n| **n != f.name) {
                        if ui.button(name).clicked() {
                            f.before_launch.push(BeforeLaunchTask::RunConfiguration { name: name.clone() });
                            ui.close();
                        }
                    }
                });
            });
            if ui
                .button("Run External Tool…")
                .on_hover_text("A command line, run in the project (or a working directory)")
                .clicked()
            {
                f.before_launch.push(BeforeLaunchTask::External { command: String::new(), working_dir: String::new() });
                ui.close();
            }
        })
        .response
        .on_hover_text("Add a task that runs before the configuration starts");
    });
    if f.before_launch.is_empty() {
        ui.label(RichText::new("  There are no tasks to run before launch").color(theme.text_disabled));
    }
    let mut action: Option<(usize, i32)> = None;
    let len = f.before_launch.len();
    for (i, task) in f.before_launch.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            match task {
                BeforeLaunchTask::Build => {
                    ui.label("🔨 Build");
                }
                BeforeLaunchTask::RunConfiguration { name } => {
                    ui.label(format!("▶ Run '{name}'"));
                }
                BeforeLaunchTask::External { command, working_dir } => {
                    ui.label("⚙ Run");
                    ui.add(
                        TextEdit::singleline(command)
                            .hint_text("command line, e.g. npm run codegen")
                            .font(FontId::monospace(12.0))
                            .desired_width(240.0),
                    );
                    ui.add(TextEdit::singleline(working_dir).hint_text("in (project)").desired_width(110.0));
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Remove").clicked() {
                    action = Some((i, 0));
                }
                if ui.add_enabled(i + 1 < len, egui::Button::new("↓").small()).on_hover_text("Move Down").clicked() {
                    action = Some((i, 1));
                }
                if ui.add_enabled(i > 0, egui::Button::new("↑").small()).on_hover_text("Move Up").clicked() {
                    action = Some((i, -1));
                }
            });
        });
    }
    match action {
        Some((i, 0)) if i < f.before_launch.len() => {
            f.before_launch.remove(i);
        }
        Some((i, -1)) if i > 0 && i < f.before_launch.len() => f.before_launch.swap(i, i - 1),
        Some((i, 1)) if i + 1 < f.before_launch.len() => f.before_launch.swap(i, i + 1),
        _ => {}
    }
}

/// Debugger options: LLDB init commands, stop at entry, the debuggee's environment.
pub(crate) fn debugger_section(ui: &mut egui::Ui, theme: &crate::theme::Theme, f: &mut crate::settings_ui::RunForm) {
    ui.add_space(6.0);
    egui::CollapsingHeader::new(RichText::new("Debugger").strong()).id_salt("run-config-debugger").show(ui, |ui| {
        ui.label(RichText::new("LLDB commands run before the program starts (one per line):").color(theme.text_muted));
        ui.add(
            TextEdit::multiline(&mut f.debugger_init)
                .hint_text("settings set target.x86-disassembly-flavor intel")
                .font(FontId::monospace(12.0))
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.checkbox(&mut f.debugger_stop, "Stop at the program's entry point");
        ui.horizontal(|ui| {
            ui.label("Debuggee environment:");
            ui.add(
                TextEdit::singleline(&mut f.debugger_env).hint_text("RUST_BACKTRACE=1").desired_width(f32::INFINITY),
            );
        });
    });
}

// ---------------------------------------------------------------- Zig

/// View › Tool Windows › Zig: the build steps of build.zig with Run and Debug.
pub fn show_zig(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!s.zig.loading, egui::Button::new("⟳ Reload"))
            .on_hover_text("Reload build steps (zig build -l)")
            .clicked()
        {
            cmds.push(("zig.refresh", Value::Null));
        }
        let selected = s.zig.selected.and_then(|i| s.zig.steps.get(i)).map(|st| st.name.clone());
        if ui.add_enabled(selected.is_some(), egui::Button::new("▶ Run")).on_hover_text("zig build <step>").clicked()
            && let Some(step) = &selected
        {
            cmds.push(("zig.runStep", json!({ "step": step })));
        }
        if ui
            .add_enabled(selected.is_some(), egui::Button::new("🐞 Debug"))
            .on_hover_text("Build, then debug the program it installs (zig-out/bin)")
            .clicked()
            && let Some(step) = &selected
        {
            cmds.push(("zig.debugStep", json!({ "step": step })));
        }
        if s.zig.loading {
            ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(root) = &s.zig.root {
                ui.label(
                    RichText::new(root.join("build.zig").display().to_string()).color(theme.text_muted).size(11.0),
                );
            }
        });
    });
    ui.separator();
    if let Some(e) = &s.zig.error {
        ui.label(RichText::new(e).color(theme.error));
    }
    if s.zig.steps.is_empty() && !s.zig.loading && s.zig.error.is_none() {
        ui.label(RichText::new("No build steps loaded. Reload lists the steps of build.zig.").color(theme.text_muted));
    }
    ScrollArea::vertical().id_salt("zig-steps").auto_shrink([false, false]).show(ui, |ui| {
        for (i, step) in s.zig.steps.iter().enumerate() {
            ui.horizontal(|ui| {
                let label = if step.default { format!("{} (default)", step.name) } else { step.name.clone() };
                let resp =
                    ui.add(egui::Button::selectable(s.zig.selected == Some(i), RichText::new(label).monospace()));
                if resp.clicked() {
                    cmds.push(("zig.select", json!({ "index": i })));
                }
                if resp.double_clicked() {
                    cmds.push(("zig.runStep", json!({ "step": step.name })));
                }
                if ui.small_button("▶").on_hover_text(format!("Run 'zig build {}'", step.name)).clicked() {
                    cmds.push(("zig.runStep", json!({ "step": step.name })));
                }
                ui.label(RichText::new(&step.description).color(theme.text_muted));
            });
        }
    });
    run_all(s, st, cmds);
}

// ---------------------------------------------------------------- TypeScript

/// View › Tool Windows › TypeScript: compiler errors of the selected tsconfig, its options, and
/// Restart TypeScript Service.
pub fn show_typescript(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let ts = &s.typescript;
    ui.horizontal(|ui| {
        let current =
            ts.configs.get(ts.selected).map(|c| short(s, &c.path)).unwrap_or_else(|| "no tsconfig.json".into());
        egui::ComboBox::from_id_salt("ts-config").selected_text(current).show_ui(ui, |ui| {
            for (i, c) in ts.configs.iter().enumerate() {
                if ui.selectable_label(i == ts.selected, short(s, &c.path)).clicked() {
                    cmds.push(("typescript.selectConfig", json!({ "index": i })));
                }
            }
        });
        if ui
            .add_enabled(!ts.running && !ts.configs.is_empty(), egui::Button::new("▶ Check"))
            .on_hover_text("tsc --noEmit -p <tsconfig>")
            .clicked()
        {
            cmds.push(("typescript.check", Value::Null));
        }
        if ui.button("⟳ Restart Service").on_hover_text("Restart TypeScript Service").clicked() {
            cmds.push(("typescript.restartService", Value::Null));
        }
        if ts.running {
            ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
            ui.label(RichText::new("Checking…").color(theme.text_muted));
        } else if ts.checked {
            let errors = ts.errors.iter().filter(|e| e.category == "error").count();
            let ms = ts.last_duration_ms.map(|d| format!(" in {d} ms")).unwrap_or_default();
            let text = match errors {
                0 => format!("No errors{ms}"),
                1 => format!("1 error{ms}"),
                n => format!("{n} errors{ms}"),
            };
            ui.label(RichText::new(text).color(if errors == 0 { theme.success } else { theme.error }));
        }
    });
    ui.separator();
    if let Some(e) = &ts.error {
        ui.label(RichText::new(e).color(theme.error));
    }
    egui::Panel::right("ts-config-options").resizable(true).default_size(260.0).show(ui, |ui| {
        ui.label(RichText::new("tsconfig").strong());
        match ts.configs.get(ts.selected) {
            Some(c) => {
                if let Some(e) = &c.error {
                    ui.label(RichText::new(e).color(theme.error));
                }
                egui::Grid::new("ts-options").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                    for (k, v) in &c.options {
                        ui.label(RichText::new(k).color(theme.text_muted));
                        ui.label(RichText::new(v).monospace());
                        ui.end_row();
                    }
                });
            }
            None => {
                ui.label(RichText::new("The project has no tsconfig.json.").color(theme.text_muted));
            }
        }
    });
    ScrollArea::vertical().id_salt("ts-errors").auto_shrink([false, false]).show(ui, |ui| {
        if ts.errors.is_empty() && ts.checked && !ts.running {
            ui.label(RichText::new("The compiler reported no problems.").color(theme.text_muted));
        }
        let mut file: Option<&std::path::Path> = None;
        for (i, e) in ts.errors.iter().enumerate() {
            if e.path.as_deref() != file {
                file = e.path.as_deref();
                let name = file.map(|p| short(s, p)).unwrap_or_else(|| "Project".into());
                ui.add_space(4.0);
                ui.label(RichText::new(name).strong());
            }
            let color = match e.category.as_str() {
                "error" => theme.error,
                "warning" => theme.warning,
                _ => theme.text_muted,
            };
            let first = e.message.lines().next().unwrap_or("");
            let text = format!("{}:{}  {}  {first}", e.line + 1, e.column + 1, e.code);
            let resp = ui
                .add(
                    egui::Button::selectable(ts.selected_error == Some(i), RichText::new(text).color(color))
                        .frame_when_inactive(false),
                )
                .on_hover_text(&e.message);
            if resp.clicked() && e.path.is_some() {
                cmds.push(("typescript.openError", json!({ "index": i })));
            }
        }
    });
    run_all(s, st, cmds);
}

fn short(s: &Session, path: &std::path::Path) -> String {
    match s.project_root() {
        Some(root) => {
            path.strip_prefix(&root).map(|p| p.display().to_string()).unwrap_or_else(|_| path.display().to_string())
        }
        None => path.display().to_string(),
    }
}

// ---------------------------------------------------------------- Modify Selected Code

/// The prompt over the selection, the spinner while the model answers, then the diff.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if !s.modify.active() {
        return;
    }
    let ctx = ui.ctx().clone();
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let anchor = st.caret_screen.unwrap_or(ctx.content_rect().center());
    let mut open = true;
    let proposal = s.modify.proposal.clone();
    let title = if proposal.is_some() { "Modified code" } else { "Modify Selected Code" };
    egui::Window::new(title)
        .id(Id::new("modify-selected-code"))
        .collapsible(false)
        .resizable(true)
        .open(&mut open)
        .default_pos(anchor + vec2(0.0, 8.0))
        .default_width(560.0)
        .pivot(Align2::LEFT_TOP)
        .show(&ctx, |ui| {
            if let Some(p) = &proposal {
                ui.label(
                    RichText::new(format!("“{}”   +{} −{} lines", p.instruction, p.added, p.removed))
                        .color(theme.text_muted),
                );
                ScrollArea::vertical().max_height(360.0).id_salt("modify-diff").show(ui, |ui| {
                    for line in &p.diff {
                        let (sign, bg, fg) = match line.tag {
                            LineTag::Insert => ("+", theme.diff_added_bg, theme.diff_added),
                            LineTag::Delete => ("-", theme.diff_removed_bg, theme.diff_removed),
                            LineTag::Equal => (" ", egui::Color32::TRANSPARENT, theme.text_editor),
                        };
                        egui::Frame::new().fill(bg).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(format!("{sign} {}", line.text)).font(FontId::monospace(12.5)).color(fg),
                            );
                        });
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Accept").on_hover_text("Replace the code (⏎)").clicked() {
                        cmds.push(("editor.modifyCode.accept", Value::Null));
                    }
                    if ui.button("Reject").on_hover_text("Keep the code (Esc)").clicked() {
                        cmds.push(("editor.modifyCode.reject", Value::Null));
                    }
                });
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    cmds.push(("editor.modifyCode.accept", Value::Null));
                }
                if ui.input(|i| i.key_pressed(Key::Escape)) {
                    cmds.push(("editor.modifyCode.reject", Value::Null));
                }
                return;
            }
            let pending = s.modify.busy();
            let mut text = s.modify.prompt.as_ref().map(|p| p.instruction.clone()).unwrap_or_default();
            let edit = ui.add_enabled(
                !pending,
                TextEdit::singleline(&mut text)
                    .id(Id::new("modify-instruction"))
                    .hint_text("How should the code change? (e.g. “handle the error instead of unwrap”)")
                    .desired_width(f32::INFINITY),
            );
            if !pending && !edit.has_focus() && ctx.memory(|m| m.focused().is_none()) {
                edit.request_focus();
            }
            if edit.changed() {
                cmds.push(("editor.modifyCode.setInstruction", json!({ "text": text })));
            }
            let submit = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            ui.horizontal(|ui| {
                if pending {
                    ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
                    ui.label(RichText::new("Waiting for the model…").color(theme.text_muted));
                } else if (ui.add_enabled(!text.trim().is_empty(), egui::Button::new("Send")).clicked() || submit)
                    && !text.trim().is_empty()
                {
                    cmds.push(("editor.modifyCode.submit", json!({ "instruction": text })));
                }
                if let Some(e) = &s.modify.error {
                    ui.label(RichText::new(e).color(theme.error));
                }
            });
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                cmds.push(("editor.modifyCode.cancel", Value::Null));
            }
        });
    if !open {
        cmds.push(("editor.modifyCode.cancel", Value::Null));
    }
    let closes = cmds.iter().any(|(id, _)| {
        matches!(*id, "editor.modifyCode.cancel" | "editor.modifyCode.accept" | "editor.modifyCode.reject")
    });
    run_all(s, st, cmds);
    if closes {
        st.focus_editor = true;
    }
}
