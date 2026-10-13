//! A virtualized results grid: only visible rows are laid out (`ScrollArea::show_rows`), the
//! header stays put while the body scrolls, columns resize by dragging the header edges, NULL is
//! greyed, and a selected cell or row copies with Cmd+C. The status line shows timing.
//!
//! Editable table results show pending changes (edited cells, new and deleted rows) and edit a
//! cell in place on double-click, Enter or F2; the caller turns the edits into `db.*` commands.

use egui::{Align2, CornerRadius, CursorIcon, Event, FontId, Id, Key, Rect, ScrollArea, Sense, Stroke, pos2, vec2};

use codecraft_engine::database::{CellView, GridEdits, RowState};
use codecraft_engine::db::{Cell, Column};

use crate::theme::Theme;

const ROW_H: f32 = 22.0;
const HEADER_H: f32 = 24.0;
const MIN_COL_W: f32 = 36.0;
const MAX_INIT_COL_W: f32 = 360.0;
/// Characters of a value shown in a cell (the rest is elided).
const MAX_CELL_CHARS: usize = 300;

/// What is selected in a grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Cell { row: usize, col: usize },
    Row(usize),
}

/// Per-grid view state (kept by the shell, keyed by result id).
#[derive(Clone, Debug, Default)]
pub struct GridState {
    pub widths: Vec<f32>,
    pub selection: Option<Selection>,
    /// Column names the widths were computed for (reset when the columns change).
    columns_key: Vec<String>,
    /// Scroll the selection into view next frame.
    reveal: bool,
    /// The cell being edited in place and its text; `focus` until the text field took focus.
    editing: Option<CellEdit>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CellEdit {
    row: usize,
    col: usize,
    text: String,
    focus: bool,
}

/// What the user asked the grid for; the caller turns these into commands.
#[derive(Clone, Debug, PartialEq)]
pub enum GridAction {
    /// Header clicked: sort by this column.
    Sort(usize),
    /// The last rows are visible (fetch the next page if there is one).
    NearEnd,
    /// Text for the clipboard.
    Copy(String),
    /// Set a cell (`None`: NULL).
    Edit { row: usize, col: usize, value: Option<String> },
    AddRow,
    DeleteRow(usize),
    RevertRow(usize),
}

/// Input for one frame of the grid.
pub struct GridData<'a> {
    pub columns: &'a [Column],
    pub rows: &'a [Vec<Cell>],
    /// `(column, descending)` shown as ▲/▼ in the header.
    pub sort: Option<(usize, bool)>,
    pub status: &'a str,
    pub status_is_error: bool,
    /// Pending changes shown over `rows` (new rows follow the fetched ones).
    pub edits: Option<&'a GridEdits>,
    /// Cells can be edited in place.
    pub editable: bool,
}

/// The display text of a cell: one line, elided.
pub fn cell_text(c: &Cell) -> String {
    if c.is_null() {
        return "<null>".into();
    }
    let s = c.display();
    let mut out: String =
        s.chars().take(MAX_CELL_CHARS).map(|ch| if ch == '\n' || ch == '\r' { '↵' } else { ch }).collect();
    if s.chars().count() > MAX_CELL_CHARS {
        out.push('…');
    }
    out
}

/// Tab-separated values of a row (NULL as an empty field), for Copy.
pub fn row_text(row: &[Cell]) -> String {
    row.iter().map(Cell::to_plain).collect::<Vec<_>>().join("\t")
}

fn initial_widths(columns: &[Column], rows: &[Vec<Cell>], char_w: f32) -> Vec<f32> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let sample = rows
                .iter()
                .take(50)
                .filter_map(|r| r.get(i))
                .map(|v| cell_text(v).chars().count().min(60))
                .max()
                .unwrap_or(4);
            let chars = sample.max(c.name.chars().count() + 2) as f32;
            (chars * char_w + 18.0).clamp(MIN_COL_W + 24.0, MAX_INIT_COL_W)
        })
        .collect()
}

/// Draw the grid and its status line; returns what the user asked for.
pub fn show(ui: &mut egui::Ui, id: Id, state: &mut GridState, data: &GridData<'_>, theme: &Theme) -> Vec<GridAction> {
    let mut actions = Vec::new();
    let font = FontId::monospace(12.5);
    let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0')).max(4.0);
    let key: Vec<String> = data.columns.iter().map(|c| c.name.clone()).collect();
    if state.columns_key != key || state.widths.len() != data.columns.len() {
        state.widths = initial_widths(data.columns, data.rows, char_w);
        state.columns_key = key;
        state.selection = None;
    }
    let fetched = data.rows.len();
    let n_rows = data.edits.map_or(fetched, |e| e.row_count(fetched));
    let digits = n_rows.max(1).to_string().len().max(2);
    let num_w = digits as f32 * char_w + 16.0;
    let total_w = num_w + state.widths.iter().sum::<f32>();
    let n_cols = data.columns.len();
    if !data.editable {
        state.editing = None;
    }
    let view = |r: usize, c: usize| -> CellView<'_> {
        match data.edits {
            Some(e) => e.cell(data.rows, r, c),
            None => data.rows.get(r).and_then(|row| row.get(c)).map(CellView::Value).unwrap_or(CellView::Missing),
        }
    };
    let start_edit = |r: usize, c: usize| -> CellEdit {
        let text = match view(r, c) {
            CellView::Value(v) | CellView::Changed(v) => v.to_plain(),
            CellView::Default | CellView::Missing => String::new(),
        };
        CellEdit { row: r, col: c, text, focus: true }
    };

    // Status line at the bottom.
    let status_h = 20.0;
    let full = ui.available_rect_before_wrap();
    let grid_rect = Rect::from_min_max(full.min, pos2(full.max.x, (full.max.y - status_h).max(full.min.y)));
    let status_rect = Rect::from_min_max(pos2(full.min.x, grid_rect.max.y), full.max);
    ui.allocate_rect(full, Sense::hover());

    // Keyboard: Cmd+C copies, arrows move the selection (only while the grid has focus).
    let focused = ui.memory(|m| m.has_focus(id));
    if focused && data.editable && state.editing.is_none() {
        // Consumed, so the text field that opens does not see the Enter and close again.
        let start = ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, Key::Enter) || i.consume_key(egui::Modifiers::NONE, Key::F2)
        });
        if start && let Some(Selection::Cell { row, col }) = state.selection {
            state.editing = Some(start_edit(row, col));
        }
    }
    if focused && state.editing.is_none() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, tab: false, escape: false },
            )
        });
        let (copy, moves) = ui.input(|i| {
            let copy = i.events.iter().any(|e| {
                matches!(e, Event::Copy)
                    || matches!(e, Event::Key { key: Key::C, pressed: true, modifiers, .. } if modifiers.command)
            });
            let mut moves = (0i64, 0i64);
            for e in &i.events {
                if let Event::Key { key, pressed: true, modifiers, .. } = e
                    && !modifiers.command
                {
                    match key {
                        Key::ArrowDown => moves.0 += 1,
                        Key::ArrowUp => moves.0 -= 1,
                        Key::ArrowRight => moves.1 += 1,
                        Key::ArrowLeft => moves.1 -= 1,
                        Key::PageDown => moves.0 += 20,
                        Key::PageUp => moves.0 -= 20,
                        _ => {}
                    }
                }
            }
            (copy, moves)
        });
        if copy && let Some(text) = selection_text(state.selection, data.rows) {
            actions.push(GridAction::Copy(text));
        }
        if moves != (0, 0) && n_rows > 0 {
            let clamp = |v: i64, n: usize| usize::try_from(v.clamp(0, n.saturating_sub(1) as i64)).unwrap_or(0);
            state.selection = Some(match state.selection {
                Some(Selection::Cell { row, col }) => Selection::Cell {
                    row: clamp(row as i64 + moves.0, n_rows),
                    col: clamp(col as i64 + moves.1, n_cols.max(1)),
                },
                Some(Selection::Row(row)) => Selection::Row(clamp(row as i64 + moves.0, n_rows)),
                None => Selection::Cell { row: 0, col: 0 },
            });
            state.reveal = true;
        }
    }

    // The grid itself is a focusable widget (keyboard navigation; accessibility needs a node for
    // whatever has focus).
    ui.interact(grid_rect, id, Sense::focusable_noninteractive());

    // Body.
    let body_rect = Rect::from_min_max(pos2(grid_rect.min.x, grid_rect.min.y + HEADER_H), grid_rect.max);
    let mut body_ui =
        ui.new_child(egui::UiBuilder::new().max_rect(body_rect).layout(egui::Layout::top_down(egui::Align::Min)));
    let mut area = ScrollArea::both().id_salt(id.with("body")).auto_shrink([false, false]);
    if state.reveal {
        if let Some(Selection::Cell { row, .. } | Selection::Row(row)) = state.selection {
            let y = row as f32 * (ROW_H + body_ui.spacing().item_spacing.y);
            let current = egui::scroll_area::State::load(ui.ctx(), body_ui.make_persistent_id(id.with("body")))
                .map(|s| s.offset.y)
                .unwrap_or(0.0);
            let view_h = body_rect.height() - 16.0;
            if y < current {
                area = area.vertical_scroll_offset(y);
            } else if y + ROW_H > current + view_h {
                area = area.vertical_scroll_offset((y + ROW_H - view_h).max(0.0));
            }
        }
        state.reveal = false;
    }
    let mut clicked: Option<Selection> = None;
    let mut copy_menu: Option<String> = None;
    let mut menu_actions: Vec<GridAction> = Vec::new();
    let mut start_editing: Option<(usize, usize)> = None;
    // `Some(None)`: editing cancelled; `Some(Some(..))`: committed.
    let mut edit_done: Option<Option<(usize, usize, String)>> = None;
    let out = area.show_rows(&mut body_ui, ROW_H, n_rows, |ui, range| {
        if range.end + 20 >= n_rows && n_rows > 0 {
            actions.push(GridAction::NearEnd);
        }
        let clip = ui.clip_rect();
        for r in range {
            if r >= n_rows {
                continue;
            }
            let row_state = data.edits.map_or(RowState::Unchanged, |e| e.row_state(r, fetched));
            let (rect, resp) = ui.allocate_exact_size(vec2(total_w.max(ui.available_width()), ROW_H), Sense::click());
            if !ui.is_rect_visible(rect) {
                continue;
            }
            let painter = ui.painter().clone();
            let row_selected = state.selection == Some(Selection::Row(r));
            if row_selected {
                painter.rect_filled(rect, CornerRadius::ZERO, theme.list_selection);
            } else if row_state == RowState::Inserted {
                painter.rect_filled(rect, CornerRadius::ZERO, theme.diff_added_bg);
            } else if row_state == RowState::Deleted {
                painter.rect_filled(rect, CornerRadius::ZERO, theme.diff_removed_bg);
            } else if r % 2 == 1 {
                painter.rect_filled(rect, CornerRadius::ZERO, theme.caret_row);
            }
            // Row number.
            let num_rect = Rect::from_min_size(rect.min, vec2(num_w, ROW_H));
            painter.text(
                pos2(num_rect.max.x - 6.0, num_rect.center().y),
                Align2::RIGHT_CENTER,
                (r + 1).to_string(),
                font.clone(),
                theme.line_number,
            );
            let mut x = rect.min.x + num_w;
            for (c, w) in state.widths.iter().enumerate() {
                let cell_rect = Rect::from_min_size(pos2(x, rect.min.y), vec2(*w, ROW_H));
                x += w;
                if cell_rect.max.x < clip.min.x || cell_rect.min.x > clip.max.x {
                    continue;
                }
                let cv = view(r, c);
                if matches!(cv, CellView::Changed(_)) && row_state != RowState::Inserted {
                    painter.rect_filled(cell_rect, CornerRadius::ZERO, theme.diff_modified_bg);
                }
                if let Some(ed) = state.editing.as_mut().filter(|e| e.row == r && e.col == c) {
                    let edit_id = id.with(("edit", r, c));
                    let te = egui::TextEdit::singleline(&mut ed.text)
                        .id(edit_id)
                        .font(font.clone())
                        .margin(egui::Margin::symmetric(4, 2))
                        .desired_width(cell_rect.width());
                    let out = ui.put(cell_rect.shrink(1.0), te);
                    let first = ed.focus;
                    if first {
                        out.request_focus();
                        ed.focus = false;
                    }
                    let (enter, escape) = ui.input(|i| (i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
                    // The key that started editing is not the one that ends it.
                    if !first && escape {
                        edit_done = Some(None);
                    } else if !first && (enter || out.lost_focus()) {
                        edit_done = Some(Some((r, c, ed.text.clone())));
                    }
                    painter.vline(cell_rect.max.x, cell_rect.y_range(), Stroke::new(1.0, theme.separator));
                    continue;
                }
                let default_cell = Cell::Null;
                let (value, is_default) = match cv {
                    CellView::Value(v) | CellView::Changed(v) => (v, false),
                    CellView::Default => (&default_cell, true),
                    CellView::Missing => (&default_cell, false),
                };
                if state.selection == Some(Selection::Cell { row: r, col: c }) {
                    painter.rect_filled(cell_rect.shrink(1.0), CornerRadius::same(2), theme.selection);
                    painter.rect_stroke(
                        cell_rect.shrink(1.0),
                        CornerRadius::same(2),
                        Stroke::new(1.0, theme.accent),
                        egui::StrokeKind::Inside,
                    );
                }
                let color = if value.is_null() {
                    theme.text_disabled
                } else if value.is_numeric() {
                    theme.number
                } else {
                    theme.text_editor
                };
                let text = if is_default { "<default>".to_string() } else { cell_text(value) };
                let p = painter.with_clip_rect(cell_rect.shrink2(vec2(4.0, 0.0)).intersect(clip));
                if value.is_numeric() {
                    p.text(
                        pos2(cell_rect.max.x - 6.0, cell_rect.center().y),
                        Align2::RIGHT_CENTER,
                        text,
                        font.clone(),
                        color,
                    );
                } else {
                    p.text(
                        pos2(cell_rect.min.x + 6.0, cell_rect.center().y),
                        Align2::LEFT_CENTER,
                        text,
                        font.clone(),
                        color,
                    );
                }
                painter.vline(cell_rect.max.x, cell_rect.y_range(), Stroke::new(1.0, theme.separator));
            }
            if row_state == RowState::Deleted {
                let y = rect.center().y;
                painter.hline((rect.min.x + num_w)..=x, y, Stroke::new(1.0, theme.diff_removed));
            }
            if resp.double_clicked()
                && data.editable
                && row_state != RowState::Deleted
                && let Some(pos) = resp.interact_pointer_pos()
                && let Selection::Cell { row, col } = hit(pos.x - rect.min.x, num_w, &state.widths, r)
            {
                start_editing = Some((row, col));
            }
            if (resp.clicked() || resp.secondary_clicked())
                && let Some(pos) = resp.interact_pointer_pos()
            {
                clicked = Some(hit(pos.x - rect.min.x, num_w, &state.widths, r));
            }
            resp.context_menu(|ui| {
                let col = match state.selection {
                    Some(Selection::Cell { row, col }) if row == r => col,
                    _ => 0,
                };
                let row = data.rows.get(r);
                if ui.add_enabled(row.is_some(), egui::Button::new("Copy Value")).clicked() {
                    copy_menu = row.and_then(|row| row.get(col)).map(Cell::to_plain);
                    ui.close();
                }
                if ui.add_enabled(row.is_some(), egui::Button::new("Copy Row")).clicked() {
                    copy_menu = row.map(|row| row_text(row));
                    ui.close();
                }
                if data.editable {
                    ui.separator();
                    let live = row_state != RowState::Deleted;
                    if ui.add_enabled(live, egui::Button::new("Edit Value")).clicked() {
                        start_editing = Some((r, col));
                        ui.close();
                    }
                    if ui.add_enabled(live, egui::Button::new("Set NULL")).clicked() {
                        menu_actions.push(GridAction::Edit { row: r, col, value: None });
                        ui.close();
                    }
                    if ui.button("Add Row").clicked() {
                        menu_actions.push(GridAction::AddRow);
                        ui.close();
                    }
                    if ui.add_enabled(live, egui::Button::new("Delete Row")).clicked() {
                        menu_actions.push(GridAction::DeleteRow(r));
                        ui.close();
                    }
                    if ui.add_enabled(row_state != RowState::Unchanged, egui::Button::new("Revert Row")).clicked() {
                        menu_actions.push(GridAction::RevertRow(r));
                        ui.close();
                    }
                }
            });
        }
    });
    if let Some(sel) = clicked {
        state.selection = Some(sel);
        ui.memory_mut(|m| m.request_focus(id));
    }
    if let Some(t) = copy_menu {
        actions.push(GridAction::Copy(t));
    }
    actions.extend(menu_actions);
    if let Some(done) = edit_done {
        if let Some((row, col, text)) = done {
            actions.push(GridAction::Edit { row, col, value: Some(text) });
        }
        state.editing = None;
        ui.memory_mut(|m| m.request_focus(id));
    }
    if let Some((row, col)) = start_editing {
        state.selection = Some(Selection::Cell { row, col });
        state.editing = Some(start_edit(row, col));
    }
    let offset_x = out.state.offset.x;

    // Header (drawn after the body so it uses this frame's horizontal offset).
    let header_rect = Rect::from_min_size(grid_rect.min, vec2(grid_rect.width(), HEADER_H));
    let hp = ui.painter_at(header_rect);
    hp.rect_filled(header_rect, CornerRadius::ZERO, theme.header_bg);
    hp.hline(header_rect.x_range(), header_rect.max.y - 0.5, Stroke::new(1.0, theme.separator));
    let mut x = header_rect.min.x + num_w - offset_x;
    for (c, col) in data.columns.iter().enumerate() {
        let w = state.widths.get(c).copied().unwrap_or(80.0);
        let rect = Rect::from_min_size(pos2(x, header_rect.min.y), vec2(w, HEADER_H));
        x += w;
        let visible = rect.intersect(header_rect);
        if visible.width() <= 0.0 {
            continue;
        }
        let resp = ui.interact(visible, id.with(("hdr", c)), Sense::click());
        if resp.hovered() {
            hp.rect_filled(visible, CornerRadius::ZERO, theme.hover);
        }
        let arrow = match data.sort {
            Some((sc, desc)) if sc == c => {
                if desc {
                    " ▼"
                } else {
                    " ▲"
                }
            }
            _ => "",
        };
        let label = format!("{}{arrow}", col.name);
        let tp = hp.with_clip_rect(rect.shrink2(vec2(4.0, 0.0)).intersect(header_rect));
        tp.text(
            pos2(rect.min.x + 6.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            FontId::proportional(12.5),
            theme.text,
        );
        hp.vline(rect.max.x, rect.y_range(), Stroke::new(1.0, theme.separator));
        let resp = resp.on_hover_text(if col.type_name.is_empty() {
            col.name.clone()
        } else {
            format!("{}: {}", col.name, col.type_name)
        });
        // Resize handle on the right edge.
        let handle = Rect::from_min_max(pos2(rect.max.x - 3.0, rect.min.y), pos2(rect.max.x + 3.0, rect.max.y))
            .intersect(header_rect);
        let drag = ui.interact(handle, id.with(("resize", c)), Sense::drag());
        if drag.hovered() || drag.dragged() {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if drag.dragged()
            && let Some(wm) = state.widths.get_mut(c)
        {
            *wm = (*wm + drag.drag_delta().x).max(MIN_COL_W);
        } else if resp.clicked() {
            actions.push(GridAction::Sort(c));
        }
    }
    // Row-number header cell (covers scrolled-under columns).
    let corner = Rect::from_min_size(header_rect.min, vec2(num_w.min(header_rect.width()), HEADER_H));
    hp.rect_filled(corner, CornerRadius::ZERO, theme.header_bg);
    hp.vline(corner.max.x, corner.y_range(), Stroke::new(1.0, theme.separator));

    // Status line.
    let sp = ui.painter_at(status_rect);
    sp.hline(status_rect.x_range(), status_rect.min.y + 0.5, Stroke::new(1.0, theme.separator));
    let color = if data.status_is_error { theme.error } else { theme.text_muted };
    sp.text(
        pos2(status_rect.min.x + 6.0, status_rect.center().y),
        Align2::LEFT_CENTER,
        data.status,
        FontId::proportional(12.0),
        color,
    );
    if n_cols == 0 && n_rows == 0 {
        ui.painter_at(body_rect).text(
            body_rect.center(),
            Align2::CENTER_CENTER,
            "No columns",
            FontId::proportional(13.0),
            theme.text_muted,
        );
    }
    actions
}

/// Which cell (or row number) an x offset within a row hits.
fn hit(x: f32, num_w: f32, widths: &[f32], row: usize) -> Selection {
    if x < num_w {
        return Selection::Row(row);
    }
    let mut left = num_w;
    for (c, w) in widths.iter().enumerate() {
        if x < left + w {
            return Selection::Cell { row, col: c };
        }
        left += w;
    }
    Selection::Cell { row, col: widths.len().saturating_sub(1) }
}

fn selection_text(sel: Option<Selection>, rows: &[Vec<Cell>]) -> Option<String> {
    match sel? {
        Selection::Cell { row, col } => rows.get(row)?.get(col).map(Cell::to_plain),
        Selection::Row(row) => rows.get(row).map(|r| row_text(r)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_testing() {
        let w = [50.0, 100.0];
        assert_eq!(hit(5.0, 30.0, &w, 3), Selection::Row(3));
        assert_eq!(hit(40.0, 30.0, &w, 3), Selection::Cell { row: 3, col: 0 });
        assert_eq!(hit(90.0, 30.0, &w, 3), Selection::Cell { row: 3, col: 1 });
        assert_eq!(hit(900.0, 30.0, &w, 3), Selection::Cell { row: 3, col: 1 });
        assert_eq!(hit(900.0, 30.0, &[], 0), Selection::Cell { row: 0, col: 0 });
    }

    #[test]
    fn texts() {
        assert_eq!(cell_text(&Cell::Null), "<null>");
        assert_eq!(cell_text(&Cell::Text("a\nb".into())), "a↵b");
        assert_eq!(cell_text(&Cell::Text("x".repeat(400))).chars().count(), MAX_CELL_CHARS + 1);
        let row = vec![Cell::Int(1), Cell::Null, Cell::Text("t".into())];
        assert_eq!(row_text(&row), "1\t\tt");
        assert_eq!(
            selection_text(Some(Selection::Cell { row: 0, col: 2 }), std::slice::from_ref(&row)).as_deref(),
            Some("t")
        );
        assert_eq!(selection_text(Some(Selection::Row(0)), &[row]).as_deref(), Some("1\t\tt"));
        assert_eq!(selection_text(Some(Selection::Row(5)), &[]), None);
    }

    #[test]
    fn enter_edits_a_cell_in_place() {
        use egui_kittest::Harness;

        let columns = vec![
            Column { name: "id".into(), type_name: String::new() },
            Column { name: "name".into(), type_name: String::new() },
        ];
        let rows = vec![vec![Cell::Int(1), Cell::Text("ab".into())]];
        let mut edits = GridEdits::default();
        edits.inserted.push(vec![None, None]);
        let theme = Theme::dark();
        let state = (GridState::default(), Vec::new(), 0);
        let mut h = Harness::new_ui_state(
            move |ui, (grid, actions, frame): &mut (GridState, Vec<GridAction>, usize)| {
                let id = Id::new("grid-test");
                if *frame == 0 {
                    ui.memory_mut(|m| m.request_focus(id));
                }
                *frame += 1;
                let data = GridData {
                    columns: &columns,
                    rows: &rows,
                    sort: None,
                    status: "",
                    status_is_error: false,
                    edits: Some(&edits),
                    editable: true,
                };
                actions.extend(show(ui, id, grid, &data, &theme));
            },
            state,
        );
        h.run_steps(2);
        // (Widths are computed on the first frame, which resets the selection.)
        h.state_mut().0.selection = Some(Selection::Cell { row: 0, col: 1 });
        h.key_press(Key::Enter);
        h.run_steps(2);
        assert!(h.state().0.editing.is_some(), "Enter starts editing");
        h.event(Event::Text("z".into()));
        h.run_steps(1);
        h.key_press(Key::Enter);
        h.run_steps(2);
        let edit = h.state().1.iter().find_map(|a| match a {
            GridAction::Edit { row, col, value } => Some((*row, *col, value.clone())),
            _ => None,
        });
        let (row, col, value) = edit.expect("an edit action");
        assert_eq!((row, col), (0, 1));
        assert!(value.as_deref().is_some_and(|v| v.contains('z')), "{value:?}");
        assert!(h.state().0.editing.is_none());
        // Escape cancels.
        h.key_press(Key::F2);
        h.run_steps(2);
        assert!(h.state().0.editing.is_some());
        h.key_press(Key::Escape);
        h.run_steps(2);
        assert!(h.state().0.editing.is_none());
        assert_eq!(h.state().1.iter().filter(|a| matches!(a, GridAction::Edit { .. })).count(), 1);
    }

    #[test]
    fn widths_fit_content() {
        let cols = vec![
            Column { name: "id".into(), type_name: String::new() },
            Column { name: "a_long_column_name".into(), type_name: String::new() },
        ];
        let rows = vec![vec![Cell::Int(1), Cell::Text("x".repeat(500))]];
        let w = initial_widths(&cols, &rows, 7.0);
        assert!(w[0] >= MIN_COL_W && w[1] <= MAX_INIT_COL_W);
    }
}
