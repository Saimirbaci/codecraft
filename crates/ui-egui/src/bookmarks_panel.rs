//! Bookmarks and TODO tool windows, the bookmark dialogs (mnemonic chooser, description, the
//! Show Bookmarks popup) and the scratch file popups (language chooser, Scratches list).

use std::path::Path;

use egui::{Align2, CornerRadius, Id, Key, RichText, ScrollArea, Sense, vec2};
use serde_json::{Value, json};

use codecraft_engine::{Session, bookmarks, scratch};

use crate::theme::Theme;
use crate::{UiState, run_command};

fn rel(s: &Session, path: &Path) -> String {
    match &s.ws.project {
        Some(p) => p.relative(path).into_owned(),
        None => path.display().to_string(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string()
}

/// The small mnemonic badge (also drawn in the editor gutter).
pub(crate) fn paint_badge(painter: &egui::Painter, theme: &Theme, center: egui::Pos2, h: f32, mnemonic: Option<char>) {
    let w = (h * 0.8).clamp(8.0, 13.0);
    let rect = egui::Rect::from_center_size(center, vec2(w, (h * 0.85).clamp(9.0, 15.0)));
    match mnemonic {
        Some(m) => {
            painter.rect_filled(rect, CornerRadius::same(2), theme.text_muted);
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                m.to_string(),
                egui::FontId::monospace((rect.height() - 2.0).max(7.0)),
                theme.editor_bg,
            );
        }
        None => {
            // A ribbon: a rectangle with a notch at the bottom.
            let (l, r, t, b) = (rect.left() + 1.0, rect.right() - 1.0, rect.top(), rect.bottom());
            let pts = vec![
                egui::pos2(l, t),
                egui::pos2(r, t),
                egui::pos2(r, b),
                egui::pos2((l + r) * 0.5, b - rect.height() * 0.3),
                egui::pos2(l, b),
            ];
            painter.add(egui::Shape::convex_polygon(pts, theme.text_muted, egui::Stroke::NONE));
        }
    }
}

// ---------------------------------------------------------------- Bookmarks tool window

pub fn show_bookmarks(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} bookmark(s)", s.bookmarks.items.len())).color(theme.text_muted).small());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(!s.bookmarks.items.is_empty(), egui::Button::new(RichText::new("Delete All").small()))
                .clicked()
            {
                cmds.push(("edit.bookmarks.removeAll", Value::Null));
            }
        });
    });
    if let Some(e) = &s.bookmarks.error {
        ui.label(RichText::new(e).color(theme.error).small());
    }
    if s.bookmarks.items.is_empty() {
        ui.label(
            RichText::new("No bookmarks. Toggle one with F3, or with a mnemonic with Alt+F3.").color(theme.text_muted),
        );
    }
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let order = bookmarks::sorted(&s.bookmarks.items);
        let mut last_path: Option<&Path> = None;
        for i in order {
            let Some(b) = s.bookmarks.items.get(i) else { continue };
            if last_path != Some(b.path.as_path()) {
                last_path = Some(b.path.as_path());
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(file_name(&b.path)).strong());
                    ui.label(RichText::new(rel(s, &b.path)).color(theme.text_muted).small());
                });
            }
            let resp = ui
                .horizontal(|ui| {
                    ui.add_space(12.0);
                    let (r, _) = ui.allocate_exact_size(vec2(14.0, 16.0), Sense::hover());
                    paint_badge(ui.painter(), &theme, r.center(), 14.0, b.mnemonic);
                    let mut text = format!("{}: {}", b.line + 1, b.preview);
                    if !b.description.is_empty() {
                        text = format!("{} — {text}", b.description);
                    }
                    ui.add(egui::Label::new(RichText::new(text).monospace()).truncate().sense(Sense::click()))
                })
                .inner;
            if resp.clicked() {
                cmds.push(("edit.bookmarks.open", json!({ "index": i })));
            }
            resp.context_menu(|ui| {
                if ui.button("Go to Bookmark").clicked() {
                    cmds.push(("edit.bookmarks.open", json!({ "index": i })));
                    ui.close();
                }
                if ui.button("Edit Description…").clicked() {
                    cmds.push(("edit.bookmarks.editDescription", json!({ "index": i })));
                    ui.close();
                }
                if ui.button("Delete").clicked() {
                    cmds.push(("edit.bookmarks.remove", json!({ "index": i })));
                    ui.close();
                }
            });
        }
    });
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

// ---------------------------------------------------------------- TODO tool window

pub fn show_todo(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    ui.horizontal(|ui| {
        if ui.add_enabled(s.ws.project.is_some(), egui::Button::new("⟳").small()).on_hover_text("Refresh").clicked() {
            cmds.push(("todo.refresh", Value::Null));
        }
        let files = s.todo.files.len();
        let mut text = format!("Found {} TODO item(s) in {files} file(s)", s.todo.count());
        if s.todo.truncated {
            text.push_str(" (stopped at the limit)");
        }
        ui.label(RichText::new(text).color(theme.text_muted).small());
        if s.todo.busy() {
            ui.spinner();
        }
    });
    if s.ws.project.is_none() {
        ui.label(RichText::new("Open a project to see its TODO items.").color(theme.text_muted));
    } else if s.todo.files.is_empty() && !s.todo.busy() {
        ui.label(RichText::new("No TODO items found.").color(theme.text_muted));
    }
    ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (path, items) in &s.todo.files {
            let header = RichText::new(format!("{}  ({})", file_name(path), items.len())).strong();
            egui::CollapsingHeader::new(header)
                .id_salt(("todo-file", path))
                .default_open(true)
                .show(ui, |ui| {
                    ui.label(RichText::new(rel(s, path)).color(theme.text_muted).small());
                    for it in items {
                        let text = RichText::new(format!("({}, {}) {}", it.line + 1, it.column + 1, it.text))
                            .color(theme.link)
                            .monospace();
                        let resp = ui.add(egui::Label::new(text).truncate().sense(Sense::click()));
                        if resp.clicked() {
                            cmds.push(("todo.open", json!({ "path": path, "line": it.line, "column": it.column })));
                        }
                    }
                });
        }
    });
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

// ---------------------------------------------------------------- dialogs and popups

/// Whether one of these dialogs is open (it reads the keyboard itself; nothing goes to the editor).
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.bookmarks.mnemonic_dialog.is_some()
        || s.bookmarks.description_dialog.is_some()
        || s.bookmarks.popup
        || s.scratch.chooser.is_some()
        || s.scratch.listing
}

pub fn windows(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    mnemonic_dialog(&ctx, s, st);
    description_dialog(&ctx, s, st);
    bookmarks_popup(&ctx, s, st);
    scratch_chooser(&ctx, s, st);
    scratches_list(&ctx, s, st);
}

/// Focus `id` the first frame a dialog shows (so keys stop going to the editor).
fn focus_once(ctx: &egui::Context, key: Id, target: Id) {
    let first = ctx.data(|d| d.get_temp::<bool>(key).is_none());
    if first {
        ctx.data_mut(|d| d.insert_temp(key, true));
        ctx.memory_mut(|m| m.request_focus(target));
    }
}

fn forget_focus(ctx: &egui::Context, key: Id) {
    ctx.data_mut(|d| d.remove::<bool>(key));
}

const MNEMONICS: &str = "1234567890ABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn mnemonic_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let key = Id::new("bookmark-mnemonic-focus");
    let Some(dialog) = s.bookmarks.mnemonic_dialog.clone() else {
        forget_focus(ctx, key);
        return;
    };
    let theme = st.theme.clone();
    let taken: Vec<char> = s.bookmarks.items.iter().filter_map(|b| b.mnemonic).collect();
    let mut chosen: Option<char> = None;
    let mut close = false;
    let mut description = ctx.data(|d| d.get_temp::<String>(key.with("desc"))).unwrap_or(dialog.description.clone());
    let mut open = true;
    egui::Window::new("Add Mnemonic Bookmark")
        .id(Id::new("bookmark-mnemonic-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .show(ctx, |ui| {
            let grid_id = Id::new("bookmark-mnemonic-grid");
            let grid = ui.interact(ui.max_rect(), grid_id, Sense::focusable_noninteractive());
            focus_once(ctx, key, grid_id);
            ui.label(RichText::new("Press a key or click a mnemonic:").color(theme.text_muted));
            for row in [&MNEMONICS[..10], &MNEMONICS[10..23], &MNEMONICS[23..]] {
                ui.horizontal(|ui| {
                    for c in row.chars() {
                        let current = dialog.current == Some(c);
                        let used = taken.contains(&c) && !current;
                        let mut b = egui::Button::new(RichText::new(c.to_string()).monospace()).min_size(vec2(22.0, 22.0));
                        if current {
                            b = b.fill(theme.accent);
                        } else if used {
                            b = b.fill(theme.list_selection_inactive);
                        }
                        let resp = ui.add(b);
                        let resp = if used { resp.on_hover_text("In use: choosing it moves the mnemonic here") } else { resp };
                        if resp.clicked() {
                            chosen = Some(c);
                        }
                    }
                });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label("Description:");
                ui.text_edit_singleline(&mut description);
            });
            if grid.has_focus() {
                let typed = ui.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Text(t) => t.chars().next().and_then(bookmarks::normalize_mnemonic),
                        _ => None,
                    })
                });
                if typed.is_some() {
                    chosen = typed;
                }
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
            ui.horizontal(|ui| {
                if dialog.current.is_some() && ui.button("Remove Mnemonic").clicked() {
                    chosen = dialog.current;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    ctx.data_mut(|d| d.insert_temp(key.with("desc"), description.clone()));
    if let Some(c) = chosen {
        let p = json!({ "mnemonic": c.to_string(), "path": dialog.path, "line": dialog.line });
        run_command(s, st, "edit.bookmarks.toggleWithMnemonic", &p);
        if let Some(i) = s.bookmarks.find(&dialog.path, dialog.line)
            && description.trim() != dialog.description
        {
            run_command(s, st, "edit.bookmarks.editDescription", &json!({ "index": i, "description": description }));
        }
        close = true;
    }
    if close || !open {
        ctx.data_mut(|d| d.remove::<String>(key.with("desc")));
        forget_focus(ctx, key);
        run_command(s, st, "edit.bookmarks.closeMnemonicDialog", &Value::Null);
        st.focus_editor = true;
    }
}

fn description_dialog(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let key = Id::new("bookmark-description-focus");
    let Some(mut dialog) = s.bookmarks.description_dialog.clone() else {
        forget_focus(ctx, key);
        return;
    };
    let mut submit = false;
    let mut close = false;
    let mut open = true;
    egui::Window::new("Edit Bookmark Description")
        .id(Id::new("bookmark-description-dialog"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_CENTER, [0.0, -60.0])
        .show(ctx, |ui| {
            let edit = ui.add(egui::TextEdit::singleline(&mut dialog.description).desired_width(320.0));
            focus_once(ctx, key, edit.id);
            if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                submit = true;
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                close = true;
            }
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    submit = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    if submit {
        let p = json!({ "index": dialog.index, "description": dialog.description });
        run_command(s, st, "edit.bookmarks.editDescription", &p);
        close = true;
    } else {
        s.bookmarks.description_dialog = Some(dialog);
    }
    if close || !open {
        forget_focus(ctx, key);
        run_command(s, st, "edit.bookmarks.closeDescriptionDialog", &Value::Null);
    }
}

/// Up/Down/Enter/Escape over `len` rows; the selection lives in egui memory under `key`.
fn list_keys(ctx: &egui::Context, key: Id, len: usize) -> (usize, bool, bool) {
    let mut sel = ctx.data(|d| d.get_temp::<usize>(key)).unwrap_or(0);
    let (up, down, enter, esc) = ctx.input(|i| {
        (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter), i.key_pressed(Key::Escape))
    });
    if down {
        sel = sel.saturating_add(1);
    }
    if up {
        sel = sel.saturating_sub(1);
    }
    sel = sel.min(len.saturating_sub(1));
    ctx.data_mut(|d| d.insert_temp(key, sel));
    (sel, enter, esc)
}

fn row(ui: &mut egui::Ui, selected: bool, text: RichText) -> egui::Response {
    let resp = ui.add(egui::Button::selectable(selected, text).frame_when_inactive(false));
    if selected {
        resp.scroll_to_me(None);
    }
    resp
}

fn bookmarks_popup(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let key = Id::new("bookmarks-popup-sel");
    if !s.bookmarks.popup {
        ctx.data_mut(|d| d.remove::<usize>(key));
        return;
    }
    let theme = st.theme.clone();
    let order = bookmarks::sorted(&s.bookmarks.items);
    let (sel, enter, esc) = list_keys(ctx, key, order.len());
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut open = true;
    egui::Window::new("Bookmarks")
        .id(Id::new("bookmarks-popup"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([520.0, 320.0])
        .anchor(Align2::CENTER_TOP, [0.0, 80.0])
        .show(ctx, |ui| {
            if order.is_empty() {
                ui.label(RichText::new("No bookmarks. Toggle one with F3.").color(theme.text_muted));
            }
            ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                for (row_i, i) in order.iter().enumerate() {
                    let Some(b) = s.bookmarks.items.get(*i) else { continue };
                    let m = b.mnemonic.map(|m| format!("[{m}] ")).unwrap_or_default();
                    let desc = if b.description.is_empty() { String::new() } else { format!("{} — ", b.description) };
                    let text = format!("{m}{desc}{}:{}  {}", file_name(&b.path), b.line + 1, b.preview);
                    let resp = row(ui, row_i == sel, RichText::new(text).monospace());
                    if resp.clicked() || (enter && row_i == sel) {
                        cmds.push(("edit.bookmarks.open", json!({ "index": i })));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Delete").clicked() {
                            cmds.push(("edit.bookmarks.remove", json!({ "index": i })));
                            ui.close();
                        }
                    });
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("Open Bookmarks Tool Window").clicked() {
                    cmds.push(("edit.bookmarks.showToolWindow", Value::Null));
                }
                if ui.input(|i| i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace))
                    && let Some(i) = order.get(sel)
                {
                    cmds.push(("edit.bookmarks.remove", json!({ "index": i })));
                }
            });
        });
    if esc || !open {
        cmds.push(("edit.bookmarks.closePopup", Value::Null));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn scratch_chooser(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let key = Id::new("scratch-chooser-focus");
    let Some(mut chooser) = s.scratch.chooser.clone() else {
        forget_focus(ctx, key);
        return;
    };
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut open = true;
    let visible = chooser.visible();
    let (up, down, enter, esc) = ctx.input(|i| {
        (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter), i.key_pressed(Key::Escape))
    });
    if down {
        chooser.selected = chooser.selected.saturating_add(1);
    }
    if up {
        chooser.selected = chooser.selected.saturating_sub(1);
    }
    chooser.selected = chooser.selected.min(visible.len().saturating_sub(1));
    egui::Window::new("New Scratch File")
        .id(Id::new("scratch-chooser"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 80.0])
        .show(ctx, |ui| {
            let edit = ui.add(egui::TextEdit::singleline(&mut chooser.query).hint_text("Language").desired_width(260.0));
            focus_once(ctx, key, edit.id);
            if edit.changed() {
                chooser.selected = 0;
            }
            ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for (row_i, i) in chooser.visible().iter().enumerate() {
                    let Some((name, ext, ..)) = scratch::LANGUAGES.get(*i) else { continue };
                    let text = RichText::new(format!("{name}  (.{ext})"));
                    let resp = row(ui, row_i == chooser.selected, text);
                    if resp.clicked() || (enter && row_i == chooser.selected) {
                        cmds.push(("file.new.scratchFile", json!({ "language": name })));
                    }
                }
            });
        });
    if esc || !open {
        cmds.push(("scratch.closeChooser", Value::Null));
        forget_focus(ctx, key);
    }
    if s.scratch.chooser.is_some() {
        s.scratch.chooser = Some(chooser);
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn scratches_list(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let key = Id::new("scratches-list-sel");
    if !s.scratch.listing {
        ctx.data_mut(|d| d.remove::<usize>(key));
        return;
    }
    let theme = st.theme.clone();
    // The directory listing is cheap but not free: refresh it twice a second.
    let cache_key = Id::new("scratches-list-cache");
    let now = ctx.input(|i| i.time);
    let cached = ctx.data(|d| d.get_temp::<(f64, Vec<std::path::PathBuf>)>(cache_key));
    let files = match cached {
        Some((t, files)) if now - t < 0.5 => files,
        _ => {
            let files: Vec<std::path::PathBuf> =
                s.scratch.dir().as_deref().map(scratch::list).unwrap_or_default().into_iter().map(|(p, _)| p).collect();
            ctx.data_mut(|d| d.insert_temp(cache_key, (now, files.clone())));
            files
        }
    };
    let (sel, enter, esc) = list_keys(ctx, key, files.len());
    let mut cmds: Vec<(&'static str, Value)> = Vec::new();
    let mut open = true;
    egui::Window::new("Scratches")
        .id(Id::new("scratches-list"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_size([420.0, 300.0])
        .anchor(Align2::CENTER_TOP, [0.0, 80.0])
        .show(ctx, |ui| {
            if files.is_empty() {
                ui.label(RichText::new("No scratch files. Create one with Shift+Cmd+N.").color(theme.text_muted));
            }
            ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).show(ui, |ui| {
                for (i, p) in files.iter().enumerate() {
                    let resp = row(ui, i == sel, RichText::new(file_name(p)).monospace());
                    if resp.clicked() || (enter && i == sel) {
                        cmds.push(("scratch.open", json!({ "path": p })));
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Delete").clicked() {
                            cmds.push(("scratch.delete", json!({ "path": p })));
                            ctx.data_mut(|d| d.remove::<(f64, Vec<std::path::PathBuf>)>(cache_key));
                            ui.close();
                        }
                    });
                }
            });
            if ui.small_button("New Scratch File…").clicked() {
                cmds.push(("file.new.scratchFile", Value::Null));
            }
        });
    if esc || !open {
        cmds.push(("scratch.closeList", Value::Null));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}
