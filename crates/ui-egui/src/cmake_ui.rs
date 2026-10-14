//! The CMake tool window (track CL): toolbar (Reload CMake Project, Stop, profile selector,
//! Build All, Clean), and tabs for the reload log, the targets (build, run), the cache variables
//! (filter, advanced, edit), the test executables (CTest, run, list tests) and the Conan/vcpkg
//! dependencies. Every action is an engine command (`cmake.*`, `cpp.tests.*`, `cpp.dependencies.*`).

use egui::{Align, FontId, Layout, RichText, ScrollArea, TextEdit};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::cpp::CmakeTab;
use codecraft_engine::cpp::jobs::JobStatus;
use codecraft_engine::testing::Framework;

use crate::{UiState, run_command};

fn job_status_text(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Running => "running",
        JobStatus::Succeeded => "finished",
        JobStatus::Failed(_) => "failed",
        JobStatus::Stopped => "stopped",
    }
}

fn framework_label(fw: Option<Framework>) -> &'static str {
    fw.map(Framework::cpp_label).unwrap_or("")
}

/// The CMake tool window.
pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    if !s.cpp.cmake.is_cmake {
        ui.label(RichText::new("Not a CMake project: there is no CMakeLists.txt in the project root.").color(theme.text_muted));
        return;
    }
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let reload = s.cpp.cmake.reload_job.and_then(|id| s.cpp.jobs.get(id));
    let reloading = reload.is_some_and(|j| j.is_running());

    // ------------------------------------------------------------ toolbar
    ui.horizontal(|ui| {
        if ui.add_enabled(!reloading, egui::Button::new("⟳ Reload")).on_hover_text("Reload CMake Project").clicked() {
            cmds.push(("cmake.reload", Value::Null));
        }
        if ui.add_enabled(reloading, egui::Button::new("■")).on_hover_text("Stop").clicked() {
            cmds.push(("cmake.stop", Value::Null));
        }
        ui.separator();
        let selected = s.cpp.cmake.selected.clone().unwrap_or_default();
        egui::ComboBox::from_id_salt("cmake-profile").selected_text(format!("Profile: {selected}")).show_ui(ui, |ui| {
            for p in &s.cpp.cmake.profiles {
                let label = match &p.preset {
                    Some(_) => format!("{} (preset)", p.name),
                    None => p.name.clone(),
                };
                if ui.selectable_label(p.name == selected, label).on_hover_text(p.build_dir.display().to_string()).clicked()
                    && p.name != selected
                {
                    cmds.push(("cmake.selectProfile", json!({ "profile": p.name })));
                }
            }
        });
        if ui.button("Build All").clicked() {
            cmds.push(("cmake.buildTarget", Value::Null));
        }
        if ui.button("Clean").clicked() {
            cmds.push(("cmake.clean", Value::Null));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if s.cpp.cmake.analyzing() {
                ui.add(egui::Spinner::new().size(12.0).color(theme.text_muted));
            }
            if s.cpp.cmake.needs_reload {
                ui.label(RichText::new("Reload to apply changes").color(theme.warning));
            }
            if let Some(dir) = s.cpp.cmake.active().map(|p| p.build_dir.display().to_string()) {
                ui.label(RichText::new(dir).color(theme.text_muted).size(11.0));
            }
        });
    });
    for e in &s.cpp.cmake.preset_errors {
        ui.label(RichText::new(format!("CMakePresets: {e}")).color(theme.warning).size(11.0));
    }

    // ------------------------------------------------------------ tabs
    let tab = s.cpp.cmake.tab;
    ui.horizontal(|ui| {
        let targets = s.cpp.cmake.codemodel.as_ref().map_or(0, |m| m.targets.len());
        for (t, label, key) in [
            (CmakeTab::Log, "Log".to_string(), "log"),
            (CmakeTab::Targets, format!("Targets {targets}"), "targets"),
            (CmakeTab::Cache, "Cache".to_string(), "cache"),
            (CmakeTab::Tests, "Tests".to_string(), "tests"),
            (CmakeTab::Dependencies, "Dependencies".to_string(), "dependencies"),
        ] {
            if ui.selectable_label(tab == t, label).clicked() && tab != t {
                cmds.push(("cmake.showTab", json!({ "tab": key })));
            }
        }
    });
    ui.separator();
    match tab {
        CmakeTab::Log => log_view(ui, s, st),
        CmakeTab::Targets => targets_view(ui, s, st, &mut cmds),
        CmakeTab::Cache => cache_view(ui, s, st, &mut cmds),
        CmakeTab::Tests => tests_view(ui, s, st, &mut cmds),
        CmakeTab::Dependencies => deps_view(ui, s, st, &mut cmds),
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn lines_view(ui: &mut egui::Ui, st: &UiState, id: &str, lines: &[String]) {
    let font = FontId::monospace(12.0);
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    egui::Frame::new().fill(st.theme.editor_bg).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
        ScrollArea::both().id_salt(id).auto_shrink([false, false]).stick_to_bottom(true).show_rows(ui, row_h, lines.len(), |ui, range| {
            for line in lines.get(range).unwrap_or(&[]) {
                let color = if line.contains("Error") || line.contains("error:") {
                    st.theme.console_stderr
                } else if line.contains("Warning") || line.contains("warning:") {
                    st.theme.warning
                } else {
                    st.theme.text_editor
                };
                ui.label(RichText::new(line).font(font.clone()).color(color));
            }
        });
    });
}

fn log_view(ui: &mut egui::Ui, s: &Session, st: &UiState) {
    match s.cpp.cmake.reload_job.and_then(|id| s.cpp.jobs.get(id)) {
        Some(job) => {
            ui.label(RichText::new(format!("{} — {}", job.title, job_status_text(job.status))).color(st.theme.text_muted));
            lines_view(ui, st, "cmake-log", &job.lines);
        }
        None => {
            ui.label(RichText::new("Reload the CMake project to configure it with the selected profile.").color(st.theme.text_muted));
        }
    }
}

fn targets_view(ui: &mut egui::Ui, s: &Session, st: &UiState, cmds: &mut Vec<(&'static str, Value)>) {
    let theme = &st.theme;
    if let Some(e) = &s.cpp.cmake.codemodel_error {
        ui.label(RichText::new(e).color(theme.text_muted));
    }
    let Some(model) = &s.cpp.cmake.codemodel else { return };
    ScrollArea::vertical().id_salt("cmake-targets").auto_shrink([false, false]).show(ui, |ui| {
        for t in &model.targets {
            ui.horizontal(|ui| {
                let fw = s.cpp.cmake.frameworks.get(&t.name).copied();
                ui.label(RichText::new(&t.name).strong());
                ui.label(RichText::new(t.kind.to_ascii_lowercase().replace('_', " ")).color(theme.text_muted).size(11.0));
                if fw.is_some() {
                    ui.label(RichText::new(framework_label(fw)).color(theme.accent).size(11.0));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if t.is_executable() && ui.small_button("▶ Run").clicked() {
                        cmds.push(("cmake.runTarget", json!({ "target": t.name })));
                    }
                    if !matches!(t.kind.as_str(), "INTERFACE_LIBRARY") && ui.small_button("Build").clicked() {
                        cmds.push(("cmake.buildTarget", json!({ "target": t.name })));
                    }
                });
            })
            .response
            .on_hover_text(t.artifacts.iter().map(|a| a.display().to_string()).collect::<Vec<_>>().join("\n"));
        }
    });
}

fn cache_view(ui: &mut egui::Ui, s: &Session, st: &UiState, cmds: &mut Vec<(&'static str, Value)>) {
    let theme = &st.theme;
    let mut filter = s.cpp.cmake.cache_filter.clone();
    let mut advanced = s.cpp.cmake.show_advanced;
    ui.horizontal(|ui| {
        let changed = ui.add(TextEdit::singleline(&mut filter).hint_text("Filter").desired_width(220.0)).changed();
        let toggled = ui.checkbox(&mut advanced, "Advanced").changed();
        if changed || toggled {
            cmds.push(("cmake.cache", json!({ "filter": filter, "advanced": advanced })));
        }
    });
    if let Some(e) = &s.cpp.cmake.cache_error {
        ui.label(RichText::new(e).color(theme.text_muted));
    }
    ScrollArea::vertical().id_salt("cmake-cache").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("cmake-cache-grid").num_columns(3).striped(true).show(ui, |ui| {
            for e in s.cpp.cmake.visible_cache().into_iter().take(2000) {
                ui.label(RichText::new(&e.name).monospace()).on_hover_text(&e.help);
                ui.label(RichText::new(&e.kind).color(theme.text_muted).size(11.0));
                let id = egui::Id::new(("cmake-cache-edit", &e.name));
                if e.kind == "BOOL" {
                    let mut on = matches!(e.value.to_ascii_uppercase().as_str(), "ON" | "TRUE" | "1" | "YES" | "Y");
                    if ui.checkbox(&mut on, "").changed() {
                        cmds.push(("cmake.cache.set", json!({ "name": e.name, "value": if on { "ON" } else { "OFF" } })));
                    }
                } else {
                    let mut text = ui.data_mut(|d| d.get_temp::<String>(id)).unwrap_or_else(|| e.value.clone());
                    let resp = ui.add(TextEdit::singleline(&mut text).desired_width(320.0));
                    if resp.changed() {
                        ui.data_mut(|d| d.insert_temp(id, text.clone()));
                    }
                    if resp.lost_focus() {
                        ui.data_mut(|d| d.remove::<String>(id));
                        if text != e.value {
                            cmds.push(("cmake.cache.set", json!({ "name": e.name, "value": text })));
                        }
                    }
                }
                ui.end_row();
            }
        });
    });
}

fn tests_view(ui: &mut egui::Ui, s: &Session, st: &UiState, cmds: &mut Vec<(&'static str, Value)>) {
    let theme = &st.theme;
    ui.horizontal(|ui| {
        let running = s.cpp.cmake.ctest_job.and_then(|id| s.cpp.jobs.get(id)).is_some_and(|j| j.is_running());
        if ui.add_enabled(!running, egui::Button::new("⟳ CTest")).on_hover_text("ctest --show-only=json-v1").clicked() {
            cmds.push(("cmake.ctest.refresh", Value::Null));
        }
        ui.label(RichText::new(format!("{} CTest tests", s.cpp.cmake.ctest.len())).color(theme.text_muted));
        if let Some(e) = &s.cpp.cmake.ctest_error {
            ui.label(RichText::new(e).color(theme.error));
        }
    });
    let st_c = &s.cpp.cmake;
    let exes = codecraft_engine::cpp::cmake::test_executables(st_c.codemodel.as_ref(), &st_c.frameworks, &st_c.ctest);
    if exes.is_empty() {
        ui.label(
            RichText::new("No test executables: reload the CMake project (targets that include GoogleTest, Catch2, doctest or Boost.Test appear here).")
                .color(theme.text_muted),
        );
    }
    ScrollArea::vertical().id_salt("cmake-tests").auto_shrink([false, false]).show(ui, |ui| {
        for e in exes.iter().take(200) {
            let fw = e.framework;
            ui.horizontal(|ui| {
                ui.label(RichText::new(e.target.clone().unwrap_or_else(|| e.path.display().to_string())).strong())
                    .on_hover_text(e.path.display().to_string());
                ui.label(RichText::new(framework_label(fw)).color(theme.accent).size(11.0));
                if e.ctest_tests > 0 {
                    ui.label(RichText::new(format!("{} CTest", e.ctest_tests)).color(theme.text_muted).size(11.0));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(fw) = fw.and_then(Framework::cpp_key) {
                        if ui.small_button("▶ Run All").clicked() {
                            let mut p = json!({ "executable": e.path, "framework": fw });
                            if let Some(t) = &e.target {
                                p["target"] = json!(t);
                            }
                            cmds.push(("cpp.tests.run", p));
                        }
                        if ui.small_button("List Tests").clicked() {
                            cmds.push(("cpp.tests.list", json!({ "executable": e.path, "framework": fw, "refresh": true })));
                        }
                    }
                });
            });
            if let (Some(listing), Some(fw)) = (s.cpp.listings.get(&e.path), fw.and_then(Framework::cpp_key)) {
                ui.indent(("cmake-listing", e.path.display().to_string()), |ui| {
                    if listing.pending {
                        ui.label(RichText::new("Listing…").color(theme.text_muted));
                    }
                    if let Some(err) = &listing.error {
                        ui.label(RichText::new(err).color(theme.error));
                    }
                    for t in listing.tests.iter().take(1000) {
                        ui.horizontal(|ui| {
                            let label = match &t.suite {
                                Some(s) => format!("{s} › {}", t.name),
                                None => t.name.clone(),
                            };
                            if ui.small_button("▶").on_hover_text("Run this test").clicked() {
                                cmds.push((
                                    "cpp.tests.run",
                                    json!({ "executable": e.path, "framework": fw, "suite": t.suite, "test": t.name }),
                                ));
                            }
                            ui.label(label);
                        });
                    }
                });
            }
        }
    });
}

fn deps_view(ui: &mut egui::Ui, s: &Session, st: &UiState, cmds: &mut Vec<(&'static str, Value)>) {
    let theme = &st.theme;
    let Some(m) = &s.cpp.deps.manifests else {
        cmds.push(("cpp.dependencies", Value::Null));
        return;
    };
    if m.conan.is_none() && m.vcpkg.is_none() {
        ui.label(RichText::new("No conanfile.txt, conanfile.py or vcpkg.json in the project.").color(theme.text_muted));
    }
    ui.horizontal(|ui| {
        if m.conan.is_some() && ui.button("Install (Conan)").on_hover_text("conan install . --build=missing").clicked() {
            cmds.push(("cpp.dependencies.install", json!({ "manager": "conan" })));
        }
        if m.vcpkg.is_some() && ui.button("Install (vcpkg)").on_hover_text("vcpkg install").clicked() {
            cmds.push(("cpp.dependencies.install", json!({ "manager": "vcpkg" })));
        }
        if ui.button("⟳").on_hover_text("Read the manifests again").clicked() {
            cmds.push(("cpp.dependencies", Value::Null));
        }
    });
    for e in &m.errors {
        ui.label(RichText::new(e).color(theme.error));
    }
    egui::Grid::new("cpp-deps-grid").num_columns(4).striped(true).show(ui, |ui| {
        for (manager, p) in m.packages.iter().take(1000) {
            ui.label(RichText::new(&p.name).strong());
            ui.label(p.version.clone().unwrap_or_default());
            ui.label(RichText::new(&p.section).color(theme.text_muted));
            ui.label(RichText::new(format!("{manager:?}").to_ascii_lowercase()).color(theme.text_muted));
            ui.end_row();
        }
    });
    if let Some((_, id)) = s.cpp.deps.jobs.last()
        && let Some(job) = s.cpp.jobs.get(*id)
    {
        ui.separator();
        ui.label(RichText::new(format!("{} — {}", job.title, job_status_text(job.status))).color(theme.text_muted));
        lines_view(ui, st, "cpp-deps-log", &job.lines);
    }
}
