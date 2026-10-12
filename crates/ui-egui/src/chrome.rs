//! Window chrome: menu bar, main toolbar, status bar, welcome screen, editor tabs and find bar.

use egui::{Align, Color32, CornerRadius, FontId, Frame, Key, Layout, Margin, RichText, Sense, Stroke, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::{LspStatus, Session};
use codecraft_workspace::Severity;

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

pub fn toolbar(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    egui::Panel::top("main-toolbar").exact_size(38.0).frame(bar_frame(st)).show(ui, |ui| {
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
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if tool_button(ui, st, "⚙", "Settings (⌘,)", true, None) {
                    run_command(s, st, "app.settings", &Value::Null);
                }
                if tool_button(ui, st, "🔍", "Search Everywhere (⇧⇧)", true, None) {
                    run_command(s, st, "navigate.searchEverywhere", &Value::Null);
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
                let current = s
                    .run_configs
                    .get(s.selected_run_config)
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "cargo run".into());
                let mut selected = s.selected_run_config;
                egui::ComboBox::from_id_salt("run-config").selected_text(current).width(180.0).show_ui(ui, |ui| {
                    for (i, c) in s.run_configs.iter().enumerate() {
                        ui.selectable_value(&mut selected, i, &c.name);
                    }
                });
                s.selected_run_config = selected;
            });
        });
    });
}

pub fn status_bar(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    egui::Panel::bottom("status-bar").exact_size(26.0).frame(bar_frame(st)).show(ui, |ui| {
        ui.horizontal_centered(|ui| {
            let muted = st.theme.text_muted;
            // Left: navigation path or status message.
            let left = if !s.status.is_empty() {
                s.status.clone()
            } else if let (Some(p), Some((_, d))) = (&s.ws.project, s.ws.active()) {
                d.path
                    .as_deref()
                    .map(|path| format!("{} › {}", p.name, p.relative(path).replace('/', " › ")))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let left: String = left.lines().next().unwrap_or_default().chars().take(160).collect();
            ui.label(RichText::new(left).color(muted).size(12.0));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let small = |t: String| RichText::new(t).size(12.0).color(muted);
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
                    (d.read_only, indent, d.doc.line_ending(), caret, d.language.name())
                });
                if let Some((read_only, indent, le, caret, language)) = info {
                    if read_only {
                        ui.label(small("🔒".into()));
                    }
                    ui.label(small(indent));
                    ui.label(small("UTF-8".into()));
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
                    if ui
                        .add(egui::Label::new(small(caret)).sense(Sense::click()))
                        .on_hover_text("Go to Line:Column (⌘L)")
                        .clicked()
                    {
                        run_command(s, st, "navigate.lineColumn", &Value::Null);
                    }
                    ui.label(small(language.into()));
                }
                let (lsp_text, lsp_color) = match &s.lsp_status {
                    LspStatus::Ready => ("rust-analyzer ✓".to_string(), st.theme.success),
                    LspStatus::Starting => ("rust-analyzer …".to_string(), muted),
                    LspStatus::Unavailable(_) => ("rust-analyzer ✗".to_string(), st.theme.warning),
                    LspStatus::Stopped(_) => ("rust-analyzer ✗".to_string(), st.theme.error),
                    LspStatus::Off => (String::new(), muted),
                };
                if !lsp_text.is_empty() {
                    let tip = match &s.lsp_status {
                        LspStatus::Unavailable(m) | LspStatus::Stopped(m) => m.clone(),
                        _ => "rust-analyzer".into(),
                    };
                    ui.label(RichText::new(lsp_text).size(12.0).color(lsp_color)).on_hover_text(tip);
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

/// Editor tabs, find bar and the active editor.
pub fn editor_area(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    egui::CentralPanel::default().frame(Frame::new().fill(st.theme.editor_bg)).show(ui, |ui| {
        tabs(ui, s, st);
        if s.find_bar.is_some() {
            find_bar(ui, s, st);
        } else {
            st.find_bar_was_open = false;
        }
        match s.ws.active_editor {
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
    });
    // Files dropped on the window open in editors.
    let dropped: Vec<std::path::PathBuf> =
        ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
    for p in dropped {
        run_command(s, st, "file.open", &json!({ "path": p }));
    }
}

fn tabs(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    Frame::new().fill(theme.panel_bg).inner_margin(Margin::symmetric(4, 0)).show(ui, |ui| {
        ui.set_height(32.0);
        egui::ScrollArea::horizontal().id_salt("editor-tabs").show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let mut activate = None;
                let mut close = None;
                for id in s.ws.tabs.clone() {
                    let Some(e) = s.ws.editor(id) else { continue };
                    let Some(d) = s.ws.doc(e.doc) else { continue };
                    let active = s.ws.active_editor == Some(id);
                    let has_errors = d
                        .path
                        .as_deref()
                        .is_some_and(|p| s.ws.problems_for(p).iter().any(|p| p.severity == Severity::Error));
                    let mut title = d.title();
                    if e.pinned {
                        title = format!("📌 {title}");
                    }
                    let color = if has_errors {
                        theme.error
                    } else if active {
                        theme.text
                    } else {
                        theme.text_muted
                    };
                    let text = RichText::new(title).size(13.0).color(color);
                    let text = if has_errors { text.underline() } else { text };
                    let resp = ui
                        .add(egui::Button::new(text).frame(false).min_size(vec2(60.0, 28.0)))
                        .on_hover_text(d.path.as_deref().map(|p| p.display().to_string()).unwrap_or_default());
                    if active {
                        let r = resp.rect;
                        ui.painter().hline(r.x_range(), r.bottom() + 1.0, Stroke::new(2.0, theme.accent));
                    }
                    if resp.clicked() {
                        activate = Some(id);
                    }
                    if resp.middle_clicked() {
                        close = Some(id);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Close").clicked() {
                            close = Some(id);
                            ui.close();
                        }
                        for (cmd, label) in [
                            ("window.editorTabs.closeOtherTabs", "Close Other Tabs"),
                            ("window.editorTabs.closeAllTabs", "Close All Tabs"),
                            ("window.editorTabs.closeTabsToTheLeft", "Close Tabs to the Left"),
                            ("window.editorTabs.closeTabsToTheRight", "Close Tabs to the Right"),
                            ("window.editorTabs.pinTab", "Pin Tab"),
                        ] {
                            if ui.button(label).clicked() {
                                s.ws.active_editor = Some(id);
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
                    ui.add_space(6.0);
                }
                if let Some(id) = activate {
                    run_command(s, st, "window.editorTabs.activate", &json!({ "editor": id.0 }));
                }
                if let Some(id) = close {
                    run_command(s, st, "window.editorTabs.close", &json!({ "editor": id.0 }));
                }
            });
        });
    });
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
