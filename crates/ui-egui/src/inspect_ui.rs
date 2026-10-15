//! M14 code analysis views (track CA): Settings › Editor › Inspections (profiles with a
//! searchable analyzer → rule tree, on/off and severity per rule, analyzer options), the
//! Problems tool window's File and Project tabs, and the commit check's warning. Thin: every
//! change is an `analysis.*` command; pure view state (the tab, the search text, unsaved option
//! text) lives in egui's memory.

use std::collections::BTreeSet;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, Id, RichText, ScrollArea, Sense, Ui};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::analysis::agent_cmds::{ProjectProblem, project_problems};
use codecraft_engine::analysis::profiles::{self, ANALYZERS, CLIPPY_GROUPS, InspectionProfile, Level, ProfileScope};
use codecraft_engine::inspections::Tool;

use crate::settings_ui::Cmds;
use crate::theme::Theme;
use crate::{UiState, run_command};

fn level_mark(theme: &Theme, l: Level) -> (&'static str, Color32) {
    match l {
        Level::Error => ("✖", theme.error),
        Level::Warning => ("⚠", theme.warning),
        Level::WeakWarning => ("ℹ", theme.weak),
        Level::Info => ("·", theme.weak),
    }
}

/// A severity chooser: `Default` (the analyzer's own) or a level. Returns the new choice.
fn severity_combo(ui: &mut Ui, id: Id, current: Option<Level>) -> Option<Option<Level>> {
    let mut out = None;
    let text = current.map_or("Default", Level::label);
    egui::ComboBox::from_id_salt(id).selected_text(text).width(110.0).show_ui(ui, |ui| {
        if ui.selectable_label(current.is_none(), "Default").clicked() && current.is_some() {
            out = Some(None);
        }
        for l in Level::ALL {
            if ui.selectable_label(current == Some(l), l.label()).clicked() && current != Some(l) {
                out = Some(Some(l));
            }
        }
    });
    out
}

fn level_json(l: Option<Level>) -> Value {
    l.map_or(Value::Null, |l| json!(l.id()))
}

// ---------------------------------------------------------------- Settings › Editor › Inspections

/// Analyzer availability, looked up once per visit of the page (Rescan clears it).
fn availability(ui: &Ui, s: &mut Session) -> std::collections::BTreeMap<String, bool> {
    let id = Id::new("inspections-settings-available");
    if let Some(m) = ui.data(|d| d.get_temp::<std::collections::BTreeMap<String, bool>>(id)) {
        return m;
    }
    let mut m = std::collections::BTreeMap::new();
    if let Ok(v) = s.execute("analysis.tools", &json!({})) {
        for t in v["analyzers"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
            if let (Some(id), Some(a)) = (t["id"].as_str(), t["available"].as_bool()) {
                m.insert(id.to_string(), a);
            }
        }
    }
    ui.data_mut(|d| d.insert_temp(id, m.clone()));
    m
}

fn temp_string(ui: &Ui, key: &str, init: impl FnOnce() -> String) -> String {
    let id = Id::new(("inspections-settings-text", key));
    ui.data(|d| d.get_temp::<String>(id)).unwrap_or_else(init)
}

fn set_temp_string(ui: &Ui, key: &str, v: String) {
    ui.data_mut(|d| d.insert_temp(Id::new(("inspections-settings-text", key)), v));
}

/// The rules shown under an analyzer: well-known ones, the profile's and the last results'.
fn rules_for(s: &Session, p: &InspectionProfile, tool: Tool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = profiles::known_rules(tool).into_iter().map(|(n, d)| (n.to_string(), d.to_string())).collect();
    let fam = format!("{}:", profiles::family(tool));
    for k in p.rules.keys() {
        if let Some(rule) = k.strip_prefix(&fam)
            && !out.iter().any(|(n, _)| n == rule)
        {
            out.push((rule.to_string(), String::new()));
        }
    }
    if let Some(r) = &s.inspections.results {
        let seen: BTreeSet<String> = r.findings.iter().filter(|f| f.tool == tool).map(|f| f.rule().to_string()).collect();
        for rule in seen.into_iter().take(500) {
            if !out.iter().any(|(n, _)| *n == rule) {
                out.push((rule, "Reported by the last analysis".into()));
            }
        }
    }
    out
}

pub fn settings_page(ui: &mut Ui, s: &mut Session, st: &mut UiState, cmds: &mut Cmds) {
    let theme = st.theme.clone();
    profiles::ensure_loaded(s);
    let list = s.analysis.profiles.list();
    let active = s.analysis.profiles.active.clone();
    let Some((profile, scope, modified)) = list.iter().find(|(p, _, _)| p.name == active).cloned() else { return };
    let available = availability(ui, s);

    ui.horizontal(|ui| {
        ui.label("Profile:");
        egui::ComboBox::from_id_salt("inspections-profile").selected_text(&active).width(180.0).show_ui(ui, |ui| {
            for (p, sc, _) in &list {
                let tag = match sc {
                    ProfileScope::Builtin => "",
                    ProfileScope::Ide => "  (IDE)",
                    ProfileScope::Project => "  (Project)",
                };
                if ui.selectable_label(p.name == active, format!("{}{tag}", p.name)).clicked() && p.name != active {
                    cmds.push(("analysis.profile.select", json!({ "name": p.name })));
                }
            }
        });
        let where_ = match (scope, modified) {
            (ProfileScope::Builtin, _) => "built in",
            (ProfileScope::Project, true) => "built in, changed for this project",
            (ProfileScope::Project, false) => "stored in the project",
            (ProfileScope::Ide, _) => "stored in the IDE",
        };
        ui.label(RichText::new(where_).color(theme.text_muted).small());
        if modified && ui.button("Reset").on_hover_text("Drop this project's changes to the built-in profile").clicked() {
            cmds.push(("analysis.profile.delete", json!({ "name": active })));
        }
        if scope != ProfileScope::Builtin && !modified && ui.button("Delete").clicked() {
            cmds.push(("analysis.profile.delete", json!({ "name": active })));
        }
    });
    ui.horizontal(|ui| {
        let mut name = temp_string(ui, "copy-name", || format!("{active} copy"));
        let r = ui.add(egui::TextEdit::singleline(&mut name).desired_width(160.0).id(Id::new("inspections-copy-name")));
        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "New profile name"));
        set_temp_string(ui, "copy-name", name.clone());
        if ui.button("Copy to Project").clicked() {
            cmds.push(("analysis.profile.copy", json!({ "from": active, "name": name, "scope": "project" })));
            cmds.push(("analysis.profile.select", json!({ "name": name })));
        }
        if ui.button("Copy to IDE").on_hover_text("Available in every project").clicked() {
            cmds.push(("analysis.profile.copy", json!({ "from": active, "name": name, "scope": "ide" })));
        }
    });
    if let Some(e) = &s.analysis.profiles.error {
        ui.label(RichText::new(e).color(theme.warning).small());
    }
    ui.separator();
    let mut query = temp_string(ui, "query", String::new);
    ui.horizontal(|ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut query).hint_text("Search analyzers and rules").desired_width(260.0));
        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, "Search inspections"));
        if ui.small_button("Rescan tools").clicked() {
            ui.data_mut(|d| d.remove::<std::collections::BTreeMap<String, bool>>(Id::new("inspections-settings-available")));
        }
    });
    set_temp_string(ui, "query", query.clone());
    let q = query.trim().to_lowercase();
    let mut commit_check = s.analysis.profiles.commit_check;

    ScrollArea::vertical().id_salt("inspections-tree").auto_shrink([false, false]).max_height((ui.available_height() - 40.0).max(120.0)).show(ui, |ui| {
        for (language, tools) in ANALYZERS {
            let rows: Vec<(Tool, Vec<(String, String)>)> = tools
                .iter()
                .map(|t| {
                    let rules: Vec<(String, String)> = rules_for(s, &profile, *t)
                        .into_iter()
                        .filter(|(n, d)| {
                            q.is_empty()
                                || n.to_lowercase().contains(&q)
                                || d.to_lowercase().contains(&q)
                                || t.label().to_lowercase().contains(&q)
                                || language.to_lowercase().contains(&q)
                        })
                        .collect();
                    (*t, rules)
                })
                .filter(|(t, rules)| q.is_empty() || !rules.is_empty() || t.label().to_lowercase().contains(&q) || language.to_lowercase().contains(&q))
                .collect();
            if rows.is_empty() {
                continue;
            }
            egui::CollapsingHeader::new(RichText::new(language.to_string()).strong())
                .id_salt(("inspections-lang", language.to_string()))
                .default_open(!q.is_empty())
                .open((!q.is_empty()).then_some(true))
                .show(ui, |ui| {
                    for (tool, rules) in rows {
                        analyzer_node(ui, s, &theme, &profile, tool, &rules, available.get(tool.id()).copied(), !q.is_empty(), cmds);
                    }
                });
        }
    });
    ui.separator();
    if ui
        .checkbox(&mut commit_check, "Analyze code before commit")
        .on_hover_text("The Commit tool window inspects the files being committed with this profile first")
        .changed()
    {
        cmds.push(("vcs.commit.analyzeCode", json!({ "enabled": commit_check })));
    }
}

#[allow(clippy::too_many_arguments)]
fn analyzer_node(
    ui: &mut Ui,
    s: &Session,
    theme: &Theme,
    p: &InspectionProfile,
    tool: Tool,
    rules: &[(String, String)],
    available: Option<bool>,
    searching: bool,
    cmds: &mut Cmds,
) {
    let on = match tool {
        Tool::Clippy => p.clippy_enabled(),
        Tool::CargoCheck => p.analyzers.get("cargo_check").is_none_or(|t| t.enabled),
        t => p.analyzer_enabled(t),
    };
    let sev = p.analyzers.get(tool.id()).and_then(|t| t.severity);
    let id = ui.make_persistent_id(("inspections-analyzer", tool.id()));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    // A search shows the matching rules.
    if searching && !rules.is_empty() {
        state.set_open(true);
    }
    state
        .show_header(ui, |ui| {
            let mut enabled = on;
            let r = ui.checkbox(&mut enabled, tool.label());
            r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, enabled, format!("Analyzer {}", tool.label())));
            if r.changed() {
                cmds.push(("analysis.profile.setAnalyzer", json!({ "analyzer": tool.id(), "enabled": enabled })));
            }
            if let Some(new) = severity_combo(ui, Id::new(("inspections-analyzer-sev", tool.id())), sev) {
                cmds.push(("analysis.profile.setAnalyzer", json!({ "analyzer": tool.id(), "severity": level_json(new) })));
            }
            match available {
                Some(false) => {
                    ui.label(RichText::new("not installed").color(theme.text_muted).small());
                    if tool == Tool::Pyright && s.analysis.install.running.is_none() && ui.small_button("Install basedpyright").clicked() {
                        cmds.push(("analysis.installTool", json!({ "tool": "basedpyright", "confirm": true })));
                    }
                }
                _ if tool == Tool::Sql => {
                    ui.label(RichText::new("built in").color(theme.text_muted).small());
                }
                _ => {}
            }
        })
        .body(|ui| {
            analyzer_options(ui, p, tool, cmds);
            for (rule, desc) in rules.iter().take(400) {
                let t = p.rule(tool, rule);
                ui.horizontal(|ui| {
                    let mut enabled = t.is_none_or(|t| t.enabled);
                    let r = ui.checkbox(&mut enabled, RichText::new(rule).monospace());
                    r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, true, enabled, format!("Rule {rule}")));
                    if r.changed() {
                        cmds.push(("analysis.profile.setRule", json!({ "analyzer": tool.id(), "rule": rule, "enabled": enabled })));
                    }
                    if let Some(new) = severity_combo(ui, Id::new(("inspections-rule-sev", tool.id(), rule)), t.and_then(|t| t.severity)) {
                        cmds.push(("analysis.profile.setRule", json!({ "analyzer": tool.id(), "rule": rule, "severity": level_json(new) })));
                    }
                    if t.is_some() && ui.small_button("↺").on_hover_text("Back to the analyzer's default").clicked() {
                        cmds.push(("analysis.profile.setRule", json!({ "analyzer": tool.id(), "rule": rule, "reset": true })));
                    }
                    if !desc.is_empty() {
                        ui.label(RichText::new(desc).color(theme.text_muted).small());
                    }
                });
            }
            // Any other rule by name.
            ui.horizontal(|ui| {
                let key = format!("add-rule-{}", tool.id());
                let mut name = temp_string(ui, &key, String::new);
                let r = ui.add(egui::TextEdit::singleline(&mut name).hint_text("rule id").desired_width(200.0));
                r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, format!("Add rule to {}", tool.label())));
                if ui.small_button("Turn on").clicked() && !name.trim().is_empty() {
                    cmds.push(("analysis.profile.setRule", json!({ "analyzer": tool.id(), "rule": name.trim(), "enabled": true })));
                    name.clear();
                }
                if ui.small_button("Turn off").clicked() && !name.trim().is_empty() {
                    cmds.push(("analysis.profile.setRule", json!({ "analyzer": tool.id(), "rule": name.trim(), "enabled": false })));
                    name.clear();
                }
                set_temp_string(ui, &key, name);
            });
        });
}

/// The analyzer's own options: Clippy lint groups, Ruff selectors, analyzer checkers, …
fn analyzer_options(ui: &mut Ui, p: &InspectionProfile, tool: Tool, cmds: &mut Cmds) {
    let text_option = |ui: &mut Ui, cmds: &mut Cmds, option: &str, label: &str, hint: &str, current: String| {
        ui.horizontal(|ui| {
            ui.label(label);
            let key = format!("opt-{}-{option}", p.name);
            let mut v = temp_string(ui, &key, || current.clone());
            let r = ui.add(egui::TextEdit::singleline(&mut v).hint_text(hint).desired_width(240.0));
            r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, label));
            let apply = ui.add_enabled(v != current, egui::Button::new("Apply")).clicked()
                || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && v != current);
            if apply {
                cmds.push(("analysis.profile.setOption", json!({ "option": option, "value": v })));
            }
            set_temp_string(ui, &key, v);
        });
    };
    match tool {
        Tool::Clippy => {
            ui.horizontal(|ui| {
                ui.label("Lint groups:");
                for g in CLIPPY_GROUPS {
                    let mut on = p.clippy_groups.contains(g);
                    if ui.checkbox(&mut on, g).changed() {
                        let mut groups: Vec<String> = p.clippy_groups.iter().cloned().collect();
                        if on {
                            groups.push(g.to_string());
                        } else {
                            groups.retain(|x| x != g);
                        }
                        cmds.push(("analysis.profile.setOption", json!({ "option": "clippyGroups", "value": groups })));
                    }
                }
            });
        }
        Tool::Ruff => text_option(ui, cmds, "ruffSelect", "Extra rules:", "B, UP, SIM", p.ruff_select.join(", ")),
        Tool::ClangAnalyzer => text_option(ui, cmds, "clangCheckers", "Extra checkers:", "security, optin.cplusplus.UninitializedObject", p.clang_checkers.join(", ")),
        Tool::ClangTidy => text_option(ui, cmds, "clangTidyChecks", "Checks:", "-*,bugprone-* (empty: .clang-tidy)", p.clang_tidy_checks.clone()),
        Tool::Cppcheck => text_option(ui, cmds, "cppcheckEnable", "Enable:", "warning,style,performance,portability", p.cppcheck_enable.clone()),
        _ => {}
    }
}

// ---------------------------------------------------------------- Problems tool window

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProblemsTab {
    File,
    Project,
}

/// The Problems tool window: the current file's problems, or the whole project's (live
/// language-server and build problems plus the last analysis).
pub fn problems_view(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let tab_id = Id::new("problems-tab");
    let mut tab = ui.data(|d| d.get_temp::<u8>(tab_id)).map_or(ProblemsTab::File, |t| if t == 1 { ProblemsTab::Project } else { ProblemsTab::File });
    let active = s.ws.active().and_then(|(_, d)| d.path.clone());
    let file_list = active.as_deref().map(|p| project_problems(s, Some(p))).unwrap_or_default();
    let project_list = project_problems(s, None);
    ui.horizontal(|ui| {
        let file_label = match &active {
            Some(p) => format!("File: {}  {}", p.file_name().and_then(|n| n.to_str()).unwrap_or(""), file_list.len()),
            None => "File".to_string(),
        };
        if ui.selectable_label(tab == ProblemsTab::File, file_label).clicked() {
            tab = ProblemsTab::File;
        }
        if ui.selectable_label(tab == ProblemsTab::Project, format!("Project  {}", project_list.len())).clicked() {
            tab = ProblemsTab::Project;
        }
        if tab == ProblemsTab::Project {
            ui.separator();
            let analyzing = s.inspections.running.is_some();
            if analyzing {
                ui.add(egui::Spinner::new().size(12.0));
            }
            if ui.add_enabled(!analyzing && s.ws.project.is_some(), egui::Button::new("Analyze Project")).on_hover_text("Inspect Code on the whole project").clicked() {
                run_command(s, st, "code.inspectCode", &json!({ "scope": "project" }));
            }
            if let Some(r) = &s.inspections.results {
                ui.label(RichText::new(format!("analysis: {}", r.scope_label)).color(theme.text_muted).small());
            }
        }
    });
    ui.data_mut(|d| d.insert_temp(tab_id, if tab == ProblemsTab::Project { 1u8 } else { 0u8 }));
    ui.separator();
    let list = if tab == ProblemsTab::File { file_list } else { project_list };
    if list.is_empty() {
        let msg = match (tab, &active) {
            (ProblemsTab::File, None) => "No file is open",
            _ => "No problems found",
        };
        ui.label(RichText::new(msg).color(theme.text_muted));
        return;
    }
    let mut open: Option<(ProjectProblem, bool)> = None;
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // Grouped by file (sorted by path), errors first inside each file.
        let mut files: Vec<&str> = list.iter().map(|p| p.path.as_str()).collect::<BTreeSet<_>>().into_iter().collect();
        files.sort_by_key(|f| list.iter().filter(|p| p.path == *f).map(|p| p.severity).min());
        for file in files {
            let items: Vec<&ProjectProblem> = list.iter().filter(|p| p.path == file).collect();
            egui::CollapsingHeader::new(RichText::new(format!("{file}   {} problem(s)", items.len())).strong())
                .id_salt(("problems-file", file))
                .default_open(true)
                .show(ui, |ui| {
                    for p in items.into_iter().take(500) {
                        let (mark, color) = level_mark(&theme, p.severity);
                        let mut job = LayoutJob::default();
                        job.append(mark, 0.0, TextFormat { color, ..Default::default() });
                        let msg: String = p.message.chars().take(300).collect();
                        job.append(&format!("  {msg}"), 0.0, TextFormat { color: theme.text, ..Default::default() });
                        let origin = match (p.analyzer, &p.rule) {
                            (Some(a), Some(r)) => format!("  {a}: {r}"),
                            (None, Some(r)) => format!("  {r}"),
                            _ => String::new(),
                        };
                        job.append(
                            &format!("  :{}:{}{origin}", p.line, p.column),
                            0.0,
                            TextFormat { color: theme.text_muted, ..Default::default() },
                        );
                        let r = ui.add(egui::Label::new(job).sense(Sense::click())).on_hover_text(&p.message);
                        let label = format!("{msg} :{}:{}", p.line, p.column);
                        r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                        if r.clicked() {
                            open = Some(((*p).clone(), false));
                        }
                        if p.id.is_some() {
                            r.context_menu(|ui| {
                                if ui.button("Show in Inspection Results").clicked() {
                                    open = Some(((*p).clone(), true));
                                    ui.close();
                                }
                            });
                        }
                    }
                });
        }
    });
    if let Some((p, in_results)) = open {
        match (in_results, p.id) {
            (true, Some(id)) => {
                run_command(s, st, "inspections.select", &json!({ "id": id }));
                if !s.ws.layout.is_visible(codecraft_workspace::ToolWindowId::InspectionResults) {
                    run_command(s, st, "view.toolWindows.inspectionResults", &Value::Null);
                }
            }
            _ => run_command(
                s,
                st,
                "navigate.openLocation",
                &json!({ "path": p.abs, "line": p.line.saturating_sub(1), "column": p.column.saturating_sub(1) }),
            ),
        }
    }
}

// ---------------------------------------------------------------- the commit check

/// The commit check's progress and warning (Review / Commit Anyway / Cancel).
pub fn windows(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    if let Some(w) = s.analysis.commit.warning.clone() {
        let mut open = true;
        egui::Window::new("Code Analysis")
            .id(Id::new("commit-check-warning"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, -60.0))
            .show(ctx, |ui| {
                ui.label(format!(
                    "Code analysis found {} error(s) and {} warning(s) in {} file(s).",
                    w.errors, w.warnings, w.files
                ));
                ui.label(RichText::new("Review them before committing?").color(theme.text_muted));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Review").strong()).fill(theme.accent)).clicked() {
                        cmds.push(("analysis.commitCheck.review", Value::Null));
                    }
                    if ui.button("Commit Anyway").clicked() {
                        cmds.push(("analysis.commitCheck.commitAnyway", Value::Null));
                    }
                    if ui.button("Cancel").clicked() {
                        cmds.push(("analysis.commitCheck.cancel", Value::Null));
                    }
                });
            });
        if !open {
            cmds.push(("analysis.commitCheck.cancel", Value::Null));
        }
    } else if s.analysis.commit.pending.is_some() {
        egui::Window::new("Analyzing Code")
            .id(Id::new("commit-check-running"))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -40.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(12.0));
                    let progress = s.inspections.running.as_ref().map(|r| r.progress.clone()).unwrap_or_default();
                    ui.label(RichText::new(format!("Analyzing the files to commit… {progress}")).color(theme.text_muted).small());
                    if ui.small_button("Cancel").clicked() {
                        cmds.push(("inspections.cancel", Value::Null));
                        cmds.push(("analysis.commitCheck.cancel", Value::Null));
                    }
                });
            });
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}
