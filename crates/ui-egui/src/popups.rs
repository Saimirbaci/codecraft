//! Popups: the engine owns their state (`Session::popup`); this module draws them and maps
//! keys/clicks to commands.

use std::path::PathBuf;

use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, Color32, FontId, Frame, Key, Margin, Modifiers, RichText, ScrollArea, Sense, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::popup::NewItemKind;
use codecraft_engine::{Popup, Session, registry};

use crate::{UiState, keymap, markdown, run_command};

/// One selectable row of a list popup.
struct Row {
    title: String,
    /// Matched char indices in `title` (highlighted).
    highlight: Vec<usize>,
    detail: String,
    icon: &'static str,
    action: RowAction,
    enabled: bool,
}

#[derive(Clone)]
enum RowAction {
    Open { path: PathBuf, line: usize, column: usize },
    Command { id: String, params: Value },
    CompletionIndex(usize),
}

/// Popups that take the keyboard away from the editor.
pub fn wants_keyboard(s: &Session) -> bool {
    s.popup.is_some()
}

fn list_len(s: &Session, st: &mut UiState) -> usize {
    rows(s, st).len()
}

fn selected_mut(popup: &mut Popup) -> Option<&mut usize> {
    match popup {
        Popup::GotoFile { selected, .. }
        | Popup::GotoSymbol { selected, .. }
        | Popup::SearchEverywhere { selected, .. }
        | Popup::FindAction { selected, .. }
        | Popup::RecentFiles { selected, .. }
        | Popup::RecentLocations { selected }
        | Popup::FileStructure { selected, .. }
        | Popup::Actions { selected, .. }
        | Popup::Locations { selected, .. }
        | Popup::RunConfigs { selected } => Some(selected),
        Popup::Completion(c) => Some(&mut c.selected),
        _ => None,
    }
}

/// Up/Down/Enter/Escape for list popups (consumed before text fields see them).
pub fn handle_keys(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
    if pressed(Key::Escape) {
        s.popup = None;
        st.focus_editor = true;
        return;
    }
    if matches!(s.popup, Some(Popup::Completion(_))) {
        // The editor's own commands drive completion (Up/Down/Enter/Tab are editor actions).
        return;
    }
    let n = list_len(s, st);
    let mut moved = 0i64;
    if pressed(Key::ArrowDown) {
        moved = 1;
    }
    if pressed(Key::ArrowUp) {
        moved = -1;
    }
    if pressed(Key::PageDown) {
        moved = 10;
    }
    if pressed(Key::PageUp) {
        moved = -10;
    }
    if moved != 0
        && n > 0
        && let Some(sel) = s.popup.as_mut().and_then(selected_mut)
    {
        *sel = usize::try_from((*sel as i64 + moved).rem_euclid(n as i64)).unwrap_or(0);
    }
    if pressed(Key::Enter) {
        accept(s, st, None);
    }
}

fn file_rows(s: &Session, st: &mut UiState, query: &str, limit: usize) -> Vec<Row> {
    let cached = st.goto_file_cache.as_ref().filter(|(q, _)| q == query).map(|(_, r)| r.clone());
    let results = match cached {
        Some(r) => r,
        None => {
            let r = s.goto_file_results(query, 200);
            st.goto_file_cache = Some((query.to_string(), r.clone()));
            r
        }
    };
    let line_col =
        query.rsplit_once(':').and_then(|(_, l)| l.parse::<usize>().ok()).map(|l| l.saturating_sub(1)).unwrap_or(0);
    results
        .into_iter()
        .take(limit)
        .map(|(path, rel, idx)| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // Map match indices from the relative path to the file name part.
            let offset = rel.chars().count().saturating_sub(name.chars().count());
            let highlight = idx.into_iter().filter_map(|i| i.checked_sub(offset)).collect();
            Row {
                title: name,
                highlight,
                detail: rel,
                icon: "◆",
                action: RowAction::Open { path, line: line_col, column: 0 },
                enabled: true,
            }
        })
        .collect()
}

fn action_rows(s: &Session, query: &str, limit: usize) -> Vec<Row> {
    let reg = registry();
    let mut scored: Vec<(i64, Row)> = reg
        .all()
        .iter()
        .filter(|c| {
            !c.id.starts_with("editor.") && !c.id.contains("Query") && c.params.is_empty()
                || reg.catalog_item(c.id).is_some()
        })
        .filter_map(|c| {
            let m = codecraft_fs::fuzzy_match(query, c.label)?;
            let menu = reg.menu_path(c.id).map(|p| p.join(" › ")).unwrap_or_default();
            let sc = keymap::shortcut_label(c.id).unwrap_or_default();
            Some((
                m.score,
                Row {
                    title: c.label.to_string(),
                    highlight: m.indices,
                    detail: format!("{menu}   {sc}"),
                    icon: "⚡",
                    action: RowAction::Command { id: c.id.to_string(), params: Value::Null },
                    enabled: s.enabled(c.id).is_ok(),
                },
            ))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.title.len().cmp(&b.1.title.len())));
    scored.into_iter().take(limit).map(|(_, r)| r).collect()
}

fn symbol_rows(items: &[codecraft_engine::popup::SymbolItem], query: &str, s: &Session) -> Vec<Row> {
    items
        .iter()
        .take(200)
        .map(|i| {
            let rel =
                s.ws.project
                    .as_ref()
                    .map(|p| p.relative(&i.path).into_owned())
                    .unwrap_or_else(|| i.path.display().to_string());
            Row {
                title: i.name.clone(),
                highlight: codecraft_fs::fuzzy_match(query, &i.name).map(|m| m.indices).unwrap_or_default(),
                detail: format!("{}  {}  {rel}:{}", i.kind, i.container.clone().unwrap_or_default(), i.line + 1),
                icon: "ƒ",
                action: RowAction::Open { path: i.path.clone(), line: i.line, column: i.column },
                enabled: true,
            }
        })
        .collect()
}

fn rows(s: &Session, st: &mut UiState) -> Vec<Row> {
    let Some(popup) = &s.popup else { return Vec::new() };
    match popup {
        Popup::GotoFile { query, .. } => file_rows(s, st, query, 50),
        Popup::GotoSymbol { query, items, .. } => symbol_rows(items, query, s),
        Popup::SearchEverywhere { query, symbols, .. } => {
            let mut v = Vec::new();
            if !query.is_empty() {
                v.extend(symbol_rows(symbols, query, s).into_iter().take(8));
            }
            v.extend(file_rows(s, st, query, 10));
            if !query.is_empty() {
                v.extend(action_rows(s, query, 8));
            }
            v
        }
        Popup::FindAction { query, .. } => action_rows(s, query, 60),
        Popup::RecentFiles { query, .. } => {
            s.ws.recent_files
                .iter()
                .filter_map(|p| {
                    let name = p.file_name()?.to_string_lossy().into_owned();
                    let m = codecraft_fs::fuzzy_match(query, &name)?;
                    let rel =
                        s.ws.project
                            .as_ref()
                            .map(|pr| pr.relative(p).into_owned())
                            .unwrap_or_else(|| p.display().to_string());
                    Some(Row {
                        title: name,
                        highlight: m.indices,
                        detail: rel,
                        icon: "◆",
                        action: RowAction::Open { path: p.clone(), line: usize::MAX, column: 0 },
                        enabled: true,
                    })
                })
                .take(50)
                .collect()
        }
        Popup::RecentLocations { .. } => {
            s.ws.history()
                .iter()
                .rev()
                .take(50)
                .map(|loc| {
                    let (line, preview) = match s.ws.doc_by_path(&loc.path).and_then(|d| s.ws.doc(d)) {
                        Some(d) => {
                            let l = d.doc.char_to_line(loc.offset);
                            (l, d.doc.line_text(l).unwrap_or_default().trim().to_string())
                        }
                        None => (0, String::new()),
                    };
                    let name = loc.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    Row {
                        title: format!("{name}:{}", line + 1),
                        highlight: Vec::new(),
                        detail: preview,
                        icon: "↩",
                        action: RowAction::Open { path: loc.path.clone(), line, column: 0 },
                        enabled: true,
                    }
                })
                .collect()
        }
        Popup::FileStructure { query, items, .. } => {
            let path = s.ws.active().and_then(|(_, d)| d.path.clone()).unwrap_or_default();
            items
                .iter()
                .filter_map(|i| {
                    let m = codecraft_fs::fuzzy_match(query, &i.name)?;
                    Some(Row {
                        title: format!("{}{}", "  ".repeat(i.depth), i.name),
                        highlight: m.indices.iter().map(|x| x + 2 * i.depth).collect(),
                        detail: format!("{} {}", i.kind, i.detail.clone().unwrap_or_default()),
                        icon: "•",
                        action: RowAction::Open { path: path.clone(), line: i.line, column: i.column },
                        enabled: true,
                    })
                })
                .collect()
        }
        Popup::Actions { items, .. } => items
            .iter()
            .enumerate()
            .map(|(i, a)| Row {
                title: a.title.clone(),
                highlight: Vec::new(),
                detail: a.kind.clone().unwrap_or_default(),
                icon: "💡",
                action: RowAction::Command { id: "code.applyContextAction".into(), params: json!({ "index": i }) },
                enabled: true,
            })
            .collect(),
        Popup::Locations { items, .. } => items
            .iter()
            .map(|l| Row {
                title: format!(
                    "{}:{}",
                    l.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                    l.line + 1
                ),
                highlight: Vec::new(),
                detail: l.preview.clone(),
                icon: "→",
                action: RowAction::Open { path: l.path.clone(), line: l.line, column: l.column },
                enabled: true,
            })
            .collect(),
        Popup::RunConfigs { .. } => s
            .run_configs
            .iter()
            .enumerate()
            .map(|(i, c)| Row {
                title: c.name.clone(),
                highlight: Vec::new(),
                detail: c.invocation.command_line(),
                icon: "▶",
                action: RowAction::Command { id: "run.run".into(), params: json!({ "config": i }) },
                enabled: true,
            })
            .collect(),
        Popup::Completion(c) => c
            .visible
            .iter()
            .filter_map(|i| c.items.get(*i).map(|it| (i, it)))
            .take(300)
            .map(|(i, it)| Row {
                title: it.label.clone(),
                highlight: Vec::new(),
                detail: it.detail.clone().unwrap_or_default(),
                icon: completion_icon(&it.kind),
                action: RowAction::CompletionIndex(*i),
                enabled: true,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn completion_icon(kind: &str) -> &'static str {
    match kind {
        "fn" | "method" => "ƒ",
        "struct" | "enum" | "trait" | "type" => "S",
        "field" => "f",
        "variable" => "v",
        "mod" => "m",
        "keyword" => "k",
        "const" => "c",
        "variant" => "e",
        "snippet" => "⌗",
        _ => "·",
    }
}

fn accept(s: &mut Session, st: &mut UiState, index: Option<usize>) {
    if let Some(Popup::Completion(c)) = &s.popup {
        let idx = index.unwrap_or(c.selected);
        run_command(s, st, "editor.completion.accept", &json!({ "index": idx }));
        return;
    }
    match &s.popup {
        Some(Popup::GotoLine { input }) => {
            let mut parts = input.split(':');
            let line = parts.next().and_then(|l| l.trim().parse::<usize>().ok());
            let column = parts.next().and_then(|c| c.trim().parse::<usize>().ok()).unwrap_or(1);
            if let Some(line) = line {
                s.popup = None;
                run_command(s, st, "navigate.lineColumn", &json!({ "line": line, "column": column }));
                st.focus_editor = true;
            }
            return;
        }
        Some(Popup::Rename { name, .. }) => {
            let name = name.clone();
            run_command(s, st, "refactor.rename", &json!({ "newName": name }));
            st.focus_editor = true;
            return;
        }
        Some(Popup::NewItem { kind, dir, name }) => {
            let path = dir.join(name.trim());
            let cmd = match kind {
                NewItemKind::File => "file.new.file",
                NewItemKind::Directory => "file.new.directory",
                NewItemKind::CargoCrate => "file.new.cargoCrate",
            };
            if !name.trim().is_empty() {
                s.popup = None;
                run_command(s, st, cmd, &json!({ "path": path }));
            }
            return;
        }
        Some(Popup::FindInFiles { query, mask, replace }) => {
            let params = json!({
                "pattern": query.pattern, "caseSensitive": query.case_sensitive, "wholeWord": query.whole_word,
                "regex": query.regex, "mask": mask, "replace": replace,
            });
            let cmd = if replace.is_some() { "edit.find.replaceInFilesRun" } else { "edit.find.findInFilesRun" };
            s.popup = None;
            run_command(s, st, cmd, &params);
            return;
        }
        _ => {}
    }
    let list = rows(s, st);
    let sel = index.or_else(|| s.popup.as_mut().and_then(selected_mut).map(|x| *x)).unwrap_or(0);
    let Some(row) = list.get(sel) else { return };
    if !row.enabled {
        return;
    }
    let action = row.action.clone();
    let keep = matches!(s.popup, Some(Popup::Actions { .. }));
    if !keep {
        s.popup = None;
    }
    match action {
        RowAction::Open { path, line, column } => {
            if line == usize::MAX {
                run_command(s, st, "file.open", &json!({ "path": path }));
            } else {
                run_command(s, st, "navigate.openLocation", &json!({ "path": path, "line": line, "column": column }));
            }
            st.focus_editor = true;
        }
        RowAction::Command { id, params } => {
            run_command(s, st, &id, &params);
            if !matches!(
                s.popup,
                Some(Popup::Rename { .. }) | Some(Popup::GotoLine { .. }) | Some(Popup::NewItem { .. })
            ) {
                st.focus_editor = true;
            }
        }
        RowAction::CompletionIndex(i) => run_command(s, st, "editor.completion.accept", &json!({ "index": i })),
    }
}

fn frame(st: &UiState) -> Frame {
    Frame::new()
        .fill(st.theme.popup_bg)
        .stroke(egui::Stroke::new(1.0, st.theme.separator))
        .corner_radius(8)
        .inner_margin(Margin::same(8))
        .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(120) })
}

fn title_job(row: &Row, theme: &crate::theme::Theme, selected: bool) -> LayoutJob {
    let mut job = LayoutJob::default();
    let color = if row.enabled { theme.text } else { theme.text_disabled };
    job.append(
        &format!("{}  ", row.icon),
        0.0,
        TextFormat { color: theme.text_muted, font_id: FontId::proportional(12.0), ..Default::default() },
    );
    for (i, c) in row.title.chars().enumerate() {
        let hl = row.highlight.contains(&i);
        let mut fmt = TextFormat {
            color: if hl { theme.link } else { color },
            font_id: FontId::proportional(13.0),
            ..Default::default()
        };
        if hl {
            fmt.underline = egui::Stroke::new(1.0, theme.link);
        }
        job.append(&c.to_string(), 0.0, fmt);
    }
    if !row.detail.is_empty() {
        let detail: String = row.detail.chars().take(120).collect();
        job.append(
            &format!("   {detail}"),
            0.0,
            TextFormat {
                color: if selected { theme.text } else { theme.text_muted },
                font_id: FontId::proportional(12.0),
                ..Default::default()
            },
        );
    }
    job
}

/// A list with optional search field; returns a clicked index.
fn list(ui: &mut egui::Ui, st: &UiState, list: &[Row], selected: usize, height: f32) -> Option<usize> {
    let mut clicked = None;
    let row_h = 24.0;
    ScrollArea::vertical().max_height(height).auto_shrink([false, true]).show_rows(
        ui,
        row_h,
        list.len(),
        |ui, range| {
            for i in range {
                let Some(row) = list.get(i) else { continue };
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                if i == selected {
                    ui.painter().rect_filled(rect, 4, st.theme.list_selection);
                    if ui.input(|inp| inp.key_down(Key::ArrowDown) || inp.key_down(Key::ArrowUp)) {
                        ui.scroll_to_rect(rect, None);
                    }
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 4, st.theme.hover);
                }
                let galley = ui.fonts_mut(|f| f.layout_job(title_job(row, &st.theme, i == selected)));
                ui.painter().galley(
                    egui::pos2(rect.left() + 6.0, rect.center().y - galley.size().y / 2.0),
                    galley,
                    st.theme.text,
                );
                if resp.clicked() {
                    clicked = Some(i);
                }
            }
        },
    );
    if list.is_empty() {
        ui.label(RichText::new("Nothing found").color(st.theme.text_muted));
    }
    clicked
}

fn search_field(ui: &mut egui::Ui, st: &mut UiState, text: &mut String, hint: &str) -> bool {
    let resp = ui.add(
        TextEdit::singleline(text)
            .hint_text(hint)
            .desired_width(f32::INFINITY)
            .font(FontId::proportional(14.0))
            .frame(Frame::NONE),
    );
    if st.popup_focus_pending || !resp.has_focus() && ui.memory(|m| m.focused().is_none()) {
        resp.request_focus();
        st.popup_focus_pending = false;
    }
    ui.separator();
    resp.changed()
}

pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let Some(popup) = s.popup.clone() else {
        st.popup_focus_pending = true;
        return;
    };
    let ctx = ui.ctx().clone();
    let screen = ctx.content_rect();
    let top_center = || {
        egui::Area::new(egui::Id::new("codecraft-popup"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_TOP, vec2(0.0, 70.0))
    };
    let width = (screen.width() * 0.5).clamp(420.0, 760.0);
    let mut clicked: Option<usize> = None;
    let mut new_popup = popup.clone();
    let mut query_changed = false;
    match &mut new_popup {
        Popup::GotoFile { query, selected }
        | Popup::FindAction { query, selected }
        | Popup::RecentFiles { query, selected }
        | Popup::FileStructure { query, selected, .. }
        | Popup::GotoSymbol { query, selected, .. }
        | Popup::SearchEverywhere { query, selected, .. } => {
            let title = match &popup {
                Popup::GotoFile { .. } => "Files",
                Popup::FindAction { .. } => "Actions",
                Popup::RecentFiles { .. } => "Recent Files",
                Popup::FileStructure { .. } => "File Structure",
                Popup::GotoSymbol { types_only: true, .. } => "Types",
                Popup::GotoSymbol { .. } => "Symbols",
                _ => "Search Everywhere",
            };
            let pending = matches!(
                &popup,
                Popup::GotoSymbol { pending: true, .. }
                    | Popup::SearchEverywhere { pending: true, .. }
                    | Popup::FileStructure { pending: true, .. }
            );
            top_center().show(&ctx, |ui| {
                frame(st).show(ui, |ui| {
                    ui.set_width(width);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(title).strong());
                        if pending {
                            ui.add(egui::Spinner::new().size(12.0));
                        }
                    });
                    query_changed = search_field(ui, st, query, "Type to search");
                    if query_changed {
                        *selected = 0;
                    }
                });
            });
        }
        _ => {}
    }
    if new_popup != popup {
        s.popup = Some(new_popup.clone());
    }
    if query_changed && let Some(Popup::GotoSymbol { query, .. } | Popup::SearchEverywhere { query, .. }) = &s.popup {
        let q = query.clone();
        run_command(s, st, "navigate.symbolQuery", &json!({ "query": q }));
    }
    // List popups: draw the rows in a second area just under the search field.
    match &s.popup {
        Some(
            Popup::GotoFile { selected, .. }
            | Popup::FindAction { selected, .. }
            | Popup::RecentFiles { selected, .. }
            | Popup::FileStructure { selected, .. }
            | Popup::GotoSymbol { selected, .. }
            | Popup::SearchEverywhere { selected, .. },
        ) => {
            let sel = *selected;
            let rows = rows(s, st);
            egui::Area::new(egui::Id::new("codecraft-popup-list"))
                .order(egui::Order::Foreground)
                .anchor(Align2::CENTER_TOP, vec2(0.0, 132.0))
                .show(&ctx, |ui| {
                    frame(st).show(ui, |ui| {
                        ui.set_width(width);
                        clicked = list(ui, st, &rows, sel, 420.0);
                    });
                });
        }
        Some(Popup::RecentLocations { selected } | Popup::RunConfigs { selected }) => {
            let sel = *selected;
            let title = if matches!(s.popup, Some(Popup::RunConfigs { .. })) { "Run" } else { "Recent Locations" };
            let rows = rows(s, st);
            top_center().show(&ctx, |ui| {
                frame(st).show(ui, |ui| {
                    ui.set_width(width);
                    ui.label(RichText::new(title).strong());
                    ui.separator();
                    clicked = list(ui, st, &rows, sel, 420.0);
                });
            });
        }
        Some(Popup::Actions { selected, title, .. } | Popup::Locations { selected, title, .. }) => {
            let (sel, title) = (*selected, title.clone());
            let rows = rows(s, st);
            let anchor = st.caret_screen.unwrap_or(screen.center());
            egui::Area::new(egui::Id::new("codecraft-popup-caret"))
                .order(egui::Order::Foreground)
                .fixed_pos(anchor + vec2(0.0, 4.0))
                .show(&ctx, |ui| {
                    frame(st).show(ui, |ui| {
                        ui.set_width(460.0);
                        ui.label(RichText::new(title).strong());
                        ui.separator();
                        clicked = list(ui, st, &rows, sel, 300.0);
                    });
                });
        }
        Some(Popup::Completion(c)) => {
            let sel = c.selected;
            let pending = c.pending;
            let doc = c.visible.get(sel).and_then(|i| c.items.get(*i)).and_then(|it| it.documentation.clone());
            let rows = rows(s, st);
            let anchor = st.caret_screen.unwrap_or(screen.center());
            egui::Area::new(egui::Id::new("codecraft-completion"))
                .order(egui::Order::Foreground)
                .fixed_pos(anchor + vec2(-20.0, 2.0))
                .show(&ctx, |ui| {
                    ui.horizontal_top(|ui| {
                        frame(st).inner_margin(Margin::same(4)).show(ui, |ui| {
                            ui.set_width(440.0);
                            if pending && rows.is_empty() {
                                ui.horizontal(|ui| {
                                    ui.add(egui::Spinner::new().size(12.0));
                                    ui.label(RichText::new("Loading…").color(st.theme.text_muted));
                                });
                            } else {
                                clicked = list(ui, st, &rows, sel, 260.0);
                            }
                        });
                        if let Some(doc) = doc.filter(|d| !d.trim().is_empty()) {
                            frame(st).show(ui, |ui| {
                                ui.set_max_width(380.0);
                                ScrollArea::vertical()
                                    .max_height(260.0)
                                    .show(ui, |ui| markdown::render(ui, &doc, &st.theme, 12.0));
                            });
                        }
                    });
                });
        }
        Some(Popup::Hover { text, .. } | Popup::ParameterInfo { text, .. }) => {
            let text = text.clone();
            let anchor = st.caret_screen.unwrap_or(screen.center());
            let size = s.ws.settings.editor_font_size;
            let resp = egui::Area::new(egui::Id::new("codecraft-hover"))
                .order(egui::Order::Foreground)
                .fixed_pos(anchor + vec2(0.0, 4.0))
                .show(&ctx, |ui| {
                    frame(st).show(ui, |ui| {
                        ui.set_max_width(640.0);
                        ScrollArea::vertical()
                            .max_height(360.0)
                            .show(ui, |ui| markdown::render(ui, &text, &st.theme, size - 1.0));
                    });
                });
            if ctx.input(|i| i.pointer.any_pressed()) && !resp.response.contains_pointer() {
                s.popup = None;
            }
        }
        Some(Popup::Message { title, text }) => {
            let (title, text) = (title.clone(), text.clone());
            top_center().show(&ctx, |ui| {
                frame(st).show(ui, |ui| {
                    ui.set_max_width(width);
                    ui.label(RichText::new(title).strong());
                    ui.separator();
                    ui.label(text);
                    if ui.button("Close").clicked() {
                        s.popup = None;
                    }
                });
            });
        }
        Some(Popup::GotoLine { .. } | Popup::Rename { .. } | Popup::NewItem { .. }) => input_popup(&ctx, s, st, width),
        Some(Popup::FindInFiles { .. }) => find_in_files(&ctx, s, st, width),
        Some(Popup::Settings) => settings(&ctx, s, st),
        Some(Popup::About) => about(&ctx, s, st),
        _ => {}
    }
    if let Some(i) = clicked {
        if let Some(sel) = s.popup.as_mut().and_then(selected_mut) {
            *sel = i;
        }
        accept(s, st, Some(i));
    }
}

fn input_popup(ctx: &egui::Context, s: &mut Session, st: &mut UiState, width: f32) {
    let Some(mut popup) = s.popup.clone() else { return };
    let (title, text) = match &mut popup {
        Popup::GotoLine { input } => ("Go to Line:Column", input),
        Popup::Rename { name, .. } => ("Rename", name),
        Popup::NewItem { kind: NewItemKind::File, name, .. } => ("New File", name),
        Popup::NewItem { kind: NewItemKind::Directory, name, .. } => ("New Directory", name),
        Popup::NewItem { kind: NewItemKind::CargoCrate, name, .. } => ("New Cargo Crate", name),
        _ => return,
    };
    let mut accept_now = false;
    egui::Area::new(egui::Id::new("codecraft-input"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 120.0))
        .show(ctx, |ui| {
            frame(st).show(ui, |ui| {
                ui.set_width(width.min(480.0));
                ui.label(RichText::new(title).strong());
                let resp =
                    ui.add(TextEdit::singleline(text).desired_width(f32::INFINITY).font(FontId::monospace(14.0)));
                if st.popup_focus_pending {
                    resp.request_focus();
                    st.popup_focus_pending = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    accept_now = true;
                }
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        accept_now = true;
                    }
                    if ui.button("Cancel").clicked() {
                        s.popup = None;
                    }
                });
            });
        });
    if s.popup.is_some() {
        s.popup = Some(popup);
    }
    if accept_now {
        accept(s, st, None);
    }
}

fn find_in_files(ctx: &egui::Context, s: &mut Session, st: &mut UiState, width: f32) {
    let Some(Popup::FindInFiles { mut query, mut mask, mut replace }) = s.popup.clone() else { return };
    let mut go = false;
    egui::Area::new(egui::Id::new("codecraft-fif"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 80.0))
        .show(ctx, |ui| {
            frame(st).show(ui, |ui| {
                ui.set_width(width);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(if replace.is_some() { "Replace in Files" } else { "Find in Files" }).strong(),
                    );
                    for (flag, label) in
                        [(&mut query.case_sensitive, "Cc"), (&mut query.whole_word, "W"), (&mut query.regex, ".*")]
                    {
                        if ui.add(egui::Button::new(RichText::new(label).monospace()).selected(*flag)).clicked() {
                            *flag = !*flag;
                        }
                    }
                    ui.label("File mask:");
                    ui.add(TextEdit::singleline(&mut mask).hint_text("*.rs").desired_width(90.0));
                });
                let resp = ui.add(
                    TextEdit::singleline(&mut query.pattern)
                        .hint_text("Text to find")
                        .desired_width(f32::INFINITY)
                        .font(FontId::monospace(14.0)),
                );
                if st.popup_focus_pending {
                    resp.request_focus();
                    st.popup_focus_pending = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    go = true;
                }
                if let Some(r) = replace.as_mut() {
                    ui.add(
                        TextEdit::singleline(r)
                            .hint_text("Replace with")
                            .desired_width(f32::INFINITY)
                            .font(FontId::monospace(14.0)),
                    );
                }
                ui.horizontal(|ui| {
                    if ui.button(if replace.is_some() { "Replace All" } else { "Find" }).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        s.popup = None;
                    }
                });
            });
        });
    if s.popup.is_some() {
        s.popup = Some(Popup::FindInFiles { query, mask, replace });
    }
    if go {
        accept(s, st, None);
    }
}

fn settings(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let mut open = true;
    let mut set: Vec<(&'static str, Value)> = Vec::new();
    let mut cfg = s.ws.settings.clone();
    egui::Window::new("Settings").open(&mut open).collapsible(false).resizable(true).default_width(520.0).show(
        ctx,
        |ui| {
            ui.heading("Editor");
            egui::Grid::new("settings-grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Font size");
                if ui.add(egui::Slider::new(&mut cfg.editor_font_size, 8.0..=32.0).step_by(1.0)).changed() {
                    set.push(("editor_font_size", json!(cfg.editor_font_size)));
                }
                ui.end_row();
                ui.label("Line spacing");
                if ui.add(egui::Slider::new(&mut cfg.line_spacing, 1.0..=2.0).step_by(0.05)).changed() {
                    set.push(("line_spacing", json!(cfg.line_spacing)));
                }
                ui.end_row();
                ui.label("Tab size");
                if ui.add(egui::Slider::new(&mut cfg.tab_size, 1..=8)).changed() {
                    set.push(("tab_size", json!(cfg.tab_size)));
                }
                ui.end_row();
                ui.label("Show line numbers");
                if ui.checkbox(&mut cfg.show_line_numbers, "").changed() {
                    set.push(("show_line_numbers", json!(cfg.show_line_numbers)));
                }
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.heading("Rust");
            egui::Grid::new("settings-rust").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("External linter");
                let mut clippy = cfg.use_clippy;
                egui::ComboBox::from_id_salt("linter")
                    .selected_text(if clippy { "Clippy" } else { "Cargo Check" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut clippy, false, "Cargo Check");
                        ui.selectable_value(&mut clippy, true, "Clippy");
                    });
                if clippy != cfg.use_clippy {
                    set.push(("use_clippy", json!(clippy)));
                }
                ui.end_row();
                ui.label("rust-analyzer path");
                let mut path = cfg.rust_analyzer_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                let resp = ui.add(TextEdit::singleline(&mut path).hint_text("auto-detect"));
                if resp.lost_focus() {
                    let v = if path.trim().is_empty() { Value::Null } else { json!(path.trim()) };
                    set.push(("rust_analyzer_path", v));
                }
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Editor font: {}", st.mono_font_name)).color(st.theme.text_muted).size(11.0),
            );
        },
    );
    for (k, v) in set {
        run_command(s, st, "settings.set", &json!({ "key": k, "value": v }));
    }
    if !open {
        s.popup = None;
    }
}

fn about(ctx: &egui::Context, s: &mut Session, st: &UiState) {
    let mut open = true;
    egui::Window::new("About CodeCraft").open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
        ui.label(RichText::new("CodeCraft").size(22.0).strong());
        ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
        ui.add_space(6.0);
        ui.label("A native Rust IDE written in Rust: a clean-room reimplementation of RustRover's workflow.");
        ui.label(
            RichText::new("Language intelligence by rust-analyzer. Licensed MIT OR Apache-2.0.")
                .color(st.theme.text_muted),
        );
    });
    if !open {
        s.popup = None;
    }
}
