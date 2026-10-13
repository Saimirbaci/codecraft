//! The Inspection Results tool window, the Inspect Code / Code Cleanup scope dialog and the
//! Analyze Stack Trace windows. Thin: state lives in `Session::inspections`; every action is a
//! command.

use std::path::Path;

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, CornerRadius, FontId, Id, Layout, RichText, ScrollArea, Sense, Stroke, Ui, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::inspections::{self, Finding, JobKind, ResultTree as Tree, ScopeKind};
use codecraft_workspace::Severity;

use crate::theme::Theme;
use crate::{UiState, run_command};

fn severity_mark(theme: &Theme, s: Severity) -> (&'static str, Color32) {
    match s {
        Severity::Error => ("✖", theme.error),
        Severity::Warning => ("⚠", theme.warning),
        Severity::Info => ("ℹ", theme.weak),
        Severity::Hint => ("·", theme.weak),
    }
}

fn rel(s: &Session, path: &Path) -> String {
    match &s.ws.project {
        Some(p) => p.relative(path).into_owned(),
        None => path.display().to_string(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string()
}

// ---------------------------------------------------------------- tool window

/// The Inspection Results tool window: toolbar, the inspection → file → problem tree and the
/// description panel of the selected problem.
pub fn show(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    toolbar(ui, s, &theme, &mut cmds);
    ui.separator();
    if s.inspections.results.is_none() {
        if s.inspections.running.is_none() {
            ui.label(
                RichText::new("Run Code › Inspect Code… to check the project with Clippy and the installed linters.")
                    .color(theme.text_muted),
            );
        }
        // Fill the panel so it keeps its size while the first run is in progress.
        ui.allocate_space(ui.available_size());
        flush(s, st, cmds);
        return;
    }
    let tree = inspections::tree(&s.inspections);
    let width = ui.available_width();
    let height = ui.available_height();
    let tree_w = (width * 0.5).clamp(200.0, width.max(200.0));
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(tree_w, height), Layout::top_down(Align::Min), |ui| {
            ScrollArea::both().id_salt("inspection-tree").auto_shrink([false, false]).show(ui, |ui| {
                tree_view(ui, s, &theme, &tree, &mut cmds);
            });
        });
        ui.separator();
        ui.allocate_ui_with_layout(vec2(ui.available_width(), height), Layout::top_down(Align::Min), |ui| {
            ScrollArea::vertical().id_salt("inspection-description").auto_shrink([false, false]).show(ui, |ui| {
                description(ui, s, &theme, &mut cmds);
            });
        });
    });
    flush(s, st, cmds);
}

fn flush(s: &mut Session, st: &mut UiState, cmds: Vec<(&'static str, Value)>) {
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn toolbar(ui: &mut Ui, s: &Session, theme: &Theme, cmds: &mut Vec<(&'static str, Value)>) {
    ui.horizontal(|ui| {
        let running = s.inspections.running.is_some();
        if ui
            .add_enabled(!running && s.inspections.last.is_some(), egui::Button::new("▶").frame(false))
            .on_hover_text("Rerun Inspection")
            .clicked()
        {
            cmds.push(("inspections.rerun", Value::Null));
        }
        if ui.add_enabled(running, egui::Button::new("■").frame(false)).on_hover_text("Stop Inspection").clicked() {
            cmds.push(("inspections.cancel", Value::Null));
        }
        ui.separator();
        let counts = s.inspections.results.as_ref().map(|r| r.counts()).unwrap_or_default();
        for sev in [Severity::Error, Severity::Warning, Severity::Info] {
            let (mark, color) = severity_mark(theme, sev);
            let shown = !s.inspections.hidden.contains(&sev);
            let n = counts.get(&sev).copied().unwrap_or(0);
            let text = RichText::new(format!("{mark} {n}")).color(if shown { color } else { theme.text_disabled });
            let tip = format!("{} {}", if shown { "Hide" } else { "Show" }, plural_severity(sev));
            if ui.selectable_label(shown, text).on_hover_text(tip).clicked() {
                cmds.push(("inspections.filter", json!({ "severity": sev.label(), "show": !shown })));
            }
        }
        ui.separator();
        if let Some(r) = &s.inspections.running {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new(&r.progress).color(theme.text_muted).small());
        } else if let Some(r) = &s.inspections.results {
            let tools: Vec<String> = r.tools.iter().map(|t| format!("{}: {}", t.tool.label(), t.outcome)).collect();
            let label = format!("{} — {}", r.scope_label, if tools.is_empty() { "no linters ran".to_string() } else { tools.join(" · ") });
            let color = if r.tools.iter().all(|t| t.ok) { theme.text_muted } else { theme.warning };
            ui.label(RichText::new(label).color(color).small());
            if r.truncated {
                ui.label(RichText::new("(truncated)").color(theme.warning).small());
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("✕").on_hover_text("Close").clicked() {
                cmds.push(("inspections.close", Value::Null));
            }
            let fixable = s.inspections.results.as_ref().is_some_and(|r| r.open().any(|f| f.safe_fix().is_some()));
            if ui.add_enabled(fixable, egui::Button::new("Fix All")).on_hover_text("Apply every machine-applicable fix").clicked() {
                cmds.push(("inspections.fixAll", json!({})));
            }
        });
    });
}

fn plural_severity(s: Severity) -> &'static str {
    match s {
        Severity::Error => "errors",
        Severity::Warning => "warnings",
        Severity::Info => "weak warnings",
        Severity::Hint => "hints",
    }
}

fn tree_view(ui: &mut Ui, s: &Session, theme: &Theme, tree: &Tree, cmds: &mut Vec<(&'static str, Value)>) {
    let Some(results) = &s.inspections.results else { return };
    if tree.is_empty() {
        let msg = if results.open().next().is_none() { "No problems found." } else { "All problems are filtered out." };
        ui.label(RichText::new(msg).color(theme.text_muted));
        return;
    }
    for (name, files) in tree {
        let count: usize = files.iter().map(|(_, ids)| ids.len()).sum();
        let worst = files
            .iter()
            .flat_map(|(_, ids)| ids.iter())
            .filter_map(|id| results.finding(*id))
            .map(|f| f.severity)
            .min()
            .unwrap_or(Severity::Warning);
        let (mark, color) = severity_mark(theme, worst);
        let mut job = LayoutJob::default();
        job.append(&format!("{mark} "), 0.0, TextFormat { color, ..Default::default() });
        job.append(name, 0.0, TextFormat { color: theme.text, ..Default::default() });
        job.append(
            &format!("  {count} problem{}", if count == 1 { "" } else { "s" }),
            0.0,
            TextFormat { color: theme.text_muted, ..Default::default() },
        );
        egui::CollapsingHeader::new(job).id_salt(("inspection", name)).default_open(true).show(ui, |ui| {
            for (path, ids) in files {
                let label = format!("{}  {}", file_name(path), ids.len());
                egui::CollapsingHeader::new(RichText::new(label).color(theme.text))
                    .id_salt(("inspection-file", name, path))
                    .default_open(true)
                    .show(ui, |ui| {
                        for id in ids {
                            let Some(f) = results.finding(*id) else { continue };
                            occurrence_row(ui, s, theme, f, cmds);
                        }
                    })
                    .header_response
                    .on_hover_text(rel(s, path));
            }
        });
    }
}

fn occurrence_row(ui: &mut Ui, s: &Session, theme: &Theme, f: &Finding, cmds: &mut Vec<(&'static str, Value)>) {
    let selected = s.inspections.selected == Some(f.id);
    let (mark, color) = severity_mark(theme, f.severity);
    let text = format!("{}  ({}:{})", f.message, f.start.line + 1, f.start.column + 1);
    let row_h = 20.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(240.0), row_h), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(3), theme.list_selection);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(3), theme.hover);
    }
    let cy = rect.center().y;
    ui.painter().text(egui::pos2(rect.left() + 4.0, cy), egui::Align2::LEFT_CENTER, mark, FontId::proportional(12.0), color);
    let fix_mark = if f.fixes.is_empty() { "" } else { "  💡" };
    ui.painter().text(
        egui::pos2(rect.left() + 22.0, cy),
        egui::Align2::LEFT_CENTER,
        format!("{text}{fix_mark}"),
        FontId::proportional(12.5),
        theme.text,
    );
    let resp = resp.on_hover_text(&f.message);
    // Accessibility label so the tree can be driven headlessly.
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &text));
    if resp.double_clicked() {
        cmds.push(("inspections.navigate", json!({ "id": f.id })));
    } else if resp.clicked() {
        cmds.push(("inspections.select", json!({ "id": f.id })));
    }
    resp.context_menu(|ui| {
        if ui.button("Jump to Source").clicked() {
            cmds.push(("inspections.navigate", json!({ "id": f.id })));
            ui.close();
        }
        for (i, fix) in f.fixes.iter().enumerate() {
            if fix.applicability.is_offered() && ui.button(fix_label(fix)).clicked() {
                cmds.push(("inspections.fix", json!({ "id": f.id, "fix": i })));
                ui.close();
            }
        }
        if f.code.is_some() && ui.button(inspections::suppress_label(f)).clicked() {
            cmds.push(("inspections.suppress", json!({ "id": f.id })));
            ui.close();
        }
    });
}

fn fix_label(fix: &inspections::Fix) -> String {
    if fix.applicability.is_safe() { fix.title.clone() } else { format!("{} (may be incorrect)", fix.title) }
}

fn description(ui: &mut Ui, s: &Session, theme: &Theme, cmds: &mut Vec<(&'static str, Value)>) {
    let Some(f) = s.inspections.selected.and_then(|id| s.inspections.results.as_ref()?.finding(id)) else {
        ui.label(RichText::new("Select a problem to see its description.").color(theme.text_muted));
        return;
    };
    let (mark, color) = severity_mark(theme, f.severity);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(mark).color(color));
        ui.label(RichText::new(&f.inspection).strong());
        ui.label(RichText::new(format!("({})", f.tool.label())).color(theme.text_muted).small());
    });
    ui.label(&f.message);
    let loc = format!("{}:{}:{}", rel(s, &f.path), f.start.line + 1, f.start.column + 1);
    if ui.link(RichText::new(&loc).color(theme.link)).on_hover_text("Jump to Source (double-click the problem)").clicked() {
        cmds.push(("inspections.navigate", json!({ "id": f.id })));
    }
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        for (i, fix) in f.fixes.iter().enumerate() {
            if !fix.applicability.is_offered() {
                continue;
            }
            let b = egui::Button::new(RichText::new(format!("💡 {}", fix_label(fix))));
            if ui.add(b).clicked() {
                cmds.push(("inspections.fix", json!({ "id": f.id, "fix": i })));
            }
        }
        if f.safe_fix().is_some() {
            let others = s
                .inspections
                .results
                .as_ref()
                .map(|r| r.open().filter(|x| x.inspection == f.inspection && x.safe_fix().is_some()).count())
                .unwrap_or(0);
            if others > 1 && ui.button(format!("Fix all '{}' problems ({others})", f.inspection)).clicked() {
                cmds.push(("inspections.fixAll", json!({ "inspection": f.inspection })));
            }
        }
        if f.code.is_some() && ui.button(inspections::suppress_label(f)).clicked() {
            cmds.push(("inspections.suppress", json!({ "id": f.id })));
        }
    });
    if let Some(url) = inspections::finding_url(f)
        && ui.link(RichText::new("Open documentation ↗").color(theme.link)).on_hover_text(&url).clicked()
    {
        cmds.push(("inspections.openDocs", json!({ "id": f.id })));
    }
    ui.add_space(6.0);
    if !f.description.is_empty() {
        ui.label(RichText::new(&f.description).font(FontId::monospace(11.5)).color(theme.text_editor));
    }
}

// ---------------------------------------------------------------- windows

pub fn windows(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    scope_dialog(&ctx, s, st);
    stack_dialog(&ctx, s, st);
    stack_window(&ctx, s, st);
}

fn scope_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(d) = s.inspections.dialog.clone() else { return };
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let title = match d.kind {
        JobKind::Inspect => "Specify Inspection Scope",
        JobKind::Cleanup => "Specify Code Cleanup Scope",
    };
    let mut open = true;
    egui::Window::new(title)
        .id(Id::new("inspection-scope-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, -80.0))
        .show(ctx, |ui| {
            let project = s.ws.project.as_ref().map(|p| p.name.clone()).unwrap_or_default();
            let mut choice = |ui: &mut Ui, kind: ScopeKind, label: String, enabled: bool| {
                let r = ui.add_enabled(enabled, egui::RadioButton::new(d.scope == kind, label));
                if r.clicked() && d.scope != kind {
                    cmds.push(("inspections.dialog.set", json!({ "scope": kind.label() })));
                }
            };
            choice(ui, ScopeKind::Project, format!("Whole project '{project}'"), true);
            choice(ui, ScopeKind::Uncommitted, "Uncommitted files".into(), d.has_vcs);
            let file = d.file.as_deref().map(|p| rel(s, p)).unwrap_or_default();
            choice(ui, ScopeKind::File, format!("File '{file}'"), d.file.is_some());
            let dir = d.directory.as_deref().map(|p| rel(s, p)).unwrap_or_default();
            choice(ui, ScopeKind::Directory, format!("Directory '{dir}'"), d.directory.is_some());
            ui.add_space(4.0);
            let hint = match d.kind {
                JobKind::Inspect => "Runs Clippy (or cargo check) and the installed linters: ruff, ESLint, clang-tidy, zig fmt.",
                JobKind::Cleanup => "Applies every machine-applicable fix, then rustfmt / ruff format / zig fmt, in a preview.",
            };
            ui.label(RichText::new(hint).color(theme.text_muted).small());
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let ok = match d.kind {
                        JobKind::Inspect => "Analyze",
                        JobKind::Cleanup => "Run",
                    };
                    if ui.add(egui::Button::new(RichText::new(ok).strong()).fill(theme.accent)).clicked() {
                        cmds.push(("inspections.dialog.ok", Value::Null));
                    }
                    if ui.button("Cancel").clicked() {
                        cmds.push(("inspections.dialog.cancel", Value::Null));
                    }
                });
            });
            if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                cmds.push(("inspections.dialog.cancel", Value::Null));
            }
        });
    if !open {
        cmds.push(("inspections.dialog.cancel", Value::Null));
    }
    flush(s, st, cmds);
}

fn stack_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    if s.inspections.stack_dialog.is_none() {
        return;
    }
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut open = true;
    egui::Window::new("Analyze Stack Trace")
        .id(Id::new("stack-trace-dialog"))
        .open(&mut open)
        .collapsible(false)
        .default_size([640.0, 420.0])
        .show(ctx, |ui| {
            ui.label(RichText::new("Put a stack trace or a panic message here:").color(theme.text_muted));
            let Some(d) = s.inspections.stack_dialog.as_mut() else { return };
            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut d.text)
                        .id(Id::new("stack-trace-text"))
                        .font(FontId::monospace(12.0))
                        .desired_rows(16)
                        .desired_width(f32::INFINITY),
                );
            });
            let text = d.text.clone();
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(!text.trim().is_empty(), egui::Button::new(RichText::new("Analyze").strong()).fill(theme.accent))
                        .clicked()
                    {
                        cmds.push(("code.analyzeStackTraceOrThreadDump", json!({ "text": text })));
                    }
                    if ui.button("Cancel").clicked() {
                        cmds.push(("inspections.stackTrace.cancel", Value::Null));
                    }
                });
            });
        });
    if !open {
        cmds.push(("inspections.stackTrace.cancel", Value::Null));
    }
    flush(s, st, cmds);
}

fn stack_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(trace) = &s.inspections.stack_trace else { return };
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut open = true;
    let font = FontId::monospace(12.0);
    egui::Window::new(format!("{} — Analyzed", trace.language.label()))
        .id(Id::new("stack-trace-window"))
        .open(&mut open)
        .default_size([760.0, 440.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let links = trace.lines.iter().filter(|l| l.link.as_ref().is_some_and(|k| k.exists)).count();
                ui.label(RichText::new(format!("{} lines, {links} file link(s)", trace.lines.len())).color(theme.text_muted).small());
                if let Some(i) = trace.first_project_frame
                    && ui.small_button("Go to first project frame").clicked()
                {
                    cmds.push(("inspections.stackTrace.open", json!({ "line": i })));
                }
                if trace.truncated {
                    ui.label(RichText::new("(truncated)").color(theme.warning).small());
                }
            });
            ui.separator();
            ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
                for (i, line) in trace.lines.iter().enumerate() {
                    let base = if line.frame && !line.project { theme.text_disabled } else { theme.text_editor };
                    let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
                    match &line.link {
                        Some(link) if link.exists => {
                            let mut job = LayoutJob::default();
                            job.append(line.text.get(..link.start).unwrap_or(""), 0.0, fmt(base));
                            let c = if line.project { theme.link } else { theme.text_muted };
                            let mut lf = fmt(c);
                            lf.underline = Stroke::new(1.0, c);
                            job.append(line.text.get(link.start..link.end).unwrap_or(""), 0.0, lf);
                            job.append(line.text.get(link.end..).unwrap_or(""), 0.0, fmt(base));
                            let r = ui
                                .add(egui::Label::new(job).sense(Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .on_hover_text(link.path.display().to_string());
                            if r.clicked() {
                                cmds.push(("inspections.stackTrace.open", json!({ "line": i })));
                            }
                        }
                        _ => {
                            ui.label(RichText::new(&line.text).font(font.clone()).color(base));
                        }
                    }
                }
            });
        });
    if !open {
        cmds.push(("inspections.stackTrace.close", Value::Null));
    }
    flush(s, st, cmds);
}
