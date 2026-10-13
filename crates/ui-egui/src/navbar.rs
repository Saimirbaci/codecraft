//! Breadcrumbs under the editor, the navigation bar (Navigate › Jump to Navigation Bar), the
//! small chooser popups (Insert Live Template, Surround With, Unwrap/Remove) and the Save as Live
//! Template dialog. The engine owns their state (`Session::editing`); this module draws it and
//! maps keys and clicks to commands.

use std::sync::Arc;

use egui::{Align2, Color32, FontId, Frame, Key, Margin, Modifiers, Rect, RichText, Sense, TextEdit, pos2, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::editing::{ChooserKind, NavKind};
use codecraft_engine::structure::{self, Extent};
use codecraft_workspace::{DocId, EditorId};

use crate::{UiState, run_command};

/// Height of the breadcrumbs strip.
pub const BREADCRUMBS_H: f32 = 22.0;

/// Whether files of this editor's language get breadcrumbs.
pub fn has_breadcrumbs(lang: codecraft_syntax::Language) -> bool {
    use codecraft_syntax::Language::*;
    matches!(lang, Rust | C | Cpp | Python | Zig | TypeScript | JavaScript | Markdown | Toml)
}

type ExtentCache = Arc<(DocId, u64, Vec<Extent>)>;

/// Outline extents of a document, recomputed when its revision changes.
fn extents_for(ui: &egui::Ui, s: &Session, eid: EditorId) -> Vec<Extent> {
    let Some(d) = s.ws.editor(eid).and_then(|e| s.ws.doc(e.doc)) else { return Vec::new() };
    let key = egui::Id::new(("codecraft-extents", eid.0));
    let rev = d.doc.revision();
    if let Some(c) = ui.data(|m| m.get_temp::<ExtentCache>(key))
        && c.0 == d.id
        && c.1 == rev
    {
        return c.2.clone();
    }
    let ex = structure::extents(d.language, &d.doc.text());
    ui.data_mut(|m| m.insert_temp(key, Arc::new((d.id, rev, ex.clone()))));
    ex
}

fn kind_icon(kind: codecraft_syntax::SymbolKind) -> &'static str {
    use codecraft_syntax::SymbolKind::*;
    match kind {
        Function => "ƒ",
        Struct | Enum | TypeAlias => "◇",
        Trait => "◈",
        Impl => "▣",
        Module => "▤",
        Const | Static => "◆",
        Macro => "!",
        Section => "§",
    }
}

/// The breadcrumbs strip: the items enclosing the caret, clickable.
pub fn breadcrumbs(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, eid: EditorId, rect: Rect) {
    let theme = st.theme.clone();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.editor_bg);
    painter.hline(rect.x_range(), rect.top(), egui::Stroke::new(1.0, theme.separator));
    let Some(caret) = s.ws.editor(eid).map(|e| e.selections.primary().head) else { return };
    let crumbs = structure::crumbs_at(&extents_for(ui, s, eid), caret);
    let font = FontId::proportional(12.0);
    let mut x = rect.left() + 10.0;
    let mut clicked = None;
    for (i, c) in crumbs.iter().enumerate() {
        if i > 0 {
            let g = ui.fonts_mut(|f| f.layout_no_wrap(" › ".into(), font.clone(), theme.text_disabled));
            painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g.clone(), theme.text_disabled);
            x += g.size().x;
        }
        let label = format!("{} {}", kind_icon(c.kind), c.label());
        let g = ui.fonts_mut(|f| f.layout_no_wrap(label, font.clone(), theme.text_muted));
        let r = Rect::from_min_size(pos2(x - 2.0, rect.top() + 2.0), vec2(g.size().x + 4.0, rect.height() - 4.0));
        let resp = ui.interact(r, egui::Id::new(("codecraft-crumb", eid.0, i)), Sense::click());
        if resp.hovered() {
            painter.rect_filled(r, 3.0, theme.hover);
        }
        painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g.clone(), theme.text_muted);
        if resp.clicked() {
            clicked = Some(c.offset);
        }
        x += g.size().x;
        if x > rect.right() {
            break;
        }
    }
    if let Some(offset) = clicked
        && s.ws.active_editor() == Some(eid)
    {
        run_command(s, st, "editor.setSelection", &json!({ "anchor": offset }));
        st.focus_editor = true;
    }
}

/// The navigation bar over the top of the editor while it is active, with the popup listing the
/// selected element's contents.
pub fn overlay(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, eid: EditorId, rect: Rect) {
    let Some(bar) = s.editing.navbar.clone().filter(|b| b.editor == eid) else { return };
    let theme = st.theme.clone();
    let strip = Rect::from_min_size(rect.min, vec2(rect.width(), 26.0));
    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("codecraft-navbar")));
    painter.rect_filled(strip, 0.0, theme.header_bg);
    painter.hline(strip.x_range(), strip.bottom(), egui::Stroke::new(1.0, theme.separator));
    let font = FontId::proportional(13.0);
    let mut x = strip.left() + 8.0;
    let mut select = None;
    let mut popup_x = x;
    for (i, seg) in bar.segments.iter().enumerate() {
        if i > 0 {
            let g = ui.fonts_mut(|f| f.layout_no_wrap(" › ".into(), font.clone(), theme.text_disabled));
            painter.galley(pos2(x, strip.center().y - g.size().y / 2.0), g.clone(), theme.text_disabled);
            x += g.size().x;
        }
        let icon = match seg.kind {
            NavKind::Directory => "▸",
            NavKind::File => "◆",
            NavKind::Symbol => "ƒ",
        };
        let g = ui.fonts_mut(|f| f.layout_no_wrap(format!("{icon} {}", seg.label), font.clone(), theme.text));
        let r = Rect::from_min_size(pos2(x - 3.0, strip.top() + 3.0), vec2(g.size().x + 6.0, strip.height() - 6.0));
        if i == bar.selected {
            painter.rect_filled(r, 4.0, theme.list_selection);
            popup_x = r.left();
        }
        let resp = ui.interact(r, egui::Id::new(("codecraft-navseg", i)), Sense::click());
        if resp.clicked() {
            select = Some(i);
        }
        painter.galley(pos2(x, strip.center().y - g.size().y / 2.0), g.clone(), theme.text);
        x += g.size().x + 2.0;
    }
    let mut accept = None;
    if !bar.items.is_empty() {
        egui::Area::new(egui::Id::new("codecraft-navbar-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos2(popup_x, strip.bottom() + 2.0))
            .show(ui.ctx(), |ui| {
                popup_frame(st).show(ui, |ui| {
                    ui.set_min_width(220.0);
                    egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                        for (i, item) in bar.items.iter().enumerate() {
                            let icon = match item.kind {
                                NavKind::Directory => "▸",
                                NavKind::File => "◆",
                                NavKind::Symbol => "ƒ",
                            };
                            let selected = i == bar.item_selected;
                            let text = RichText::new(format!("{icon}  {}", item.label))
                                .color(if selected { theme.text } else { theme.text_muted });
                            let resp = ui.add(egui::Button::selectable(selected, text).frame_when_inactive(false));
                            if selected {
                                resp.scroll_to_me(None);
                            }
                            if resp.clicked() {
                                accept = Some(i);
                            }
                        }
                    });
                });
            });
    }
    if let Some(i) = select {
        run_command(s, st, "navbar.select", &json!({ "index": i }));
    }
    if let Some(i) = accept {
        run_command(s, st, "navbar.accept", &json!({ "index": i }));
        if s.editing.navbar.is_none() {
            st.focus_editor = true;
        }
    }
}

fn popup_frame(st: &UiState) -> Frame {
    Frame::new()
        .fill(st.theme.popup_bg)
        .stroke(egui::Stroke::new(1.0, st.theme.separator))
        .corner_radius(8)
        .inner_margin(Margin::same(6))
        .shadow(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(120) })
}

/// The chooser, the navigation bar and the save dialog take the keyboard while shown.
pub fn wants_keyboard(s: &Session) -> bool {
    s.editing.chooser.is_some() || s.editing.navbar.is_some() || s.editing.save_dialog.is_some()
}

/// Keys for the chooser, navigation bar and save dialog (consumed before text fields see them).
pub fn handle_keys(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
    if s.editing.save_dialog.is_some() {
        if pressed(Key::Escape) {
            s.editing.save_dialog = None;
            st.focus_editor = true;
        }
        return;
    }
    if s.editing.chooser.is_some() {
        if pressed(Key::Escape) {
            run_command(s, st, "editor.chooser.close", &Value::Null);
            st.focus_editor = true;
        } else if pressed(Key::ArrowDown) {
            run_command(s, st, "editor.chooser.move", &json!({ "delta": 1 }));
        } else if pressed(Key::ArrowUp) {
            run_command(s, st, "editor.chooser.move", &json!({ "delta": -1 }));
        } else if pressed(Key::Enter) || pressed(Key::Tab) {
            run_command(s, st, "editor.chooser.accept", &Value::Null);
            st.focus_editor = true;
        }
        return;
    }
    if s.editing.navbar.is_some() {
        if pressed(Key::Escape) {
            run_command(s, st, "navbar.close", &Value::Null);
            st.focus_editor = true;
        } else if pressed(Key::ArrowLeft) {
            run_command(s, st, "navbar.select", &json!({ "delta": -1 }));
        } else if pressed(Key::ArrowRight) {
            run_command(s, st, "navbar.select", &json!({ "delta": 1 }));
        } else if pressed(Key::ArrowDown) {
            run_command(s, st, "navbar.moveItem", &json!({ "delta": 1 }));
        } else if pressed(Key::ArrowUp) {
            run_command(s, st, "navbar.moveItem", &json!({ "delta": -1 }));
        } else if pressed(Key::Enter) {
            run_command(s, st, "navbar.accept", &Value::Null);
            if s.editing.navbar.is_none() {
                st.focus_editor = true;
            }
        }
    }
}

/// The chooser popup and the Save as Live Template dialog.
pub fn show_popups(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    if let Some(c) = s.editing.chooser.clone() {
        let anchor = st.caret_screen.unwrap_or(ctx.content_rect().center());
        let visible = c.visible();
        let mut query = c.query.clone();
        let mut accept = None;
        egui::Area::new(egui::Id::new("codecraft-chooser"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor + vec2(0.0, 4.0))
            .show(&ctx, |ui| {
                popup_frame(st).show(ui, |ui| {
                    ui.set_width(340.0);
                    ui.label(RichText::new(&c.title).strong());
                    if c.kind != ChooserKind::UnwrapRemove {
                        let resp = ui.add(
                            TextEdit::singleline(&mut query)
                                .hint_text("Type to filter")
                                .desired_width(f32::INFINITY)
                                .frame(Frame::NONE),
                        );
                        if !resp.has_focus() {
                            resp.request_focus();
                        }
                    }
                    ui.separator();
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        for (row, i) in visible.iter().enumerate() {
                            let Some(item) = c.items.get(*i) else { continue };
                            let selected = row == c.selected;
                            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                            if selected {
                                ui.painter().rect_filled(rect, 4.0, st.theme.list_selection);
                                resp.scroll_to_me(None);
                            } else if resp.hovered() {
                                ui.painter().rect_filled(rect, 4.0, st.theme.hover);
                            }
                            ui.painter().text(
                                pos2(rect.left() + 6.0, rect.center().y),
                                Align2::LEFT_CENTER,
                                &item.label,
                                FontId::proportional(13.0),
                                st.theme.text,
                            );
                            if !item.detail.is_empty() {
                                ui.painter().text(
                                    pos2(rect.left() + 120.0, rect.center().y),
                                    Align2::LEFT_CENTER,
                                    &item.detail,
                                    FontId::proportional(12.0),
                                    st.theme.text_muted,
                                );
                            }
                            if resp.clicked() {
                                accept = Some(row);
                            }
                        }
                        if visible.is_empty() {
                            ui.label(RichText::new("Nothing found").color(st.theme.text_muted));
                        }
                    });
                });
            });
        if query != c.query {
            run_command(s, st, "editor.chooser.setQuery", &json!({ "query": query }));
        }
        if let Some(i) = accept {
            run_command(s, st, "editor.chooser.accept", &json!({ "index": i }));
            st.focus_editor = true;
        }
    }
    if let Some(mut d) = s.editing.save_dialog.clone() {
        let mut save = false;
        let mut cancel = false;
        egui::Window::new("Save as Live Template")
            .id(egui::Id::new("codecraft-save-template"))
            .collapsible(false)
            .resizable(true)
            .default_width(460.0)
            .show(&ctx, |ui| {
                egui::Grid::new("codecraft-save-template-grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("Abbreviation:");
                    ui.add(TextEdit::singleline(&mut d.abbreviation).desired_width(200.0));
                    ui.end_row();
                    ui.label("Description:");
                    ui.add(TextEdit::singleline(&mut d.description).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label("Language:");
                    ui.add(TextEdit::singleline(&mut d.language).desired_width(120.0));
                    ui.end_row();
                });
                ui.label(
                    RichText::new("Template text: $NAME$ variables, $END$ caret, $SELECTION$ selection, $$ for $")
                        .color(st.theme.text_muted),
                );
                ui.add(
                    TextEdit::multiline(&mut d.text)
                        .code_editor()
                        .desired_rows(8)
                        .desired_width(f32::INFINITY),
                );
                if let Some(e) = &d.error {
                    ui.label(RichText::new(e).color(st.theme.error));
                }
                ui.horizontal(|ui| {
                    save = ui.button("Save").clicked();
                    cancel = ui.button("Cancel").clicked();
                });
            });
        if cancel {
            s.editing.save_dialog = None;
            st.focus_editor = true;
        } else {
            let params = json!({
                "abbreviation": d.abbreviation,
                "description": d.description,
                "text": d.text,
                "language": d.language,
            });
            s.editing.save_dialog = Some(d);
            if save {
                run_command(s, st, "code.saveAsLiveTemplate", &params);
                if s.editing.save_dialog.is_none() {
                    st.focus_editor = true;
                }
            }
        }
    }
}
