//! Window chrome: menu bar, main toolbar, status bar, welcome screen, editor tabs and find bar.

use egui::{
    Align, Color32, CornerRadius, FontId, Frame, Key, Layout, Margin, Rect, RichText, Sense, Stroke, TextEdit, pos2,
    vec2,
};
use serde_json::{Value, json};

use codecraft_engine::appearance::{self, NavPathItem, widget_visible};
use codecraft_engine::{LspStatus, Session};
use codecraft_workspace::{EditorId, GroupId, Severity, SplitAxis, SplitNode};

use crate::{PersistedState, UiState, editor, menus, run_command};

fn bar_frame(st: &UiState) -> Frame {
    Frame::new()
        .fill(st.theme.header_bg)
        .inner_margin(Margin::symmetric(8, 4))
        .stroke(Stroke::new(1.0, st.theme.border))
}

pub fn menu_bar(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    egui::Panel::top("menu-bar")
        .frame(Frame::new().fill(st.theme.header_bg).inner_margin(Margin::symmetric(6, 2)))
        .show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| menus::menu_bar_contents(ui, s, st));
        });
}

fn tool_button(ui: &mut egui::Ui, st: &UiState, text: &str, tip: &str, enabled: bool, color: Option<Color32>) -> bool {
    let rich = RichText::new(text).size(15.0).color(if enabled {
        color.unwrap_or(st.theme.text)
    } else {
        st.theme.text_disabled
    });
    ui.add_enabled(enabled, egui::Button::new(rich).frame(false).min_size(vec2(26.0, 24.0)))
        .on_hover_text(tip)
        .clicked()
}

/// Heights of the main toolbar and the status bar (View › Appearance › Compact Mode lowers them).
pub(crate) fn bar_heights(compact: bool) -> (f32, f32) {
    if compact { (30.0, 22.0) } else { (38.0, 26.0) }
}

pub fn toolbar(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let (height, _) = bar_heights(s.ws.settings.compact_mode);
    egui::Panel::top("main-toolbar").exact_size(height).frame(bar_frame(st)).show(ui, |ui| {
        ui.horizontal_centered(|ui| {
            // Project widget.
            let name = s.ws.project.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| "No project".into());
            let badge = name.chars().next().map(|c| c.to_ascii_uppercase().to_string()).unwrap_or_default();
            let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(4), st.theme.accent);
            ui.painter().text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                badge,
                FontId::proportional(12.0),
                Color32::WHITE,
            );
            ui.menu_button(RichText::new(format!("{name} ▾")).strong(), |ui| {
                if ui.button("Open…").clicked() {
                    ui.close();
                    run_command(s, st, "file.open", &Value::Null);
                }
                if ui.button("New Project…").clicked() {
                    ui.close();
                    run_command(s, st, "file.new.project", &Value::Null);
                }
                if s.ws.project.is_some() && ui.button("Close Project").clicked() {
                    ui.close();
                    run_command(s, st, "file.closeProject", &Value::Null);
                }
                ui.separator();
                ui.label(RichText::new("Recent Projects").color(st.theme.text_muted));
                for p in s.ws.recent_projects.clone().into_iter().take(15) {
                    let label = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    if ui.button(label).on_hover_text(p.display().to_string()).clicked() {
                        ui.close();
                        run_command(s, st, "file.open", &json!({ "path": p }));
                    }
                }
            });
            // Git branch widget (also draws the Git dialogs).
            crate::vcs_panels::branch_widget(ui, s, st);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if tool_button(ui, st, "⚙", "Settings (⌘,)", true, None) {
                    run_command(s, st, "app.settings", &Value::Null);
                }
                if tool_button(ui, st, "🔍", "Search Everywhere (⇧⇧)", true, None) {
                    run_command(s, st, "navigate.searchEverywhere", &Value::Null);
                }
                if tool_button(ui, st, "✏", "Draw Feedback for an Agent (⌥⇧⌘F)", true, Some(st.theme.annotation))
                {
                    run_command(s, st, "feedback.toggle", &Value::Null);
                }
                ui.add_space(8.0);
                let has_project = s.ws.project.is_some();
                let running = s.ws.runs.iter().any(|r| r.is_running());
                if tool_button(ui, st, "■", "Stop (⌘F2)", running, Some(st.theme.error)) {
                    run_command(s, st, "run.stop", &Value::Null);
                }
                if tool_button(ui, st, "▶", "Run (⌃R)", has_project, Some(st.theme.success)) {
                    run_command(s, st, "run.run", &Value::Null);
                }
                if tool_button(ui, st, "🔨", "Build Project", has_project, None) {
                    run_command(s, st, "build.buildProject", &Value::Null);
                }
                // Run configuration selector.
                let current = s.run_configs.get(s.selected_run_config).map(|c| c.name.clone()).unwrap_or_else(|| {
                    let cargo = s.ws.project.as_ref().is_some_and(|p| p.root.join("Cargo.toml").is_file());
                    if cargo { "cargo run".into() } else { "Current File".into() }
                });
                let mut selected = s.selected_run_config;
                let mut edit = false;
                egui::ComboBox::from_id_salt("run-config").selected_text(current).width(180.0).show_ui(ui, |ui| {
                    for (i, c) in s.run_configs.iter().enumerate() {
                        ui.selectable_value(&mut selected, i, &c.name);
                    }
                    ui.separator();
                    edit = ui.add_enabled(has_project, egui::Button::new("Edit Configurations…")).clicked();
                });
                s.selected_run_config = selected;
                if edit {
                    run_command(s, st, "run.editConfigurations", &Value::Null);
                }
            });
        });
    });
}

/// The navigation path as small clickable labels: a member moves the caret to it, the other
/// elements open the navigation bar popup (Navigate › Jump to Navigation Bar).
fn navigation_path(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, items: &[NavPathItem], size: f32) {
    let mut clicked = None;
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            ui.label(RichText::new("›").size(size).color(st.theme.text_disabled));
        }
        let text = RichText::new(&item.label).size(size).color(st.theme.text_muted);
        let resp = ui.add(egui::Label::new(text).sense(Sense::click()));
        if resp.on_hover_text(item.path.display().to_string()).clicked() {
            clicked = Some(item.offset);
        }
    }
    match clicked {
        Some(Some(offset)) => {
            run_command(s, st, "editor.setSelection", &json!({ "anchor": offset }));
            st.focus_editor = true;
        }
        Some(None) => run_command(s, st, "navigate.jumpToNavigationBar", &Value::Null),
        None => {}
    }
}

/// View › Appearance › Navigation Bar › Top: the path of the active file above the editor.
pub fn navigation_bar_top(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let items = appearance::navigation_path(s);
    if items.is_empty() {
        return;
    }
    egui::Panel::top("navigation-bar-top").exact_size(26.0).frame(bar_frame(st)).show(ui, |ui| {
        ui.horizontal_centered(|ui| navigation_path(ui, s, st, &items, 12.0));
    });
}

/// The status bar: the status text or navigation path on the left, the widgets of View ›
/// Appearance › Status Bar Widgets on the right.
pub fn status_bar(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let (_, height) = bar_heights(s.ws.settings.compact_mode);
    let widgets = s.ws.settings.clone();
    let shown = |key: &str| widget_visible(&widgets, key);
    let chrome = appearance::chrome(s);
    egui::Panel::bottom("status-bar").exact_size(height).frame(bar_frame(st)).show(ui, |ui| {
        ui.horizontal_centered(|ui| {
            let muted = st.theme.text_muted;
            // Left: the status message (Status Text widget), else the navigation path.
            if shown("statusText") && !s.status.is_empty() {
                let left: String = s.status.lines().next().unwrap_or_default().chars().take(160).collect();
                ui.label(RichText::new(left).color(muted).size(12.0));
            } else if chrome.navigation_in_status_bar {
                let items = appearance::navigation_path(s);
                navigation_path(ui, s, st, &items, 12.0);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let small = |t: String| RichText::new(t).size(12.0).color(muted);
                if shown("memoryIndicator")
                    && let Some(mb) = s.appearance.memory_mb
                {
                    ui.label(small(format!("{mb} MB"))).on_hover_text("Memory used by CodeCraft");
                }
                let info = s.ws.active().map(|(e, d)| {
                    let unit = d.doc.detect_indent_unit();
                    let indent =
                        if unit == "\t" { "Tab".to_string() } else { format!("{} spaces", s.ws.settings.tab_size) };
                    let pos = d.doc.offset_to_position(e.selections.primary().head);
                    let sel_len = e.selections.primary().end() - e.selections.primary().start();
                    let caret = if sel_len > 0 {
                        format!("{}:{} ({} chars)", pos.line + 1, pos.column + 1, sel_len)
                    } else if e.selections.len() > 1 {
                        format!("{} carets", e.selections.len())
                    } else {
                        format!("{}:{}", pos.line + 1, pos.column + 1)
                    };
                    let enc = format!("{}{}", d.encoding.label(), if d.bom { " BOM" } else { "" });
                    let column_mode = s.editing.column_mode.contains(&e.id);
                    (d.read_only, indent, d.doc.line_ending(), caret, d.language.name(), enc, column_mode)
                });
                if let Some((read_only, indent, le, caret, language, enc, column_mode)) = info {
                    if read_only && shown("readOnlyAttribute") {
                        ui.label(small("🔒".into())).on_hover_text("The file is read-only");
                    }
                    if shown("indentation") {
                        ui.label(small(indent));
                    }
                    if shown("fileEncoding")
                        && ui
                            .add(egui::Label::new(small(enc)).sense(Sense::click()))
                            .on_hover_text("File Encoding")
                            .clicked()
                    {
                        run_command(s, st, "file.fileProperties.fileEncoding", &Value::Null);
                    }
                    if shown("lineSeparator") {
                        ui.menu_button(small(le.label().into()), |ui| {
                            if ui.button("LF - Unix and macOS (\\n)").clicked() {
                                ui.close();
                                run_command(s, st, "file.fileProperties.lineSeparators.lfUnixAndMacOS", &Value::Null);
                            }
                            if ui.button("CRLF - Windows (\\r\\n)").clicked() {
                                ui.close();
                                run_command(s, st, "file.fileProperties.lineSeparators.crlfWindows", &Value::Null);
                            }
                        });
                    }
                    if shown("lineColumnNumber")
                        && ui
                            .add(egui::Label::new(small(caret)).sense(Sense::click()))
                            .on_hover_text("Go to Line:Column (⌘L)")
                            .clicked()
                    {
                        run_command(s, st, "navigate.lineColumn", &Value::Null);
                    }
                    if column_mode
                        && shown("editorSelectionMode")
                        && ui
                            .add(egui::Label::new(small("Column".into())).sense(Sense::click()))
                            .on_hover_text("Column selection mode: click to turn it off")
                            .clicked()
                    {
                        run_command(s, st, "edit.columnSelectionMode", &json!({ "on": false }));
                    }
                    ui.label(small(language.into()));
                }
                // The language server of the active file (rust-analyzer when no file is open).
                let kind = s.ws.active().and_then(|(_, d)| codecraft_engine::server_kind(d.language)).or(s
                    .ws
                    .project
                    .as_ref()
                    .and_then(|p| p.cargo.as_ref())
                    .map(|_| codecraft_engine::ServerKind::RustAnalyzer));
                if let Some(kind) = kind.filter(|_| shown("languageServices")) {
                    let status = s.server_status(kind);
                    let name = s.server_name(kind).unwrap_or(kind.label()).to_string();
                    let mem = s.server_memory_mb(kind).map(|mb| format!(" {mb} MB")).unwrap_or_default();
                    let (lsp_text, lsp_color) = match &status {
                        LspStatus::Ready => (format!("{name} ✓{mem}"), st.theme.success),
                        LspStatus::Starting => (format!("{name} …"), muted),
                        LspStatus::Unavailable(_) => (format!("{name} ✗"), st.theme.warning),
                        LspStatus::Stopped(_) => (format!("{name} ✗"), st.theme.error),
                        LspStatus::Off => (String::new(), muted),
                    };
                    if !lsp_text.is_empty() {
                        let tip = match &status {
                            LspStatus::Unavailable(m) | LspStatus::Stopped(m) => m.clone(),
                            _ => format!("{name}: click to restart"),
                        };
                        let resp = ui.add(
                            egui::Label::new(RichText::new(lsp_text).size(12.0).color(lsp_color)).sense(Sense::click()),
                        );
                        if resp.on_hover_text(tip).clicked() {
                            run_command(s, st, "lsp.restart", &Value::Null);
                        }
                    }
                }
                // Rust External Linter: cargo check / Clippy on save.
                if shown("rustExternalLinter") && s.ws.project.as_ref().is_some_and(|p| p.cargo.is_some()) {
                    let on = s.ws.settings.check_on_save;
                    let text = match (on, s.ws.settings.use_clippy) {
                        (false, _) => "Linter off",
                        (true, false) => "cargo check",
                        (true, true) => "Clippy",
                    };
                    let resp = ui.add(egui::Label::new(small(text.into())).sense(Sense::click()));
                    if resp.on_hover_text("Rust external linter on save: click to turn it on or off").clicked() {
                        run_command(s, st, "settings.set", &json!({ "key": "check_on_save", "value": !on }));
                    }
                }
                if shown("gitBranch") && s.vcs.repo.is_some() {
                    ui.label(small(format!("⎇ {}", s.vcs.head_label()))).on_hover_text("Git Branch");
                }
                if s.power_save() && shown("powerSaveMode") {
                    let resp = ui.add(
                        egui::Label::new(RichText::new("Power Save").size(12.0).color(st.theme.warning))
                            .sense(Sense::click()),
                    );
                    if resp.on_hover_text("Power Save Mode is on: click to turn it off").clicked() {
                        run_command(s, st, "file.powerSaveMode", &Value::Null);
                    }
                }
                let (errors, warnings) = s.ws.problem_counts();
                if errors + warnings > 0 {
                    let text = format!("✖ {errors}  ⚠ {warnings}");
                    let color = if errors > 0 { st.theme.error } else { st.theme.warning };
                    if ui
                        .add(egui::Label::new(RichText::new(text).size(12.0).color(color)).sense(Sense::click()))
                        .clicked()
                    {
                        s.ws.layout.show(codecraft_workspace::ToolWindowId::Problems);
                    }
                }
                if let Some(task) = s.ws.tasks.values().next() {
                    let mut t = task.title.clone();
                    if let Some(m) = &task.message {
                        t = format!("{t}: {m}");
                    }
                    if let Some(p) = task.percentage {
                        t = format!("{t} ({p}%)");
                    }
                    let t: String = t.chars().take(90).collect();
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.label(small(t));
                }
            });
        });
    });
}

pub fn welcome(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, persisted: &PersistedState) {
    let _ = persisted;
    egui::CentralPanel::default().frame(Frame::new().fill(st.theme.editor_bg)).show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.18);
            ui.label(RichText::new("CodeCraft").size(34.0).strong());
            ui.label(RichText::new("A native Rust IDE").size(14.0).color(st.theme.text_muted));
            ui.add_space(24.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 330.0).max(0.0) * 0.5);
                if ui
                    .add(egui::Button::new(RichText::new("New Project").size(14.0)).min_size(vec2(100.0, 30.0)))
                    .clicked()
                {
                    run_command(s, st, "file.new.project", &Value::Null);
                }
                if ui.add(egui::Button::new(RichText::new("Open").size(14.0)).min_size(vec2(100.0, 30.0))).clicked() {
                    run_command(s, st, "file.open", &Value::Null);
                }
                if ui.add(egui::Button::new(RichText::new("Settings").size(14.0)).min_size(vec2(100.0, 30.0))).clicked()
                {
                    run_command(s, st, "app.settings", &Value::Null);
                }
            });
            ui.add_space(28.0);
            if !s.ws.recent_projects.is_empty() {
                ui.label(RichText::new("Recent Projects").color(st.theme.text_muted));
                ui.add_space(6.0);
                for p in s.ws.recent_projects.clone().into_iter().take(12) {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let text = RichText::new(format!("{name}    {}", p.display())).size(13.0);
                    if ui.add(egui::Button::new(text).frame(false)).clicked() {
                        run_command(s, st, "file.open", &json!({ "path": p }));
                    }
                }
            }
            if let LspStatus::Unavailable(m) = &s.lsp_status {
                ui.add_space(20.0);
                ui.label(RichText::new(m).color(st.theme.warning).size(12.0));
            }
        });
    });
}

/// The editor groups (split editors), each with its tab bar; the focused group also gets the
/// find bar and the database console toolbar.
pub fn editor_area(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    egui::CentralPanel::default().frame(Frame::new().fill(st.theme.editor_bg)).show(ui, |ui| {
        let area = ui.available_rect_before_wrap();
        ui.advance_cursor_after_rect(area);
        let mut groups = Vec::new();
        let mut splitters = Vec::new();
        layout_groups(s.ws.split_layout(), area, &mut 0, &mut groups, &mut splitters);
        for sp in &splitters {
            splitter(ui, s, st, sp);
        }
        // A press inside another group focuses it before its editor turns the press into commands
        // (not a press on a popup or window floating over it).
        let press = ui.input(|i| {
            if i.pointer.primary_pressed() || i.pointer.secondary_pressed() { i.pointer.interact_pos() } else { None }
        });
        if let Some(pos) = press
            && ui.ctx().layer_id_at(pos) == Some(ui.layer_id())
            && let Some((g, _)) = groups.iter().find(|(_, r)| r.contains(pos))
            && *g != s.ws.focused_group()
        {
            run_command(s, st, "window.editorTabs.focusGroup", &json!({ "group": g.0 }));
        }
        let focused = s.ws.focused_group();
        let split = groups.len() > 1;
        let mut tab_bars = Vec::new();
        // The focused group last, so the caret position popups use is its editor's.
        groups.sort_by_key(|(g, _)| *g == focused);
        for (g, rect) in &groups {
            let bar = ui
                .scope_builder(egui::UiBuilder::new().max_rect(*rect).id_salt(("editor-group", g.0)), |ui| {
                    ui.set_clip_rect(rect.intersect(ui.clip_rect()));
                    group(ui, s, st, *g, *g == focused, split)
                })
                .inner;
            tab_bars.push((*g, *rect, bar));
        }
        tab_drop(ui, s, st, &tab_bars);
    });
    // Files dropped on the window open in editors.
    let dropped: Vec<std::path::PathBuf> =
        ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
    for p in dropped {
        run_command(s, st, "file.open", &json!({ "path": p }));
    }
}

/// A splitter between two groups: `index` in pre-order (as the workspace numbers them).
struct Splitter {
    index: usize,
    axis: SplitAxis,
    outer: Rect,
    divider: Rect,
}

const SPLITTER_W: f32 = 5.0;

fn layout_groups(
    node: &SplitNode,
    rect: Rect,
    next: &mut usize,
    groups: &mut Vec<(GroupId, Rect)>,
    splitters: &mut Vec<Splitter>,
) {
    match node {
        SplitNode::Group(g) => groups.push((*g, rect)),
        SplitNode::Split { axis, ratio, first, second } => {
            let index = *next;
            *next += 1;
            let half = SPLITTER_W / 2.0;
            let (a, b, divider) = match axis {
                SplitAxis::Horizontal => {
                    let x = rect.left() + rect.width() * ratio;
                    (
                        Rect::from_min_max(rect.min, pos2(x - half, rect.bottom())),
                        Rect::from_min_max(pos2(x + half, rect.top()), rect.max),
                        Rect::from_min_max(pos2(x - half, rect.top()), pos2(x + half, rect.bottom())),
                    )
                }
                SplitAxis::Vertical => {
                    let y = rect.top() + rect.height() * ratio;
                    (
                        Rect::from_min_max(rect.min, pos2(rect.right(), y - half)),
                        Rect::from_min_max(pos2(rect.left(), y + half), rect.max),
                        Rect::from_min_max(pos2(rect.left(), y - half), pos2(rect.right(), y + half)),
                    )
                }
            };
            splitters.push(Splitter { index, axis: *axis, outer: rect, divider });
            layout_groups(first, a, next, groups, splitters);
            layout_groups(second, b, next, groups, splitters);
        }
    }
}

/// Draw a splitter and let the user drag it (pure view state: the ratio is set directly).
fn splitter(ui: &mut egui::Ui, s: &mut Session, st: &UiState, sp: &Splitter) {
    let resp = ui.interact(sp.divider, egui::Id::new(("editor-splitter", sp.index)), Sense::drag());
    let horizontal = sp.axis == SplitAxis::Horizontal;
    if resp.hovered() || resp.dragged() {
        ui.ctx().set_cursor_icon(if horizontal {
            egui::CursorIcon::ResizeHorizontal
        } else {
            egui::CursorIcon::ResizeVertical
        });
    }
    if resp.dragged()
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let ratio = if horizontal {
            (pos.x - sp.outer.left()) / sp.outer.width().max(1.0)
        } else {
            (pos.y - sp.outer.top()) / sp.outer.height().max(1.0)
        };
        s.ws.set_split_ratio(sp.index, ratio);
    }
    let color = if resp.dragged() { st.theme.accent } else { st.theme.border };
    ui.painter().rect_filled(sp.divider, CornerRadius::ZERO, st.theme.panel_bg);
    let c = sp.divider.center();
    if horizontal {
        ui.painter().vline(c.x, sp.divider.y_range(), Stroke::new(1.0, color));
    } else {
        ui.painter().hline(sp.divider.x_range(), c.y, Stroke::new(1.0, color));
    }
}

/// One group: tab bar, (focused only) find bar and console toolbar, then the editor.
/// Returns the tab bar's rect and its tabs' rects (for dropping dragged tabs).
fn group(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, g: GroupId, focused: bool, split: bool) -> TabBar {
    let bar = if s.ws.settings.show_editor_tabs {
        tabs(ui, s, st, g, focused, split)
    } else {
        TabBar { rect: Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 0.0)), tabs: Vec::new() }
    };
    if focused {
        if s.find_bar.is_some() {
            find_bar(ui, s, st);
        } else {
            st.find_bar_was_open = false;
        }
        crate::database_panel::console_toolbar(ui, s, st);
    }
    match s.ws.group(g).and_then(|gr| gr.active) {
        Some(eid) => editor::show(ui, s, st, eid),
        None => {
            ui.centered_and_justified(|ui| {
                let hints = [
                    ("Search Everywhere", "Double ⇧"),
                    ("Go to File", "⇧⌘O"),
                    ("Recent Files", "⌘E"),
                    ("Navigation Bar", "⌘↑"),
                    ("Drop files here to open them", ""),
                ];
                let text = hints.iter().map(|(a, b)| format!("{a}   {b}")).collect::<Vec<_>>().join("\n\n");
                ui.label(RichText::new(text).color(st.theme.text_muted).size(14.0));
            });
        }
    }
    bar
}

struct TabBar {
    rect: Rect,
    tabs: Vec<(EditorId, Rect)>,
}

/// Finish a tab drag: dropping on another group (its tab bar or editor) moves the tab there;
/// dropping on a tab bar inserts it at that position.
fn tab_drop(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, bars: &[(GroupId, Rect, TabBar)]) {
    let ctx = ui.ctx().clone();
    let Some(dragged) = egui::DragAndDrop::payload::<EditorId>(&ctx) else { return };
    let Some(pos) = ctx.pointer_latest_pos() else { return };
    let Some((g, rect, bar)) = bars.iter().find(|(_, r, _)| r.contains(pos)) else { return };
    let same_group = s.ws.group_of(*dragged) == Some(*g);
    let index = if bar.rect.contains(pos) {
        bar.tabs.iter().position(|(_, r)| pos.x < r.center().x).unwrap_or(bar.tabs.len())
    } else if same_group {
        return;
    } else {
        usize::MAX
    };
    // Where it would land.
    let marker = if bar.rect.contains(pos) {
        let x = bar.tabs.get(index).map(|(_, r)| r.left()).or_else(|| bar.tabs.last().map(|(_, r)| r.right()));
        x.map(|x| Rect::from_min_max(pos2(x - 1.0, bar.rect.top()), pos2(x + 1.0, bar.rect.bottom())))
    } else {
        Some(*rect)
    };
    if let Some(m) = marker {
        let fill = st.theme.accent.gamma_multiply(if m.width() > 4.0 { 0.12 } else { 1.0 });
        ui.painter().rect_filled(m, CornerRadius::ZERO, fill);
    }
    if ui.input(|i| i.pointer.any_released())
        && let Some(id) = egui::DragAndDrop::take_payload::<EditorId>(&ctx)
    {
        // Within one bar, dropping right of itself means one slot further left once it is removed.
        let index = match s.ws.group(*g).and_then(|gr| gr.tabs.iter().position(|t| *t == *id)) {
            Some(from) if same_group && index > from => index - 1,
            _ => index,
        };
        let index = if index == usize::MAX { Value::Null } else { json!(index) };
        run_command(s, st, "window.editorTabs.moveTab", &json!({ "editor": id.0, "group": g.0, "index": index }));
    }
}

fn tabs(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, g: GroupId, focused: bool, split: bool) -> TabBar {
    let theme = st.theme.clone();
    let tab_ids: Vec<EditorId> = s.ws.group(g).map(|gr| gr.tabs.clone()).unwrap_or_default();
    let active_tab = s.ws.group(g).and_then(|gr| gr.active);
    let mut rects = Vec::new();
    let frame = Frame::new().fill(theme.panel_bg).inner_margin(Margin::symmetric(4, 0)).show(ui, |ui| {
        ui.set_height(32.0);
        ui.set_width(ui.available_width());
        egui::ScrollArea::horizontal().id_salt(("editor-tabs", g.0)).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let mut activate = None;
                let mut close = None;
                let mut keep = None;
                for id in tab_ids {
                    let Some(e) = s.ws.editor(id) else { continue };
                    let Some(d) = s.ws.doc(e.doc) else { continue };
                    let active = active_tab == Some(id);
                    let has_errors = d
                        .path
                        .as_deref()
                        .is_some_and(|p| s.ws.problems_for(p).iter().any(|p| p.severity == Severity::Error));
                    let mut title = d.title();
                    if e.pinned {
                        title = format!("📌 {title}");
                    }
                    let vcs_color = d.path.as_deref().and_then(|p| crate::vcs_panels::file_color(s, &theme, p));
                    let color = if has_errors {
                        theme.error
                    } else if let Some(c) = vcs_color {
                        c
                    } else if active {
                        theme.text
                    } else {
                        theme.text_muted
                    };
                    let text = RichText::new(title).size(13.0).color(color);
                    let text = if has_errors { text.underline() } else { text };
                    // A preview tab (single click in the Project view) is shown in italics.
                    let text = if e.preview { text.italics() } else { text };
                    let resp = ui
                        .add(
                            egui::Button::new(text)
                                .frame(false)
                                .min_size(vec2(60.0, 28.0))
                                .sense(Sense::click_and_drag()),
                        )
                        .on_hover_text(d.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default());
                    resp.dnd_set_drag_payload(id);
                    if active {
                        let r = resp.rect;
                        let accent = if focused { theme.accent } else { theme.text_muted };
                        ui.painter().hline(r.x_range(), r.bottom() + 1.0, Stroke::new(2.0, accent));
                    }
                    if resp.clicked() {
                        activate = Some(id);
                    }
                    if resp.double_clicked() {
                        keep = Some(id);
                    }
                    if resp.middle_clicked() {
                        close = Some(id);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Close").clicked() {
                            close = Some(id);
                            ui.close();
                        }
                        let mut items = vec![
                            ("window.editorTabs.closeOtherTabs", "Close Other Tabs"),
                            ("window.editorTabs.closeAllTabs", "Close All Tabs"),
                            ("window.editorTabs.closeTabsToTheLeft", "Close Tabs to the Left"),
                            ("window.editorTabs.closeTabsToTheRight", "Close Tabs to the Right"),
                            ("window.editorTabs.pinTab", "Pin Tab"),
                            ("---", ""),
                            ("window.editorTabs.splitRight", "Split Right"),
                            ("window.editorTabs.splitDown", "Split Down"),
                            ("window.editorTabs.splitAndMoveRight", "Split and Move Right"),
                            ("window.editorTabs.splitAndMoveDown", "Split and Move Down"),
                        ];
                        if split {
                            items.push(("window.editorTabs.unsplit", "Unsplit"));
                            items.push(("window.editorTabs.unsplitAll", "Unsplit All"));
                        }
                        for (cmd, label) in items {
                            if cmd == "---" {
                                ui.separator();
                            } else if ui.button(label).clicked() {
                                s.ws.set_active_editor(id);
                                run_command(s, st, cmd, &Value::Null);
                                ui.close();
                            }
                        }
                    });
                    let x = ui
                        .add(egui::Button::new(RichText::new("×").color(theme.text_muted)).frame(false))
                        .on_hover_text("Close (⌘W)");
                    if x.clicked() {
                        close = Some(id);
                    }
                    rects.push((id, resp.rect.union(x.rect)));
                    ui.add_space(6.0);
                }
                if let Some(id) = activate {
                    run_command(s, st, "window.editorTabs.activate", &json!({ "editor": id.0 }));
                }
                if let Some(id) = close {
                    run_command(s, st, "window.editorTabs.close", &json!({ "editor": id.0 }));
                }
                if let Some(id) = keep {
                    run_command(s, st, "window.editorTabs.keepTabOpen", &json!({ "editor": id.0 }));
                }
            });
        });
    });
    TabBar { rect: frame.response.rect, tabs: rects }
}

fn find_bar(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let Some(mut bar) = s.find_bar.clone() else { return };
    let just_opened = !st.find_bar_was_open;
    st.find_bar_was_open = true;
    Frame::new().fill(theme.panel_bg).inner_margin(Margin::symmetric(8, 4)).show(ui, |ui| {
        let mut changed = false;
        let mut next = None;
        ui.horizontal(|ui| {
            let resp = ui.add(
                TextEdit::singleline(&mut bar.query.pattern)
                    .hint_text("Find")
                    .desired_width(280.0)
                    .font(FontId::monospace(13.0)),
            );
            if just_opened {
                resp.request_focus();
            }
            changed |= resp.changed();
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                next = Some(!ui.input(|i| i.modifiers.shift));
                resp.request_focus();
            }
            if resp.has_focus() && ui.input(|i| i.key_pressed(Key::Escape)) {
                s.find_bar = None;
                st.focus_editor = true;
                return;
            }
            for (flag, label, tip) in [
                (&mut bar.query.case_sensitive, "Cc", "Match Case"),
                (&mut bar.query.whole_word, "W", "Words"),
                (&mut bar.query.regex, ".*", "Regex"),
            ] {
                let text = RichText::new(label).color(if *flag { theme.accent } else { theme.text_muted }).monospace();
                if ui.add(egui::Button::new(text).selected(*flag)).on_hover_text(tip).clicked() {
                    *flag = !*flag;
                    changed = true;
                }
            }
            let count = bar.matches.len();
            let info = match (&bar.error, count, bar.current) {
                (Some(e), _, _) => {
                    RichText::new(e.lines().last().unwrap_or("bad regex").to_string()).color(theme.error)
                }
                (None, 0, _) if !bar.query.pattern.is_empty() => RichText::new("0 results").color(theme.error),
                (None, n, Some(c)) => RichText::new(format!("{}/{n}", c + 1)).color(theme.text_muted),
                (None, n, None) => RichText::new(format!("{n} results")).color(theme.text_muted),
            };
            ui.label(info.size(12.0));
            if ui.button("↑").on_hover_text("Previous Occurrence (⇧⌘G)").clicked() {
                next = Some(false);
            }
            if ui.button("↓").on_hover_text("Next Occurrence (⌘G)").clicked() {
                next = Some(true);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("×").clicked() {
                    s.find_bar = None;
                    st.focus_editor = true;
                }
            });
        });
        if let Some(mut replace) = bar.replace.clone() {
            let mut action = None;
            ui.horizontal(|ui| {
                let resp = ui.add(
                    TextEdit::singleline(&mut replace)
                        .hint_text("Replace")
                        .desired_width(280.0)
                        .font(FontId::monospace(13.0)),
                );
                changed |= resp.changed();
                if ui.button("Replace").clicked() {
                    action = Some("edit.find.replaceCurrent");
                }
                if ui.button("Replace All").clicked() {
                    action = Some("edit.find.replaceAll");
                }
            });
            bar.replace = Some(replace.clone());
            if let Some(cmd) = action {
                next = None;
                let _ = s.execute("edit.find.setQuery", &json!({ "replace": replace }));
                run_command(s, st, cmd, &Value::Null);
            }
        }
        if changed && s.find_bar.is_some() {
            let params = json!({
                "pattern": bar.query.pattern,
                "caseSensitive": bar.query.case_sensitive,
                "wholeWord": bar.query.whole_word,
                "regex": bar.query.regex,
                "replace": bar.replace.clone(),
            });
            run_command(s, st, "edit.find.setQuery", &params);
            if bar.replace.is_none()
                && let Some(b) = &mut s.find_bar
            {
                b.replace = None;
            }
        }
        if let Some(forward) = next {
            run_command(
                s,
                st,
                if forward { "edit.find.findNextOccurrence" } else { "edit.find.findPreviousOccurrence" },
                &Value::Null,
            );
        }
    });
}
