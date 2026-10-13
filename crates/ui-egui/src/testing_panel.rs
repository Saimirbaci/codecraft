//! Test runner UI (track T): the test tree beside the Run console (RustRover-style: toolbar, tree
//! on the left, the selected test's output on the right), the Test History and Manage Coverage
//! Reports dialogs, the Coverage tool window and the editor gutter's test/coverage marks.
//! Every action is an engine command (`test.*`, `coverage.*`, `run.*`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use egui::{Align, Align2, Color32, CornerRadius, FontId, Id, Layout, RichText, ScrollArea, Sense, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::testing::{self, FileCoverage, RowKind, TestStatus, TreeRow};
use codecraft_workspace::RunTabId;

use crate::theme::Theme;
use crate::{UiState, run_command};

fn status_mark(theme: &Theme, status: TestStatus) -> (&'static str, Color32) {
    match status {
        TestStatus::Passed => ("✓", theme.success),
        TestStatus::Failed => ("✖", theme.error),
        TestStatus::Ignored => ("⊘", theme.text_muted),
        TestStatus::Running => ("●", theme.warning),
    }
}

/// `12 ms`, `1.5 s`, `2 m 5 s`
fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} m {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// A glyph button; its accessible name is the tooltip (`Show Passed`), not the glyph.
fn toolbar_button(ui: &mut egui::Ui, text: &str, tip: &str, enabled: bool, selected: bool) -> bool {
    let resp = ui.add_enabled(enabled, egui::Button::new(text).selected(selected).min_size(vec2(24.0, 20.0)));
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Button, enabled, selected, tip));
    resp.on_hover_text(tip).clicked()
}

/// The test runner of a Run tab. Returns true when the shell should still show the console in
/// the remaining space (nothing, or the root, is selected).
pub fn show_test_run(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, tab: RunTabId) -> bool {
    let theme = st.theme.clone();
    let Some(run) = s.testing.run(tab) else { return true };
    let finished = run.finished;
    let counts = run.counts();
    let can_rerun = run.origin.is_some();
    let duration = run.duration_ms;
    let options = s.testing.options;
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let t = json!({ "tab": tab.0 });

    // ------------------------------------------------------------ toolbar
    ui.horizontal(|ui| {
        if toolbar_button(ui, "↻✖", "Rerun Failed Tests", finished && counts.failed > 0 && can_rerun, false) {
            cmds.push(("test.rerunFailed", t.clone()));
        }
        ui.separator();
        if toolbar_button(ui, "✓", "Show Passed", true, options.show_passed) {
            cmds.push(("test.toggleShowPassed", Value::Null));
        }
        if toolbar_button(ui, "⊘", "Show Ignored", true, options.show_ignored) {
            cmds.push(("test.toggleShowIgnored", Value::Null));
        }
        if toolbar_button(ui, "⏱", "Sort by Duration", true, options.sort_by_duration) {
            cmds.push(("test.toggleSortByDuration", Value::Null));
        }
        ui.separator();
        if toolbar_button(ui, "⊞", "Expand All", true, false) {
            cmds.push(("test.expandAll", t.clone()));
        }
        if toolbar_button(ui, "⊟", "Collapse All", true, false) {
            cmds.push(("test.collapseAll", t.clone()));
        }
        if toolbar_button(ui, "↑", "Previous Failed Test", counts.failed > 0, false) {
            cmds.push(("test.previousFailed", t.clone()));
        }
        if toolbar_button(ui, "↓", "Next Failed Test", counts.failed > 0, false) {
            cmds.push(("test.nextFailed", t.clone()));
        }
        ui.separator();
        if toolbar_button(ui, "🕘", "Test History", true, false) {
            cmds.push(("run.testHistory", Value::Null));
        }
        if toolbar_button(ui, "⤓", "Import Tests from File…", true, false) {
            cmds.push(("run.importTestsFromFile", Value::Null));
        }
        if toolbar_button(ui, "⤒", "Export Test Results…", finished, false)
            && let Some(path) = rfd::FileDialog::new()
                .set_title("Export Test Results")
                .add_filter("JSON", &["json"])
                .set_file_name("test-results.json")
                .save_file()
        {
            cmds.push(("test.export", json!({ "tab": tab.0, "path": path })));
        }
        ui.separator();
        let (mark, color) = if !finished {
            ("●", theme.warning)
        } else if counts.failed > 0 {
            ("✖", theme.error)
        } else {
            ("✓", theme.success)
        };
        let mut text = if counts.failed > 0 {
            format!("Tests failed: {}, passed: {}", counts.failed, counts.passed)
        } else {
            format!("Tests passed: {}", counts.passed)
        };
        if counts.ignored > 0 {
            text.push_str(&format!(", ignored: {}", counts.ignored));
        }
        text.push_str(&format!(" of {} tests", counts.total()));
        if let Some(d) = duration {
            text.push_str(&format!(" – {}", format_duration(d)));
        }
        ui.label(RichText::new(mark).color(color));
        ui.label(RichText::new(text).color(if counts.failed > 0 { theme.error } else { theme.text }));
    });

    // ------------------------------------------------------------ tree
    let rows: Vec<TreeRow> = s.testing.run(tab).map(|r| testing::tree_rows(r, options, true)).unwrap_or_default();
    let selected = s.testing.run(tab).and_then(|r| r.view.selected.clone());
    let row_h = 20.0;
    let width = ui.available_width();
    egui::Panel::left(Id::new(("test-tree", tab.0)))
        .resizable(true)
        .default_size((width * 0.4).clamp(200.0, 480.0))
        .size_range(160.0..=(width * 0.8).max(200.0))
        .frame(egui::Frame::new().fill(theme.panel_bg))
        .show(ui, |ui| {
            ScrollArea::both().id_salt(("test-tree-scroll", tab.0)).auto_shrink([false, false]).show_rows(
                ui,
                row_h,
                rows.len(),
                |ui, range| {
                    for row in rows.get(range).unwrap_or(&[]) {
                        tree_row(ui, &theme, row, selected.as_deref() == Some(row.key.as_str()), tab, &mut cmds);
                    }
                },
            );
        });

    // ------------------------------------------------------------ output of the selection
    let output = s.testing.run(tab).and_then(|r| selected.as_deref().and_then(|k| testing::node_output(r, k)));
    let failure = selected
        .as_deref()
        .and_then(testing::test_key)
        .and_then(|(si, ti)| s.testing.run(tab).and_then(|r| r.test(si, ti)))
        .and_then(|t| t.failure.clone().filter(|f| f.path.is_absolute()));
    let show_console = output.is_none();
    if let Some(text) = output {
        if let Some(f) = failure {
            let label = format!("at {}:{}", f.path.display(), f.line + 1);
            if ui.link(RichText::new(label).color(theme.link)).clicked() {
                cmds.push(("navigate.openLocation", json!({ "path": f.path, "line": f.line, "column": f.column })));
            }
        }
        let font = FontId::monospace((s.ws.settings.editor_font_size - 1.0).max(9.0));
        egui::Frame::new().fill(theme.editor_bg).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
            ScrollArea::both().id_salt(("test-output", tab.0)).auto_shrink([false, false]).show(ui, |ui| {
                for line in text.lines() {
                    let color = if line.contains("panicked at") || line.starts_with("E ") {
                        theme.console_stderr
                    } else {
                        theme.text_editor
                    };
                    ui.label(RichText::new(line).font(font.clone()).color(color));
                }
            });
        });
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
    show_console
}

fn tree_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    row: &TreeRow,
    selected: bool,
    tab: RunTabId,
    cmds: &mut Vec<(&'static str, Value)>,
) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(200.0), 20.0), Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &row.label));
    if selected {
        ui.painter().rect_filled(rect, CornerRadius::same(4), theme.list_selection);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, CornerRadius::same(4), theme.hover);
    }
    let x0 = rect.left() + 4.0 + row.depth as f32 * 14.0;
    let cy = rect.center().y;
    if row.has_children {
        let arrow = if row.expanded { "⏷" } else { "⏵" };
        ui.painter().text(
            egui::pos2(x0 + 5.0, cy),
            Align2::CENTER_CENTER,
            arrow,
            FontId::proportional(10.0),
            theme.text_muted,
        );
    }
    let (mark, color) = status_mark(theme, row.status);
    ui.painter().text(egui::pos2(x0 + 18.0, cy), Align2::CENTER_CENTER, mark, FontId::proportional(12.0), color);
    let label_color =
        if row.kind == RowKind::Test && row.status == TestStatus::Ignored { theme.text_muted } else { theme.text };
    ui.painter().text(
        egui::pos2(x0 + 28.0, cy),
        Align2::LEFT_CENTER,
        &row.label,
        FontId::proportional(13.0),
        label_color,
    );
    if let Some(d) = row.duration_ms {
        ui.painter().text(
            egui::pos2(rect.right() - 6.0, cy),
            Align2::RIGHT_CENTER,
            format_duration(d),
            FontId::proportional(11.0),
            theme.text_muted,
        );
    }
    let key = row.key.clone();
    if resp.clicked() {
        let on_arrow = resp.interact_pointer_pos().is_some_and(|p| p.x < x0 + 12.0);
        if row.has_children && on_arrow {
            cmds.push(("test.toggleNode", json!({ "tab": tab.0, "key": key })));
        } else {
            cmds.push(("test.select", json!({ "tab": tab.0, "key": key })));
        }
    }
    if resp.double_clicked() {
        if row.kind == RowKind::Test {
            cmds.push(("test.navigate", json!({ "tab": tab.0, "key": key })));
        } else if row.has_children {
            cmds.push(("test.toggleNode", json!({ "tab": tab.0, "key": key })));
        }
    }
    resp.context_menu(|ui| {
        if row.kind == RowKind::Test && ui.button("Jump to Source").clicked() {
            cmds.push(("test.navigate", json!({ "tab": tab.0, "key": key })));
            ui.close();
        }
        if ui.button("Rerun Failed Tests").clicked() {
            cmds.push(("test.rerunFailed", json!({ "tab": tab.0 })));
            ui.close();
        }
        if ui.button("Expand All").clicked() {
            cmds.push(("test.expandAll", json!({ "tab": tab.0 })));
            ui.close();
        }
        if ui.button("Collapse All").clicked() {
            cmds.push(("test.collapseAll", json!({ "tab": tab.0 })));
            ui.close();
        }
    });
}

// ---------------------------------------------------------------- dialogs

/// Floating windows of the test runner (Test History, Manage Coverage Reports) and the file
/// picker for `Import Tests from File…`. Called once per frame by the shell.
pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if s.testing.import_requested {
        s.testing.import_requested = false;
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Import Tests from File")
            .add_filter("Test results", &["xml", "json"])
            .pick_file()
        {
            run_command(s, st, "run.importTestsFromFile", &json!({ "path": path }));
        }
    }
    if s.testing.history_open {
        history_window(ui, s, st);
    }
    if s.testing.reports_open {
        reports_window(ui, s, st);
    }
}

fn unix_label(secs: u64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(secs);
    let ago = now.saturating_sub(secs);
    if ago < 60 {
        "just now".into()
    } else if ago < 3600 {
        format!("{} min ago", ago / 60)
    } else if ago < 86_400 {
        format!("{} h ago", ago / 3600)
    } else {
        format!("{} days ago", ago / 86_400)
    }
}

fn history_window(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let list = s.project_root().map(|r| testing::history_summaries(&r)).unwrap_or_default();
    let mut open = true;
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    crate::windows::resizable(egui::Window::new("Test History").default_size([560.0, 360.0]), st, "test-history")
        .id(Id::new("test-history"))
        .open(&mut open)
        .collapsible(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
        .show(ui.ctx(), |ui| {
            if list.is_empty() {
                ui.label(RichText::new("No test runs recorded for this project yet.").color(theme.text_muted));
            }
            ScrollArea::vertical().auto_shrink([false, false]).max_height(300.0).show(ui, |ui| {
                for h in &list {
                    let (mark, color) = if h.counts.failed > 0 {
                        status_mark(&theme, TestStatus::Failed)
                    } else {
                        status_mark(&theme, TestStatus::Passed)
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(mark).color(color));
                        let text = format!(
                            "{}  ·  {} passed, {} failed  ·  {}",
                            h.title,
                            h.counts.passed,
                            h.counts.failed,
                            unix_label(h.started_unix)
                        );
                        if ui.selectable_label(false, text).on_hover_text(&h.command_line).clicked() {
                            cmds.push(("test.openHistory", json!({ "index": h.index })));
                        }
                    });
                }
            });
            ui.separator();
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Import from File…").clicked() {
                    cmds.push(("run.importTestsFromFile", Value::Null));
                }
                if ui.add_enabled(!list.is_empty(), egui::Button::new("Clear History")).clicked() {
                    cmds.push(("test.clearHistory", Value::Null));
                }
            });
        });
    if !open {
        s.testing.history_open = false;
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn reports_window(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let list = s.project_root().map(|r| testing::list_reports(&r)).unwrap_or_default();
    let active = s.testing.coverage.as_ref().map(|c| c.id.clone());
    let mut open = true;
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    crate::windows::resizable(
        egui::Window::new("Coverage Reports").default_size([560.0, 360.0]),
        st,
        "coverage-reports",
    )
    .id(Id::new("coverage-reports"))
    .open(&mut open)
    .collapsible(false)
    .anchor(Align2::CENTER_CENTER, [0.0, -40.0])
    .show(ui.ctx(), |ui| {
        if list.is_empty() {
            ui.label(RichText::new("No coverage reports. Use Run › Run with Coverage.").color(theme.text_muted));
        }
        ScrollArea::vertical().auto_shrink([false, false]).max_height(300.0).show(ui, |ui| {
            for m in list.iter().rev() {
                ui.horizontal(|ui| {
                    let pct = testing::coverage_percent(m.lines_hit, m.lines_found);
                    let text = format!("{}  ·  {pct:.1}% lines  ·  {}", m.name, unix_label(m.created_unix));
                    let is_active = active.as_deref() == Some(m.id.as_str());
                    if ui.selectable_label(is_active, text).on_hover_text(&m.command_line).clicked() {
                        cmds.push(("coverage.openReport", json!({ "id": m.id })));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Delete").clicked() {
                            cmds.push(("coverage.deleteReport", json!({ "id": m.id })));
                        }
                    });
                });
            }
        });
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Import LCOV…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Import Coverage")
                    .add_filter("LCOV", &["lcov", "info"])
                    .pick_file()
            {
                cmds.push(("coverage.importLcov", json!({ "path": path })));
            }
            if ui.add_enabled(active.is_some(), egui::Button::new("Hide Coverage Data")).clicked() {
                cmds.push(("coverage.closeReport", Value::Null));
            }
        });
    });
    if !open {
        s.testing.reports_open = false;
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

// ---------------------------------------------------------------- Coverage tool window

type CollapsedDirs = std::collections::BTreeSet<PathBuf>;

/// The Coverage tool window: per-directory and per-file line coverage of the active report.
pub fn show_coverage(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let Some(report) = s.testing.coverage.as_ref() else {
        ui.label(RichText::new("No coverage data. Run tests with Run › Run with Coverage.").color(theme.text_muted));
        if ui.button("Manage Coverage Reports…").clicked() {
            run_command(s, st, "run.manageCoverageReports", &Value::Null);
        }
        return;
    };
    let root = s.project_root().unwrap_or_default();
    let rows = report.rows(&root);
    let pct = report.line_percent();
    let (hit, found) = report.totals();
    let mut in_editor = s.testing.coverage_in_editor;
    ui.horizontal(|ui| {
        ui.label(RichText::new(&report.name).strong());
        ui.label(RichText::new(format!("{pct:.1}% lines covered ({hit}/{found})")).color(theme.text_muted));
        ui.separator();
        if ui.checkbox(&mut in_editor, "Show in editor").changed() {
            cmds.push(("coverage.toggleEditorMarks", json!({ "value": in_editor })));
        }
        if ui.button("Reports…").clicked() {
            cmds.push(("run.manageCoverageReports", Value::Null));
        }
        if ui.button("Hide").on_hover_text("Hide Coverage Data").clicked() {
            cmds.push(("coverage.closeReport", Value::Null));
        }
    });
    let id = Id::new("coverage-collapsed");
    let mut collapsed: CollapsedDirs = ui.data_mut(|d| d.get_temp::<CollapsedDirs>(id).unwrap_or_default());
    // Rows under a collapsed directory are hidden.
    let visible: Vec<&testing::CoverageRow> = {
        let mut out = Vec::new();
        let mut hide_below: Option<(usize, &Path)> = None;
        for r in &rows {
            if let Some((depth, p)) = hide_below {
                if r.depth > depth && r.path.starts_with(p) {
                    continue;
                }
                hide_below = None;
            }
            out.push(r);
            if r.is_dir && collapsed.contains(&r.path) {
                hide_below = Some((r.depth, r.path.as_path()));
            }
        }
        out
    };
    let row_h = 20.0;
    let name_w = (ui.available_width() - 240.0).max(160.0);
    ui.horizontal(|ui| {
        ui.add_sized([name_w, row_h], egui::Label::new(RichText::new("Element").color(theme.text_muted)));
        ui.add_sized([110.0, row_h], egui::Label::new(RichText::new("Line, %").color(theme.text_muted)));
        ui.add_sized([110.0, row_h], egui::Label::new(RichText::new("Function, %").color(theme.text_muted)));
    });
    let mut toggle: Option<PathBuf> = None;
    ScrollArea::vertical().id_salt("coverage-rows").auto_shrink([false, false]).show_rows(
        ui,
        row_h,
        visible.len(),
        |ui, range| {
            for r in visible.get(range).unwrap_or(&[]) {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(400.0), row_h), Sense::click());
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &r.name));
                if resp.hovered() {
                    ui.painter().rect_filled(rect, CornerRadius::same(4), theme.hover);
                }
                let x0 = rect.left() + 4.0 + r.depth as f32 * 14.0;
                let cy = rect.center().y;
                if r.is_dir {
                    let arrow = if collapsed.contains(&r.path) { "⏵" } else { "⏷" };
                    ui.painter().text(
                        egui::pos2(x0 + 5.0, cy),
                        Align2::CENTER_CENTER,
                        arrow,
                        FontId::proportional(10.0),
                        theme.text_muted,
                    );
                }
                ui.painter().text(
                    egui::pos2(x0 + 16.0, cy),
                    Align2::LEFT_CENTER,
                    &r.name,
                    FontId::proportional(13.0),
                    theme.text,
                );
                // Line coverage: a bar and the percentage.
                let pct = r.line_percent();
                let bar = egui::Rect::from_min_size(egui::pos2(rect.left() + name_w + 4.0, cy - 4.0), vec2(40.0, 8.0));
                ui.painter().rect_filled(bar, CornerRadius::same(2), theme.error.gamma_multiply(0.6));
                let covered =
                    egui::Rect::from_min_size(bar.min, vec2(40.0 * (pct as f32 / 100.0).clamp(0.0, 1.0), 8.0));
                ui.painter().rect_filled(covered, CornerRadius::same(2), theme.success);
                ui.painter().text(
                    egui::pos2(bar.right() + 6.0, cy),
                    Align2::LEFT_CENTER,
                    format!("{pct:.0}% ({}/{})", r.lines_hit, r.lines_found),
                    FontId::proportional(12.0),
                    theme.text,
                );
                if r.functions_found > 0 {
                    ui.painter().text(
                        egui::pos2(rect.left() + name_w + 118.0, cy),
                        Align2::LEFT_CENTER,
                        format!("{:.0}% ({}/{})", r.function_percent(), r.functions_hit, r.functions_found),
                        FontId::proportional(12.0),
                        theme.text_muted,
                    );
                }
                if resp.clicked() {
                    if r.is_dir {
                        toggle = Some(r.path.clone());
                    } else {
                        cmds.push(("navigate.openLocation", json!({ "path": r.path, "line": 0, "column": 0 })));
                    }
                }
            }
        },
    );
    if let Some(p) = toggle {
        if !collapsed.remove(&p) {
            collapsed.insert(p);
        }
        ui.data_mut(|d| d.insert_temp(id, collapsed));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

// ---------------------------------------------------------------- editor gutter

/// Test results and coverage of one file, for the editor gutter.
#[derive(Default)]
pub(crate) struct GutterTests {
    tests: BTreeMap<usize, TestStatus>,
    coverage: Option<FileCoverage>,
}

impl GutterTests {
    pub(crate) fn collect(s: &Session, path: &Path) -> Self {
        Self {
            tests: s.testing.gutter_tests(path).cloned().unwrap_or_default(),
            coverage: s.testing.editor_coverage(path).cloned(),
        }
    }

    /// The run icon's colour on `line`: red after a failed run of the test there.
    pub(crate) fn run_icon_color(&self, line: usize, theme: &Theme) -> Color32 {
        match self.tests.get(&line) {
            Some(TestStatus::Failed) => theme.error,
            _ => theme.success,
        }
    }

    /// A coverage bar between the run icons and the line numbers (green covered, red not).
    pub(crate) fn paint_coverage(
        &self,
        painter: &egui::Painter,
        theme: &Theme,
        line: usize,
        y: f32,
        line_h: f32,
        x: f32,
    ) {
        let Some(covered) = self.coverage.as_ref().and_then(|c| c.line(line)) else { return };
        let color = if covered { theme.success } else { theme.error };
        let rect = egui::Rect::from_min_size(egui::pos2(x, y), vec2(3.0, line_h));
        painter.rect_filled(rect, CornerRadius::ZERO, color.gamma_multiply(0.8));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(format_duration(12), "12 ms");
        assert_eq!(format_duration(1500), "1.5 s");
        assert_eq!(format_duration(125_000), "2 m 5 s");
    }
}
