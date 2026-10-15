//! Tool windows: stripes with toggle buttons, and the left/bottom/right panels.

use std::path::{Path, PathBuf};

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, ScrollArea, Sense, Stroke, vec2};
use serde_json::{Value, json};

use codecraft_engine::{IMPLEMENTED_TOOL_WINDOWS, Session};
use codecraft_fs::NodeId;
use codecraft_workspace::{ConsoleKind, NotificationLevel, RunStatus, Severity, Side, ToolWindowId};

use crate::{UiState, keymap, run_command};

fn icon(tw: ToolWindowId) -> &'static str {
    match tw {
        ToolWindowId::Project => "🗀",
        ToolWindowId::Structure => "☰",
        ToolWindowId::Run => "▶",
        ToolWindowId::Problems => "⚠",
        ToolWindowId::Find => "🔍",
        ToolWindowId::Build => "🔨",
        ToolWindowId::Cargo => "📦",
        ToolWindowId::Notifications => "🔔",
        ToolWindowId::Debug => "🐞",
        ToolWindowId::Database => "🗄",
        ToolWindowId::Services => "📊",
        ToolWindowId::Agent => "✨",
        ToolWindowId::Terminal => ">_",
        ToolWindowId::Commit => "✓",
        ToolWindowId::Git => "🔀",
        ToolWindowId::Bookmarks => "🔖",
        ToolWindowId::Todo => "☑",
        ToolWindowId::Coverage => "☂",
        ToolWindowId::Hierarchy => "⊶",
        ToolWindowId::DatabaseChanges => "⇅",
        ToolWindowId::InspectionResults => "🔎",
        ToolWindowId::Profiler => "🔥",
        ToolWindowId::PythonConsole => "🐍",
        ToolWindowId::PythonPackages => "📦",
        ToolWindowId::FileTransfer => "🔃",
        ToolWindowId::RemoteHost => "🖥",
        ToolWindowId::RustRepl => "Rs",
        ToolWindowId::PythonProcessOutput => "🐍",
        ToolWindowId::CMake => "△",
        ToolWindowId::Zig => "Ƶ",
        ToolWindowId::TypeScript => "TS",
    }
}

/// A stripe button's side at full size.
const STRIPE_BUTTON: f32 = 30.0;
/// The smallest a stripe button gets before the stripe scrolls.
const STRIPE_BUTTON_MIN: f32 = 22.0;

fn stripe_button(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, tw: ToolWindowId, size: f32) {
    let visible = s.ws.layout.is_visible(tw);
    let color = if visible { st.theme.text } else { st.theme.text_muted };
    let mut tip = tw.title().to_string();
    if let Some(sc) = keymap::shortcut_label(&format!("view.toolWindows.{}", tw.key())) {
        tip = format!("{tip} ({sc})");
    }
    let glyph = (16.0 * size / STRIPE_BUTTON).max(12.0);
    let resp = ui
        .add(egui::Button::new(RichText::new(icon(tw)).size(glyph).color(color)).frame(false).min_size(vec2(size, size)))
        .on_hover_text(tip);
    if visible {
        ui.painter().rect_filled(resp.rect.shrink(1.0), CornerRadius::same(6), Color32::from_white_alpha(14));
    }
    if resp.clicked() {
        run_command(s, st, &format!("view.toolWindows.{}", tw.key()), &Value::Null);
    }
}

/// The left and right stripes (RustRover's New UI puts bottom tool windows low on the left stripe).
pub fn stripes(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let frame = Frame::new()
        .fill(st.theme.panel_bg)
        .inner_margin(Margin::symmetric(3, 6))
        .stroke(Stroke::new(1.0, st.theme.border));
    let width = if s.ws.settings.compact_mode { 32.0 } else { 38.0 };
    egui::Panel::left("stripe-left").exact_size(width).resizable(false).frame(frame).show(ui, |ui| {
        let top: Vec<ToolWindowId> = IMPLEMENTED_TOOL_WINDOWS.into_iter().filter(|t| t.side() == Side::Left).collect();
        // Bottom tool windows sit low on the stripe, the last one lowest.
        let mut bottom: Vec<ToolWindowId> =
            IMPLEMENTED_TOOL_WINDOWS.into_iter().rev().filter(|t| t.side() == Side::Bottom).collect();
        // CMake projects get the CMake tool window (track CL).
        if s.cpp.cmake.is_cmake {
            bottom.push(ToolWindowId::CMake);
        }
        // Zig and TypeScript projects get their tool windows (M13 track RC).
        if is_project_of(ui, s, "zig", codecraft_engine::zig_tool::is_zig_project) {
            bottom.push(ToolWindowId::Zig);
        }
        if is_project_of(ui, s, "typescript", codecraft_engine::typescript_tool::is_typescript_project) {
            bottom.push(ToolWindowId::TypeScript);
        }
        // The two groups overlapped in a 700 px window (QA M14): buttons shrink to fit, and when
        // even small ones don't fit, the stripe is one list that scrolls.
        let (size, fits) = stripe_fit(ui, top.len() + bottom.len());
        if fits {
            ui.vertical_centered(|ui| {
                for tw in top {
                    stripe_button(ui, s, st, tw, size);
                }
                ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
                    for tw in bottom {
                        stripe_button(ui, s, st, tw, size);
                    }
                });
            });
        } else {
            ScrollArea::vertical()
                .id_salt("stripe-left-scroll")
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        for tw in top {
                            stripe_button(ui, s, st, tw, size);
                        }
                        ui.separator();
                        for tw in bottom.into_iter().rev() {
                            stripe_button(ui, s, st, tw, size);
                        }
                    });
                });
        }
    });
    egui::Panel::right("stripe-right").exact_size(width).resizable(false).frame(frame).show(ui, |ui| {
        let right: Vec<ToolWindowId> = IMPLEMENTED_TOOL_WINDOWS.into_iter().filter(|t| t.side() == Side::Right).collect();
        let (size, _) = stripe_fit(ui, right.len());
        ScrollArea::vertical()
            .id_salt("stripe-right-scroll")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    for tw in right {
                        stripe_button(ui, s, st, tw, size);
                    }
                });
            });
    });
}

/// The button size for `count` stripe buttons in the stripe's height, and whether they fit.
pub(crate) fn stripe_fit(ui: &egui::Ui, count: usize) -> (f32, bool) {
    let n = count.max(1) as f32;
    let gap = ui.spacing().item_spacing.y;
    let room = ui.available_height() - 8.0;
    let size = (room / n - gap).clamp(STRIPE_BUTTON_MIN, STRIPE_BUTTON);
    (size, n * (size + gap) <= room)
}

fn header(ui: &mut egui::Ui, st: &UiState, title: &str) -> bool {
    let mut hide = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().size(13.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui
                .add(egui::Button::new(RichText::new("—").color(st.theme.text_muted)).frame(false))
                .on_hover_text("Hide")
                .clicked()
            {
                hide = true;
            }
        });
    });
    ui.add_space(2.0);
    hide
}

/// The right-aligned end of a toolbar row: on this row when `needed` points fit, else on a row
/// of its own (the row must be `horizontal_wrapped`). A right-to-left group in a full row drew
/// over the row's buttons and fields at small widths (QA M14: the Debug and Profiler toolbars at
/// 900 px).
pub(crate) fn trailing(ui: &mut egui::Ui, needed: f32, add: impl FnOnce(&mut egui::Ui)) {
    if ui.available_size_before_wrap().x < needed && ui.layout().main_wrap() {
        ui.end_row();
    }
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), add);
}

/// Start a new row of a `horizontal_wrapped` toolbar unless `width` points are left on this one.
/// Combo boxes and fixed-width fields don't wrap by themselves: they ran past the panel's edge
/// and pushed the rest of the row out of view (QA M14).
pub(crate) fn room_for(ui: &mut egui::Ui, width: f32) {
    if ui.available_size_before_wrap().x < width && ui.layout().main_wrap() {
        ui.end_row();
    }
}

/// The width `text` takes in `style` (to decide whether a trailing group fits).
pub(crate) fn text_width(ui: &egui::Ui, text: &str, style: egui::TextStyle) -> f32 {
    let font = style.resolve(ui.style());
    ui.painter().layout_no_wrap(text.to_string(), font, egui::Color32::WHITE).size().x
}

/// Claim the rest of a tool window panel. egui sizes a panel's frame by its contents (at least
/// the panel's minimum), and the size is remembered: a tool window with little to show (an
/// empty Run, "Nothing to debug") shrank to the 80 px minimum and every bottom tool window
/// opened that small afterwards (QA M14).
fn fill_panel(ui: &mut egui::Ui) {
    let rest = ui.available_size_before_wrap();
    if rest.x > 0.0 && rest.y > 0.0 {
        ui.allocate_space(rest);
    }
}

pub fn side_panels(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    if s.ws.layout.all_hidden {
        return;
    }
    let frame =
        Frame::new().fill(st.theme.panel_bg).inner_margin(Margin::same(6)).stroke(Stroke::new(1.0, st.theme.border));
    // Sizes set by commands (Stretch, layouts) apply through a new panel id; a maximized tool
    // window fills the window (Window › Active Tool Window › Maximize Tool Window).
    let rev = s.ws.layout.size_rev;
    let maximized = s.ws.layout.maximized;
    if let Some(tw) = s.ws.layout.bottom {
        let size = s.ws.layout.size_of(tw, s.ws.layout.bottom_height);
        let mut panel = egui::Panel::bottom(format!("tw-bottom-{}-{rev}", tw.key()))
            .resizable(true)
            .default_size(size)
            .size_range(80.0..=1200.0)
            .frame(frame);
        if maximized == Some(tw) {
            panel = panel.resizable(false).exact_size((ui.available_height() - 60.0).max(80.0));
        }
        let resp = panel.show(ui, |ui| {
            if header(ui, st, tw.title()) {
                s.ws.layout.hide(tw);
            }
            tool_window(ui, s, st, tw);
            fill_panel(ui);
        });
        if maximized != Some(tw) {
            s.ws.layout.bottom_height = resp.response.rect.height();
            s.ws.layout.remember_size(tw, s.ws.layout.bottom_height);
        }
    }
    if let Some(tw) = s.ws.layout.left {
        let size = s.ws.layout.size_of(tw, s.ws.layout.left_width);
        let mut panel = egui::Panel::left(format!("tw-left-{}-{rev}", tw.key()))
            .resizable(true)
            .default_size(size)
            .size_range(140.0..=900.0)
            .frame(frame);
        if maximized == Some(tw) {
            panel = panel.resizable(false).exact_size((ui.available_width() - 60.0).max(140.0));
        }
        let resp = panel.show(ui, |ui| {
            if header(ui, st, tw.title()) {
                s.ws.layout.hide(tw);
            }
            tool_window(ui, s, st, tw);
            fill_panel(ui);
        });
        if maximized != Some(tw) {
            s.ws.layout.left_width = resp.response.rect.width();
            s.ws.layout.remember_size(tw, s.ws.layout.left_width);
        }
    }
    if let Some(tw) = s.ws.layout.right {
        let size = s.ws.layout.size_of(tw, s.ws.layout.right_width);
        let mut panel = egui::Panel::right(format!("tw-right-{}-{rev}", tw.key()))
            .resizable(true)
            .default_size(size)
            .size_range(160.0..=900.0)
            .frame(frame);
        if maximized == Some(tw) {
            panel = panel.resizable(false).exact_size((ui.available_width() - 60.0).max(160.0));
        }
        let resp = panel.show(ui, |ui| {
            if header(ui, st, tw.title()) {
                s.ws.layout.hide(tw);
            }
            tool_window(ui, s, st, tw);
            fill_panel(ui);
        });
        if maximized != Some(tw) {
            s.ws.layout.right_width = resp.response.rect.width();
            s.ws.layout.remember_size(tw, s.ws.layout.right_width);
        }
    }
}

fn tool_window(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, tw: ToolWindowId) {
    match tw {
        ToolWindowId::Project => project_view(ui, s, st),
        ToolWindowId::Run => runs_view(ui, s, st, false),
        ToolWindowId::Build => runs_view(ui, s, st, true),
        ToolWindowId::Problems => problems_view(ui, s, st),
        ToolWindowId::Find => find_view(ui, s, st),
        ToolWindowId::Structure => structure_view(ui, s, st),
        ToolWindowId::Cargo => cargo_view(ui, s, st),
        ToolWindowId::Notifications => notifications_view(ui, s, st),
        ToolWindowId::Debug => crate::debug_panel::show(ui, s, st),
        ToolWindowId::Database => crate::database_panel::show(ui, s, st),
        ToolWindowId::Services => crate::services_panel::show(ui, s, st),
        ToolWindowId::Agent => crate::agent_panel::show(ui, s, st),
        ToolWindowId::Terminal => crate::terminal_panel::show(ui, s, st),
        ToolWindowId::Commit => crate::vcs_panels::show(ui, s, st),
        ToolWindowId::Git => crate::vcs_panels::show_git(ui, s, st),
        ToolWindowId::Bookmarks => crate::bookmarks_panel::show_bookmarks(ui, s, st),
        ToolWindowId::Todo => crate::bookmarks_panel::show_todo(ui, s, st),
        ToolWindowId::Coverage => crate::testing_panel::show_coverage(ui, s, st),
        ToolWindowId::Hierarchy => crate::refactor_ui::show_hierarchy(ui, s, st),
        ToolWindowId::DatabaseChanges => crate::misc_ui::show_database_changes(ui, s, st),
        ToolWindowId::InspectionResults => crate::inspections_ui::show(ui, s, st),
        ToolWindowId::Profiler => crate::profiler_ui::show(ui, s, st),
        ToolWindowId::PythonConsole => crate::python_ui::show_console(ui, s, st),
        ToolWindowId::PythonPackages => crate::python_ui::show_packages(ui, s, st),
        ToolWindowId::FileTransfer => crate::remote_ui::show_file_transfer(ui, s, st),
        ToolWindowId::RemoteHost => crate::remote_ui::show_remote_host(ui, s, st),
        ToolWindowId::RustRepl => crate::rust_ui::show_repl(ui, s, st),
        ToolWindowId::PythonProcessOutput => crate::python_ui::show_process_output(ui, s, st),
        ToolWindowId::CMake => crate::cmake_ui::show(ui, s, st),
        ToolWindowId::Zig => crate::rc_ui::show_zig(ui, s, st),
        ToolWindowId::TypeScript => crate::rc_ui::show_typescript(ui, s, st),
    }
}

/// Whether the project is of a kind (checked on disk once per project and kind, cached in egui
/// memory), or its tool window is open anyway.
fn is_project_of(ui: &egui::Ui, s: &Session, kind: &str, check: fn(&Session) -> bool) -> bool {
    let Some(root) = s.project_root() else { return false };
    let id = egui::Id::new(("project-kind", kind, root.clone()));
    if let Some(v) = ui.data(|d| d.get_temp::<bool>(id)) {
        return v;
    }
    let v = check(s);
    ui.data_mut(|d| d.insert_temp(id, v));
    v
}

// ---------------------------------------------------------------- Project

/// `~/…` for paths under the home directory, like RustRover's project view.
fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if path.starts_with(&home) => {
            format!("~/{}", path.strip_prefix(&home).map(|p| p.display().to_string()).unwrap_or_default())
        }
        _ => path.display().to_string(),
    }
}

fn file_badge(name: &str, is_dir: bool, theme: &crate::theme::Theme) -> (&'static str, Color32) {
    if is_dir {
        return ("▣", Color32::from_rgb(0x8C, 0x9B, 0xB5));
    }
    match Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => ("◆", Color32::from_rgb(0xE0, 0x7B, 0x3C)),
        "toml" => ("⚙", theme.text_muted),
        "md" => ("M", Color32::from_rgb(0x54, 0x8A, 0xF7)),
        "json" | "yml" | "yaml" => ("{}", theme.text_muted),
        "lock" => ("🔒", theme.text_muted),
        _ => ("▫", theme.text_muted),
    }
}

fn project_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let Some(project) = s.ws.project.clone() else {
        ui.label(RichText::new("No project open").color(st.theme.text_muted));
        return;
    };
    let theme = st.theme.clone();
    let error_files: Vec<PathBuf> =
        s.ws.all_problems()
            .iter()
            .filter(|(_, v)| v.iter().any(|p| p.severity == Severity::Error))
            .map(|(p, _)| p.to_path_buf())
            .collect();
    // Flatten the visible tree.
    let mut rows: Vec<(NodeId, usize)> = Vec::new();
    let mut stack = vec![(project.tree.root(), 0usize)];
    while let Some((id, depth)) = stack.pop() {
        let Some(node) = project.tree.node(id) else { continue };
        rows.push((id, depth));
        if node.is_dir && project.expanded.contains(&node.path) {
            for c in node.children.iter().rev() {
                stack.push((*c, depth + 1));
            }
        }
        if rows.len() > 20_000 {
            break;
        }
    }
    let row_h = 22.0;
    let mut toggle: Option<PathBuf> = None;
    let mut select: Option<PathBuf> = None;
    let mut open: Option<PathBuf> = None;
    let mut preview: Option<PathBuf> = None;
    let mut cmd: Option<(&'static str, Value)> = None;
    ScrollArea::both().auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
        for (id, depth) in rows.get(range).unwrap_or(&[]) {
            let Some(node) = project.tree.node(*id) else { continue };
            // Rows below the root drag onto directories to move (refactor.moveFile).
            let sense = if *depth > 0 { Sense::click_and_drag() } else { Sense::click() };
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width().max(200.0), row_h), sense);
            if *depth > 0 {
                resp.dnd_set_drag_payload(node.path.clone());
            }
            let drop_dir = if node.is_dir { Some(node.path.clone()) } else { node.path.parent().map(Path::to_path_buf) };
            let selected = project.selected.as_deref() == Some(node.path.as_path());
            if let Some(dragged) = resp.dnd_hover_payload::<PathBuf>()
                && drop_dir.as_deref().is_some_and(|d| dragged.parent() != Some(d) && !d.starts_with(dragged.as_path()))
            {
                ui.painter().rect_stroke(
                    rect,
                    CornerRadius::same(4),
                    egui::Stroke::new(1.5, theme.accent),
                    egui::StrokeKind::Inside,
                );
            }
            if let Some(dragged) = resp.dnd_release_payload::<PathBuf>()
                && let Some(dir) = drop_dir.filter(|d| dragged.parent() != Some(d.as_path()) && !d.starts_with(dragged.as_path()))
            {
                cmd = Some(("refactor.moveFile", json!({ "path": *dragged, "to": dir, "ask": true })));
            }
            if selected {
                ui.painter().rect_filled(rect, CornerRadius::same(4), theme.list_selection);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, CornerRadius::same(4), theme.hover);
            }
            let x0 = rect.left() + 4.0 + *depth as f32 * 16.0;
            let cy = rect.center().y;
            if node.is_dir {
                let arrow = if project.expanded.contains(&node.path) { "⏷" } else { "⏵" };
                ui.painter().text(
                    egui::pos2(x0 + 5.0, cy),
                    egui::Align2::CENTER_CENTER,
                    arrow,
                    FontId::proportional(10.0),
                    theme.text_muted,
                );
            }
            let (badge, badge_color) = file_badge(&node.name, node.is_dir, &theme);
            ui.painter().text(
                egui::pos2(x0 + 20.0, cy),
                egui::Align2::CENTER_CENTER,
                badge,
                FontId::proportional(11.0),
                badge_color,
            );
            let has_error = !node.is_dir && error_files.contains(&node.path)
                || node.is_dir && error_files.iter().any(|f| f.starts_with(&node.path));
            let name_color = if has_error {
                theme.error
            } else {
                crate::vcs_panels::tree_color(s, &theme, &node.path, node.is_dir).unwrap_or(theme.text)
            };
            let mut job = LayoutJob::default();
            let name = if *depth == 0 { project.name.clone() } else { node.name.clone() };
            job.append(
                &name,
                0.0,
                TextFormat { font_id: FontId::proportional(13.0), color: name_color, ..Default::default() },
            );
            if *depth == 0 {
                job.append(
                    &format!("  {}", tilde(&node.path)),
                    0.0,
                    TextFormat { font_id: FontId::proportional(12.0), color: theme.text_muted, ..Default::default() },
                );
            }
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            ui.painter().galley(egui::pos2(x0 + 30.0, cy - galley.size().y / 2.0), galley, theme.text);
            if resp.clicked() {
                select = Some(node.path.clone());
                if node.is_dir && resp.interact_pointer_pos().is_some_and(|p| p.x < x0 + 12.0) {
                    toggle = Some(node.path.clone());
                }
                if !node.is_dir {
                    preview = Some(node.path.clone());
                }
            }
            if resp.double_clicked() {
                if node.is_dir {
                    toggle = Some(node.path.clone());
                } else {
                    open = Some(node.path.clone());
                }
            }
            resp.context_menu(|ui| {
                if ui.button("New › File").clicked() {
                    select = Some(node.path.clone());
                    cmd = Some(("file.new.file", Value::Null));
                    ui.close();
                }
                if ui.button("New › Directory").clicked() {
                    select = Some(node.path.clone());
                    cmd = Some(("file.new.directory", Value::Null));
                    ui.close();
                }
                if *depth > 0 {
                    ui.separator();
                    for (label, id) in [
                        ("Rename…", "refactor.renameFile"),
                        ("Move…", "refactor.moveFile"),
                        ("Copy…", "refactor.copyFile"),
                        ("Delete…", "refactor.safeDelete"),
                    ] {
                        if ui.button(label).clicked() {
                            select = Some(node.path.clone());
                            cmd = Some((id, json!({ "path": node.path })));
                            ui.close();
                        }
                    }
                    ui.separator();
                }
                if ui.button("Copy Absolute Path").clicked() {
                    cmd = Some(("projectView.copyPath", json!({ "path": node.path })));
                    ui.close();
                }
                if ui.button("Copy Path from Project Root").clicked() {
                    cmd = Some(("projectView.copyPath", json!({ "path": node.path, "relative": true })));
                    ui.close();
                }
                if ui.button("Reveal in Finder").clicked() {
                    cmd = Some(("projectView.revealInFinder", json!({ "path": node.path })));
                    ui.close();
                }
                if !node.is_dir && node.name.ends_with(".rs") && ui.button("Run Tests in File").clicked() {
                    cmd = Some(("run.contextConfiguration", json!({ "path": node.path, "line": 0 })));
                    ui.close();
                }
            });
        }
    });
    if let Some(p) = &mut s.ws.project {
        if let Some(sel) = select {
            p.selected = Some(sel);
        }
        if let Some(t) = toggle
            && !p.expanded.remove(&t)
        {
            p.expanded.insert(t);
        }
    }
    if let Some(path) = open {
        run_command(s, st, "file.open", &json!({ "path": path }));
    } else if let Some(path) = preview {
        // A single click opens (or selects) the file in the preview tab.
        run_command(s, st, "projectView.openPreview", &json!({ "path": path }));
    }
    if let Some((c, params)) = cmd {
        run_command(s, st, c, &params);
    }
    if project.tree.truncated() {
        ui.label(RichText::new("The project tree was truncated (too many files).").color(theme.warning).size(11.0));
    }
}

// ---------------------------------------------------------------- Run / Build

/// A `path.rs:line:col` reference in console output (rustc `-->`, panics, test failures).
pub fn find_location(line: &str) -> Option<(String, usize, usize, std::ops::Range<usize>)> {
    let idx = line.find(".rs:")?;
    let start =
        line[..idx].rfind(|c: char| c.is_whitespace() || c == '(' || c == '\'' || c == '"').map(|i| i + 1).unwrap_or(0);
    let rest = &line[idx + 4..];
    let ln_len = rest.chars().take_while(char::is_ascii_digit).count();
    if ln_len == 0 {
        return None;
    }
    let ln: usize = rest.get(..ln_len)?.parse().ok()?;
    let after = &rest[ln_len..];
    let (col, col_len) = match after.strip_prefix(':') {
        Some(c) => {
            let n = c.chars().take_while(char::is_ascii_digit).count();
            (c.get(..n).and_then(|d| d.parse().ok()).unwrap_or(1), n + 1)
        }
        None => (1, 0),
    };
    let path = line.get(start..idx + 3)?.to_string();
    Some((path, ln, col, start..idx + 4 + ln_len + col_len))
}

fn runs_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, build: bool) {
    let theme = st.theme.clone();
    let tabs: Vec<_> =
        s.ws.runs.iter().filter(|r| r.build == build).map(|r| (r.id, r.title.clone(), r.status)).collect();
    if tabs.is_empty() {
        ui.label(
            RichText::new(if build {
                "Build output appears here (Build › Build Project)."
            } else {
                "Nothing has run yet. Run › Run (⌃R)."
            })
            .color(theme.text_muted),
        );
        return;
    }
    let active = s.ws.active_run.filter(|a| tabs.iter().any(|(id, _, _)| id == a)).or_else(|| tabs.last().map(|t| t.0));
    let mut select = None;
    let mut close = None;
    // Window › Active Tool Window › Group Tabs: the tabs as one list.
    if s.ws.layout.group_tabs {
        ui.horizontal(|ui| {
            let current = tabs.iter().find(|(id, ..)| Some(*id) == active).map(|(_, t, _)| t.clone()).unwrap_or_default();
            egui::ComboBox::from_id_salt(("run-tabs", build)).selected_text(format!("{current}  ({} tabs)", tabs.len())).show_ui(ui, |ui| {
                for (id, title, status) in &tabs {
                    let mark = match status {
                        RunStatus::Running => "●",
                        RunStatus::Succeeded => "✓",
                        RunStatus::Failed(_) => "✖",
                        RunStatus::Stopped => "■",
                    };
                    if ui.selectable_label(Some(*id) == active, format!("{mark} {title}")).clicked() {
                        select = Some(*id);
                    }
                }
            });
            if let Some(a) = active
                && ui.add(egui::Button::new(RichText::new("×").color(theme.text_muted)).frame(false)).clicked()
            {
                close = Some(a);
            }
        });
    }
    ui.horizontal(|ui| {
        if s.ws.layout.group_tabs {
            return;
        }
        for (id, title, status) in &tabs {
            let (mark, color) = match status {
                RunStatus::Running => ("●", theme.success),
                RunStatus::Succeeded => ("✓", theme.success),
                RunStatus::Failed(_) => ("✖", theme.error),
                RunStatus::Stopped => ("■", theme.text_muted),
            };
            let mut job = LayoutJob::default();
            job.append(mark, 0.0, TextFormat { color, font_id: FontId::proportional(12.0), ..Default::default() });
            job.append(
                &format!(" {title}"),
                0.0,
                TextFormat { color: theme.text, font_id: FontId::proportional(13.0), ..Default::default() },
            );
            if ui.add(egui::Button::new(job).selected(Some(*id) == active)).clicked() {
                select = Some(*id);
            }
            if ui.add(egui::Button::new(RichText::new("×").color(theme.text_muted)).frame(false)).clicked() {
                close = Some(*id);
            }
        }
    });
    if let Some(id) = select {
        s.ws.active_run = Some(id);
    }
    if let Some(id) = close {
        run_command(s, st, "run.closeTab", &json!({ "tab": id.0 }));
        return;
    }
    let Some(tab) = active.and_then(|a| s.ws.run(a)).cloned() else { return };
    let is_test_run = s.testing.run(tab.id).is_some();
    ui.horizontal(|ui| {
        if ui.button("↻ Rerun").on_hover_text("Rerun (⌘R)").clicked() {
            s.ws.active_run = Some(tab.id);
            run_command(s, st, "run.rerun", &Value::Null);
        }
        if ui.add_enabled(tab.is_running(), egui::Button::new("■ Stop")).clicked() {
            s.ws.active_run = Some(tab.id);
            run_command(s, st, "run.stop", &Value::Null);
        }
        ui.checkbox(&mut st.console_follow, "Scroll to end");
        let (p, f, i) = tab.test_counts();
        if p + f + i > 0 && !is_test_run {
            let color = if f > 0 { theme.error } else { theme.success };
            ui.label(RichText::new(format!("Tests passed: {p}, failed: {f}, ignored: {i}")).color(color));
        }
        if let Some(d) = tab.duration {
            ui.label(RichText::new(format!("{:.1}s", d.as_secs_f32())).color(theme.text_muted));
        }
    });
    let failed: Vec<&str> = tab
        .tests
        .iter()
        .filter(|t| t.outcome == codecraft_cargo::TestOutcome::Failed)
        .map(|t| t.name.as_str())
        .collect();
    // Test runs: the test tree (and the selected test's output) beside the console.
    if is_test_run && !crate::testing_panel::show_test_run(ui, s, st, tab.id) {
        return;
    }
    // Valgrind Memcheck and sanitizer reports of this run (track CL).
    if crate::valgrind_ui::show_run_extras(ui, s, st, tab.id) {
        return;
    }
    if !failed.is_empty() && !is_test_run {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Failed:").color(theme.error));
            for name in failed {
                ui.label(RichText::new(name).monospace().color(theme.error));
            }
        });
    }
    let root =
        s.ws.project
            .as_ref()
            .map(|p| p.cargo.as_ref().map(|m| m.workspace_root.clone()).unwrap_or_else(|| p.root.clone()));
    let font = FontId::monospace((s.ws.settings.editor_font_size - 1.0).max(9.0));
    let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
    let mut open: Option<(PathBuf, usize, usize)> = None;
    Frame::new().fill(theme.editor_bg).inner_margin(Margin::same(4)).show(ui, |ui| {
        ScrollArea::both().auto_shrink([false, false]).stick_to_bottom(st.console_follow).show_rows(
            ui,
            row_h,
            tab.lines.len(),
            |ui, range| {
                for line in tab.lines.get(range).unwrap_or(&[]) {
                    let color = match line.kind {
                        ConsoleKind::Stdout => theme.text_editor,
                        ConsoleKind::Stderr => {
                            if line.text.starts_with("warning") {
                                theme.warning
                            } else if line.text.starts_with("error") {
                                theme.console_stderr
                            } else {
                                theme.text_editor
                            }
                        }
                        ConsoleKind::System => theme.console_system,
                    };
                    match find_location(&line.text) {
                        Some((path, ln, col, range)) => {
                            let mut job = LayoutJob::default();
                            let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
                            job.append(line.text.get(..range.start).unwrap_or(""), 0.0, fmt(color));
                            let mut link = fmt(theme.link);
                            link.underline = Stroke::new(1.0, theme.link);
                            job.append(line.text.get(range.clone()).unwrap_or(""), 0.0, link);
                            job.append(line.text.get(range.end..).unwrap_or(""), 0.0, fmt(color));
                            if ui
                                .add(egui::Label::new(job).sense(Sense::click()))
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                            {
                                let p = PathBuf::from(&path);
                                let abs = if p.is_absolute() { p } else { root.clone().unwrap_or_default().join(p) };
                                open = Some((abs, ln.saturating_sub(1), col.saturating_sub(1)));
                            }
                        }
                        None => {
                            ui.label(RichText::new(&line.text).font(font.clone()).color(color));
                        }
                    }
                }
            },
        );
    });
    if let Some((path, line, column)) = open {
        run_command(s, st, "navigate.openLocation", &json!({ "path": path, "line": line, "column": column }));
    }
}

// ---------------------------------------------------------------- Problems / Find / Structure

/// The Problems tool window: File and Project tabs (M14 track CA, `inspect_ui.rs`).
fn problems_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    crate::inspect_ui::problems_view(ui, s, st);
}

fn find_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let Some(find) = s.ws.find.clone() else {
        ui.label(RichText::new("Use Find in Files (⇧⌘F) or Find Usages (⌥F7).").color(theme.text_muted));
        return;
    };
    let files = {
        let mut f: Vec<&PathBuf> = find.matches.iter().map(|m| &m.path).collect();
        f.dedup();
        f.len()
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(&find.title).strong());
        if find.searching {
            ui.add(egui::Spinner::new().size(12.0));
        } else {
            ui.label(
                RichText::new(format!("{} result(s) in {files} file(s)", find.matches.len())).color(theme.text_muted),
            );
        }
    });
    let mut open = None;
    let font = FontId::monospace(12.0);
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let mut current: Option<&PathBuf> = None;
        for m in find.matches.iter().take(5000) {
            if current != Some(&m.path) {
                current = Some(&m.path);
                let name =
                    s.ws.project
                        .as_ref()
                        .map(|p| p.relative(&m.path).into_owned())
                        .unwrap_or_else(|| m.path.display().to_string());
                ui.label(RichText::new(name).strong().size(12.0));
            }
            let mut job = LayoutJob::default();
            let fmt = |c: Color32| TextFormat { font_id: font.clone(), color: c, ..Default::default() };
            job.append(&format!("{:>5}  ", m.line + 1), 0.0, fmt(theme.line_number_active));
            let chars: Vec<char> = m.line_text.chars().collect();
            let lead = chars.iter().take_while(|c| c.is_whitespace()).count().min(m.column);
            let seg = |a: usize, b: usize| {
                chars
                    .get(a.min(chars.len())..b.min(chars.len()))
                    .map(|c| c.iter().collect::<String>())
                    .unwrap_or_default()
            };
            job.append(&seg(lead, m.column), 0.0, fmt(theme.text_editor));
            let mut hl = fmt(theme.text);
            hl.background = theme.find_match;
            job.append(&seg(m.column, m.end_column), 0.0, hl);
            job.append(&seg(m.end_column, chars.len()), 0.0, fmt(theme.text_editor));
            if ui.add(egui::Label::new(job).sense(Sense::click())).clicked() {
                open = Some((m.path.clone(), m.line, m.column));
            }
        }
    });
    if let Some((path, line, column)) = open {
        run_command(s, st, "navigate.openLocation", &json!({ "path": path, "line": line, "column": column }));
    }
}

fn structure_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let Some((_, d)) = s.ws.active() else {
        ui.label(RichText::new("No file is open").color(theme.text_muted));
        return;
    };
    let items = codecraft_syntax::outline(d.language, &d.doc.text());
    let path = d.path.clone();
    let mut open = None;
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for item in &items {
            let kind = format!("{:?}", item.kind).to_lowercase();
            let mut job = LayoutJob::default();
            job.append(&"    ".repeat(item.depth), 0.0, TextFormat::default());
            job.append(
                &format!("{kind} "),
                0.0,
                TextFormat { color: theme.keyword, font_id: FontId::proportional(12.0), ..Default::default() },
            );
            job.append(
                &item.name,
                0.0,
                TextFormat { color: theme.text, font_id: FontId::proportional(13.0), ..Default::default() },
            );
            if ui.add(egui::Label::new(job).sense(Sense::click())).clicked() {
                open = Some((item.line, item.column));
            }
        }
    });
    if let (Some((line, column)), Some(path)) = (open, path) {
        run_command(s, st, "navigate.openLocation", &json!({ "path": path, "line": line, "column": column }));
    }
}

// ---------------------------------------------------------------- Cargo / Notifications

fn cargo_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    ui.horizontal_wrapped(|ui| {
        for (label, cmd) in [
            ("Check", "cargo.check"),
            ("Clippy", "cargo.clippy"),
            ("Build", "build.buildProject"),
            ("Test", "cargo.test"),
            ("Doc", "cargo.doc"),
        ] {
            if ui.add_enabled(s.enabled(cmd).is_ok(), egui::Button::new(label)).clicked() {
                run_command(s, st, cmd, &Value::Null);
            }
        }
    });
    ui.separator();
    let Some(project) = s.ws.project.clone() else { return };
    let Some(meta) = project.cargo.clone() else {
        ui.label(
            RichText::new(project.cargo_error.unwrap_or_else(|| "Loading Cargo metadata…".into()))
                .color(theme.text_muted),
        );
        return;
    };
    let mut run = None;
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for p in meta.workspace_packages() {
            egui::CollapsingHeader::new(RichText::new(format!("📦 {} {}", p.name, p.version)).strong())
                .id_salt(&p.id)
                .default_open(true)
                .show(ui, |ui| {
                    for t in &p.targets {
                        let kind = t.kind.join(", ");
                        let text = RichText::new(format!("{}  ({kind})", t.name));
                        let resp =
                            ui.add(egui::Label::new(text).sense(Sense::click())).on_hover_text("Double-click to run");
                        if resp.double_clicked() {
                            run = Some((p.name.clone(), t.clone()));
                        }
                    }
                    if !p.dependencies.is_empty() {
                        egui::CollapsingHeader::new(RichText::new("Dependencies").color(theme.text_muted))
                            .id_salt((&p.id, "deps"))
                            .show(ui, |ui| {
                                for d in &p.dependencies {
                                    let kind = d.kind.as_deref().map(|k| format!(" [{k}]")).unwrap_or_default();
                                    ui.label(
                                        RichText::new(format!("{} {}{kind}", d.name, d.req))
                                            .color(theme.text_muted)
                                            .size(12.0),
                                    );
                                }
                            });
                    }
                });
        }
    });
    if let Some((package, t)) = run {
        let kind = if t.is_bin() {
            "bin"
        } else if t.is_example() {
            "example"
        } else if t.is_test() {
            "test"
        } else if t.is_bench() {
            "bench"
        } else {
            "lib"
        };
        run_command(s, st, "cargo.runTarget", &json!({ "package": package, "kind": kind, "name": t.name }));
    }
}

fn notifications_view(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    if s.ws.notifications.is_empty() {
        ui.label(RichText::new("No notifications").color(theme.text_muted));
        return;
    }
    if ui.button("Clear All").clicked() {
        s.ws.notifications.clear();
        return;
    }
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for n in s.ws.notifications.iter().rev() {
            let color = match n.level {
                NotificationLevel::Error => theme.error,
                NotificationLevel::Warning => theme.warning,
                NotificationLevel::Info => theme.text,
            };
            Frame::new().fill(theme.editor_bg).corner_radius(6).inner_margin(8).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(&n.title).strong().color(color));
                if !n.message.is_empty() {
                    ui.label(RichText::new(&n.message).size(12.0));
                }
            });
            ui.add_space(4.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::find_location;

    #[test]
    fn console_locations() {
        let (p, l, c, _) = find_location("  --> src/main.rs:12:5").unwrap();
        assert_eq!((p.as_str(), l, c), ("src/main.rs", 12, 5));
        let (p, l, c, _) = find_location("thread 'main' panicked at src/lib.rs:3:9:").unwrap();
        assert_eq!((p.as_str(), l, c), ("src/lib.rs", 3, 9));
        assert!(find_location("no location here").is_none());
        assert!(find_location("file.rs:x").is_none());
        let (_, l, c, _) = find_location("at /abs/path/ünï.rs:7").unwrap();
        assert_eq!((l, c), (7, 1));
    }
}
