//! The Terminal tool window. OWNED BY the terminal track.
//!
//! A tab strip (+, ×, Open Claude Code; double-click or right-click a tab to rename it), the grid (per-cell colours and attributes, cursor
//! shapes and blink, mouse selection, wheel scrollback, a scrollbar, Cmd+click links) and the
//! keyboard routing used while the grid has focus ([`handle_keys`]). Terminal state lives in the
//! engine (`Session::terminal`); this module paints snapshots and keeps view state only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Color32, CursorIcon, Event, FontId, Id, Key, Layout, Modifiers, Rect, RichText, Sense, Stroke, Ui, pos2,
    vec2,
};
use serde_json::{Value, json};

use codecraft_engine::term::{
    self as term, CellFlags, Color as TermColor, CursorShape, LinkTarget, Mods, MouseAction, MouseButton, Screen,
    SelectionKind,
};
use codecraft_engine::terminal::TabStatus;
use codecraft_engine::{Context, Session};

use crate::theme::Theme;
use crate::{UiState, run_command};

const SCROLLBAR_W: f32 = 10.0;
const PAD_X: f32 = 6.0;
const PAD_Y: f32 = 2.0;
const BLINK: Duration = Duration::from_millis(530);
const MULTI_CLICK: Duration = Duration::from_millis(400);

/// View state of the Terminal tool window (kept in [`UiState`]).
#[derive(Default)]
pub struct TerminalUi {
    installed: bool,
    /// Frame the window was last painted (to notice when it is shown again).
    last_shown_frame: Option<u64>,
    shown_before: bool,
    spawn_attempted: bool,
    was_focused: bool,
    selecting: Option<(usize, usize, bool)>,
    moved: bool,
    reported: Option<MouseButton>,
    last_click: Option<(Instant, usize, usize, u8)>,
    last_motion_cell: Option<(usize, usize)>,
    wheel: f32,
    scrollbar_drag: bool,
    blink_start: Option<Instant>,
    /// The tab whose name is being edited in place, and the text so far.
    renaming: Option<(u64, String)>,
    /// The name field should take keyboard focus (it was just opened).
    rename_focus: bool,
}

/// The rename field's widget id.
fn rename_id() -> Id {
    Id::new("codecraft-terminal-rename")
}

/// The grid's widget id (keyboard focus).
pub fn grid_id() -> Id {
    Id::new("codecraft-terminal-grid")
}

/// Whether the terminal grid has keyboard focus (then [`handle_keys`] gets every key).
pub fn has_focus(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(grid_id()))
}

pub(crate) fn palette(theme: &Theme) -> term::Palette {
    let rgb = |c: Color32| (c.r(), c.g(), c.b());
    let mut ansi = [(0, 0, 0); 16];
    for (dst, src) in ansi.iter_mut().zip(theme.terminal_ansi.iter()) {
        *dst = rgb(*src);
    }
    term::Palette { fg: rgb(theme.text_editor), bg: rgb(theme.editor_bg), cursor: rgb(theme.caret), ansi }
}

enum Action {
    Command(&'static str, Value),
    OpenUrl(String),
}

pub fn show(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let started = std::time::Instant::now();
    show_inner(ui, s, st);
    s.record_frame_section("terminal paint", started.elapsed());
}

fn show_inner(ui: &mut Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    if !st.terminal.installed {
        let waker_ctx = ctx.clone();
        s.terminal.set_waker(Arc::new(move || waker_ctx.request_repaint_after(std::time::Duration::from_millis(17))));
        s.terminal.set_palette(palette(&st.theme));
        st.terminal.installed = true;
    }
    let frame = ctx.cumulative_frame_nr();
    let newly_shown = st.terminal.last_shown_frame.is_none_or(|f| f.saturating_add(1) < frame);
    st.terminal.last_shown_frame = Some(frame);
    if newly_shown {
        st.terminal.spawn_attempted = false;
        // Showing the window (Alt+F12) focuses the terminal; not when it is restored at startup.
        if st.terminal.shown_before {
            s.terminal.focus_requested = true;
        }
    }
    // RustRover opens a "Local" tab whenever the window is shown without one.
    if s.terminal.tabs.is_empty() && !st.terminal.spawn_attempted {
        st.terminal.spawn_attempted = true;
        run_command(s, st, "terminal.new", &Value::Null);
        if !st.terminal.shown_before {
            s.terminal.focus_requested = false;
        }
    }
    st.terminal.shown_before = true;

    let theme = st.theme.clone();
    let mut actions = Vec::new();
    if let Some(id) = s.terminal.rename_requested.take() {
        start_rename(s, &mut st.terminal, id);
    }
    tab_strip(ui, s, &mut st.terminal, &theme, &mut actions);
    if s.terminal.tabs.is_empty() {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("No terminal is open.").color(theme.text_muted));
            if ui.link("New tab").clicked() {
                actions.push(Action::Command("terminal.new", Value::Null));
            }
        });
    } else {
        grid(ui, s, st, &theme, &mut actions);
    }
    for action in actions {
        match action {
            Action::Command(id, params) => run_command(s, st, id, &params),
            Action::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
        }
    }
}

// ---------------------------------------------------------------- tab strip

fn start_rename(s: &Session, view: &mut TerminalUi, id: u64) {
    if let Some(tab) = s.terminal.tabs.iter().find(|t| t.id == id) {
        view.renaming = Some((id, tab.title().to_string()));
        view.rename_focus = true;
    }
}

/// The in-place name field: Enter or clicking elsewhere renames, Escape cancels.
fn rename_field(ui: &mut Ui, view: &mut TerminalUi, actions: &mut Vec<Action>) {
    let Some((id, text)) = view.renaming.as_mut() else { return };
    let id = *id;
    let resp = ui.add(
        egui::TextEdit::singleline(text)
            .id(rename_id())
            .desired_width(140.0)
            .font(FontId::proportional(12.5))
            .hint_text("Tab name"),
    );
    if view.rename_focus {
        view.rename_focus = false;
        resp.request_focus();
        if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), rename_id()) {
            let all = egui::text::CCursorRange::two(
                egui::text::CCursor::new(0),
                egui::text::CCursor::new(text.chars().count()),
            );
            state.cursor.set_char_range(Some(all));
            state.store(ui.ctx(), rename_id());
        }
        return;
    }
    let cancel = ui.input(|i| i.key_pressed(Key::Escape));
    if resp.lost_focus() || cancel || !resp.has_focus() {
        if !cancel {
            actions.push(Action::Command("terminal.rename", json!({ "tab": id, "title": text.as_str() })));
        }
        view.renaming = None;
        // Back to the shell.
        actions.push(Action::Command("terminal.activate", json!({ "tab": id })));
        ui.memory_mut(|m| m.request_focus(grid_id()));
    }
}

fn tab_strip(ui: &mut Ui, s: &Session, view: &mut TerminalUi, theme: &Theme, actions: &mut Vec<Action>) {
    if view.renaming.as_ref().is_some_and(|(id, _)| s.terminal.index_of(*id).is_none()) {
        view.renaming = None;
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        for (i, tab) in s.terminal.tabs.iter().enumerate() {
            let selected = i == s.terminal.active;
            if view.renaming.as_ref().is_some_and(|(id, _)| *id == tab.id) {
                rename_field(ui, view, actions);
                ui.add_space(4.0);
                continue;
            }
            let mut title: String = tab.title().chars().take(32).collect();
            if tab.title().chars().count() > 32 {
                title.push('…');
            }
            let (mark, mark_color) = match &tab.status {
                TabStatus::Running if tab.bell && !selected => ("● ", theme.warning),
                TabStatus::Running => ("", theme.text),
                TabStatus::Exited(info) if info.code == Some(0) => ("✓ ", theme.success),
                TabStatus::Exited(_) => ("✗ ", theme.error),
            };
            let mut job = LayoutJob::default();
            if !mark.is_empty() {
                job.append(mark, 0.0, TextFormat::simple(FontId::proportional(12.0), mark_color));
            }
            let color = if selected { theme.text } else { theme.text_muted };
            job.append(&title, 0.0, TextFormat::simple(FontId::proportional(12.5), color));
            let resp = ui.add(egui::Button::selectable(selected, job));
            let tip = match &tab.command {
                Some(c) => format!("{}\n{c}", tab.cwd.display()),
                None => tab.cwd.display().to_string(),
            };
            let resp = resp.on_hover_text(format!("{tip}\nDouble-click to rename"));
            if resp.double_clicked() {
                start_rename(s, view, tab.id);
            } else if resp.clicked() {
                actions.push(Action::Command("terminal.activate", json!({ "tab": tab.id })));
            }
            resp.context_menu(|ui| {
                if ui.button("Rename Session…").clicked() {
                    start_rename(s, view, tab.id);
                    ui.close();
                }
                if tab.renamed && ui.button("Use Automatic Name").clicked() {
                    actions.push(Action::Command("terminal.rename", json!({ "tab": tab.id, "title": "" })));
                    ui.close();
                }
                ui.separator();
                if ui.add(egui::Button::new("Close Tab").shortcut_text("⌘W")).clicked() {
                    actions.push(Action::Command("terminal.close", json!({ "tab": tab.id })));
                    ui.close();
                }
            });
            if ui
                .add(egui::Button::new(RichText::new("×").color(theme.text_muted)).frame(false))
                .on_hover_text("Close tab (⌘W)")
                .clicked()
            {
                actions.push(Action::Command("terminal.close", json!({ "tab": tab.id })));
            }
            ui.add_space(4.0);
        }
        if ui
            .add(egui::Button::new(RichText::new("+").color(theme.text)).frame(false))
            .on_hover_text("New tab (⌘T)")
            .clicked()
        {
            actions.push(Action::Command("terminal.new", Value::Null));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.menu_button(RichText::new("…").color(theme.text_muted), |ui| {
                let has = !s.terminal.tabs.is_empty();
                if ui.add_enabled(has, egui::Button::new("Rename Tab…")).clicked() {
                    if let Some(id) = s.terminal.active_tab().map(|t| t.id) {
                        start_rename(s, view, id);
                    }
                    ui.close();
                }
                for (label, id, hint) in [
                    ("Copy", "terminal.copy", "⌘C"),
                    ("Paste", "terminal.paste", "⌘V"),
                    ("Select All", "terminal.selectAll", "⌘A"),
                    ("Clear", "terminal.clear", "⌘K"),
                ] {
                    if ui.add_enabled(has, egui::Button::new(label).shortcut_text(hint)).clicked() {
                        actions.push(Action::Command(id, Value::Null));
                        ui.close();
                    }
                }
                ui.separator();
                let mut meta = s.terminal.alt_is_meta;
                if ui.checkbox(&mut meta, "Use Option as Meta key").clicked() {
                    actions.push(Action::Command("terminal.toggleAltAsMeta", json!({ "enabled": meta })));
                    ui.close();
                }
            });
            if ui
                .add(egui::Button::new(RichText::new("✳ Claude Code").color(theme.text)).frame(false))
                .on_hover_text("Open Claude Code in a terminal tab (connected to CodeCraft)")
                .clicked()
            {
                actions.push(Action::Command("terminal.openClaude", Value::Null));
            }
        });
    });
}

// ---------------------------------------------------------------- grid

struct Metrics {
    font: FontId,
    cw: f32,
    ch: f32,
    glyph_h: f32,
    text: Rect,
}

impl Metrics {
    /// The cell under a point, clamped to the grid, and whether it is on the cell's right half.
    fn cell_at(&self, p: egui::Pos2, screen: &Screen) -> (usize, usize, bool) {
        let fx = ((p.x - self.text.left()) / self.cw).max(0.0);
        let fy = ((p.y - self.text.top()) / self.ch).max(0.0);
        let col = (fx.floor() as usize).min(screen.cols.saturating_sub(1));
        let row = (fy.floor() as usize).min(screen.rows.saturating_sub(1));
        (row, col, fx.fract() >= 0.5)
    }

    fn cell_rect(&self, row: usize, col: usize, width: usize) -> Rect {
        Rect::from_min_size(
            pos2(self.text.left() + col as f32 * self.cw, self.text.top() + row as f32 * self.ch),
            vec2(self.cw * width as f32, self.ch),
        )
    }
}

fn grid(ui: &mut Ui, s: &mut Session, st: &mut UiState, theme: &Theme, actions: &mut Vec<Action>) {
    let font_size = s.ws.settings.editor_font_size.clamp(6.0, 72.0);
    let font = FontId::monospace(font_size);
    let (cw, glyph_h) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
    let cw = cw.max(1.0);
    let ch = (font_size * 1.3).max(glyph_h).round().max(1.0);
    let active = s.terminal.active;
    let exited = s.terminal.tabs.get(active).and_then(|t| match &t.status {
        TabStatus::Exited(info) => Some(info.describe()),
        TabStatus::Running => None,
    });
    let avail = ui.available_rect_before_wrap();
    let banner_h = if exited.is_some() { 26.0 } else { 0.0 };
    let rect = Rect::from_min_max(avail.min, pos2(avail.max.x, (avail.max.y - banner_h).max(avail.min.y + ch)));
    let text = Rect::from_min_max(
        pos2(rect.left() + PAD_X, rect.top() + PAD_Y),
        pos2((rect.right() - SCROLLBAR_W - PAD_X).max(rect.left() + PAD_X + 2.0 * cw), rect.bottom()),
    );
    let m = Metrics { font, cw, ch, glyph_h, text };
    let cols = ((text.width() / cw).floor() as usize).clamp(2, usize::from(term::MAX_COLS));
    let rows = ((text.height() / ch).floor() as usize).clamp(1, usize::from(term::MAX_ROWS));
    let id = grid_id();
    let response = ui.interact(rect, id, Sense::click_and_drag());
    let track = Rect::from_min_max(pos2(rect.right() - SCROLLBAR_W, rect.top()), rect.max);
    let sb_response = ui.interact(track, id.with("scrollbar"), Sense::click_and_drag());

    // Focus.
    let pointer = ui.input(|i| {
        (
            i.pointer.interact_pos(),
            i.pointer.hover_pos(),
            i.pointer.primary_pressed(),
            i.pointer.primary_down(),
            i.pointer.primary_released(),
            i.modifiers,
            i.smooth_scroll_delta.y,
        )
    });
    let (interact_pos, hover_pos, pressed, down, released, mods, wheel) = pointer;
    if s.terminal.focus_requested {
        s.terminal.focus_requested = false;
        ui.memory_mut(|mem| mem.request_focus(id));
    }
    if pressed && interact_pos.is_some_and(|p| rect.contains(p)) {
        ui.memory_mut(|mem| mem.request_focus(id));
    }
    let focused = ui.memory(|mem| mem.has_focus(id));
    let Some(tab) = s.terminal.tabs.get_mut(active) else { return };
    if focused != st.terminal.was_focused {
        st.terminal.was_focused = focused;
        st.terminal.blink_start = Some(Instant::now());
        tab.term.focus_changed(focused);
    }
    tab.bell = false;

    // Size: the PTY follows the panel.
    if tab.term.size() != (cols as u16, rows as u16) {
        tab.term.resize(cols as u16, rows as u16, (cw.round() as u16, ch as u16));
        tab.refresh();
    }

    let modes = tab.screen.modes;
    let tmods = Mods { ctrl: mods.ctrl, alt: mods.alt, shift: mods.shift };
    let in_grid = |p: egui::Pos2| rect.contains(p) && !track.contains(p);

    // Wheel: report to the program, arrow keys on the alternate screen, or scrollback.
    if wheel != 0.0 && hover_pos.is_some_and(in_grid) {
        st.terminal.wheel += wheel / ch;
        let lines = st.terminal.wheel.trunc();
        st.terminal.wheel -= lines;
        let n = (lines.abs() as usize).min(20);
        if n > 0 {
            let up = lines > 0.0;
            if modes.mouse_reporting() && !mods.shift {
                let (row, col, _) = m.cell_at(hover_pos.unwrap_or(text.min), &tab.screen);
                let button = if up { MouseButton::WheelUp } else { MouseButton::WheelDown };
                for _ in 0..n {
                    tab.term.mouse(button, MouseAction::Press, col, row, tmods);
                }
            } else if modes.alt_screen && modes.alternate_scroll {
                let key = if up { term::Key::Up } else { term::Key::Down };
                for _ in 0..n {
                    tab.term.send_key(key, Mods::NONE);
                }
            } else {
                tab.term.scroll(term::Scroll::Lines(if up { n as i32 } else { -(n as i32) }));
                tab.refresh();
            }
        }
    }

    // Links under the pointer while Cmd is held.
    let hover_cell = hover_pos.filter(|p| in_grid(*p)).map(|p| m.cell_at(p, &tab.screen));
    let link =
        hover_cell.filter(|_| mods.mac_cmd || mods.command).and_then(|(row, col, _)| link_at(s, active, row, col));
    let link_action = link.as_ref().map(|(_, _, _, target)| open_link(s, target));
    let Some(tab) = s.terminal.tabs.get_mut(active) else { return };
    if link.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    } else if response.hovered() && hover_pos.is_some_and(|p| !track.contains(p)) {
        ui.ctx().set_cursor_icon(CursorIcon::Text);
    }

    // Mouse buttons: links, reporting to the program, or selection.
    let report = modes.mouse_reporting() && !mods.shift;
    if pressed && let Some(p) = interact_pos.filter(|p| in_grid(*p)) {
        let (row, col, right) = m.cell_at(p, &tab.screen);
        if let Some(action) = link_action {
            actions.push(action);
        } else if report {
            if tab.term.mouse(MouseButton::Left, MouseAction::Press, col, row, tmods) {
                st.terminal.reported = Some(MouseButton::Left);
            }
        } else {
            let now = Instant::now();
            let count = match st.terminal.last_click {
                Some((t, r, c, n)) if now.duration_since(t) < MULTI_CLICK && r == row && c.abs_diff(col) <= 1 => {
                    n % 3 + 1
                }
                _ => 1,
            };
            st.terminal.last_click = Some((now, row, col, count));
            let kind = match count {
                2 => SelectionKind::Word,
                3 => SelectionKind::Line,
                _ if mods.alt => SelectionKind::Block,
                _ => SelectionKind::Simple,
            };
            tab.term.start_selection(kind, row, col, right);
            st.terminal.selecting = Some((row, col, right));
            st.terminal.moved = count > 1;
            tab.refresh();
        }
    } else if down && let Some(p) = interact_pos {
        let (row, col, right) = m.cell_at(p, &tab.screen);
        if st.terminal.reported.is_some() {
            if st.terminal.last_motion_cell != Some((row, col)) {
                tab.term.mouse(MouseButton::Left, MouseAction::Motion, col, row, tmods);
            }
        } else if let Some(start) = st.terminal.selecting
            && start != (row, col, right)
        {
            // Dragging past the top or bottom edge scrolls the scrollback.
            if p.y < text.top() && !modes.alt_screen {
                tab.term.scroll(term::Scroll::Lines(1));
            } else if p.y > text.bottom() && !modes.alt_screen {
                tab.term.scroll(term::Scroll::Lines(-1));
            }
            tab.term.update_selection(row, col, right);
            st.terminal.moved = true;
            tab.refresh();
        }
        st.terminal.last_motion_cell = Some((row, col));
    } else if let Some(p) = hover_pos.filter(|p| in_grid(*p))
        && modes.mouse_motion
        && !down
    {
        let (row, col, _) = m.cell_at(p, &tab.screen);
        if st.terminal.last_motion_cell != Some((row, col)) {
            tab.term.mouse(MouseButton::None, MouseAction::Motion, col, row, tmods);
            st.terminal.last_motion_cell = Some((row, col));
        }
    }
    if released {
        if let Some(button) = st.terminal.reported.take() {
            let (row, col, _) = m.cell_at(interact_pos.unwrap_or(text.min), &tab.screen);
            tab.term.mouse(button, MouseAction::Release, col, row, tmods);
        }
        if st.terminal.selecting.take().is_some() && !st.terminal.moved {
            tab.term.clear_selection();
            tab.refresh();
        }
    }
    if !report {
        response.context_menu(|ui| {
            let has_selection = tab.screen.selection.is_some();
            if ui.add_enabled(has_selection, egui::Button::new("Copy").shortcut_text("⌘C")).clicked() {
                actions.push(Action::Command("terminal.copy", Value::Null));
                ui.close();
            }
            if ui.add(egui::Button::new("Paste").shortcut_text("⌘V")).clicked() {
                actions.push(Action::Command("terminal.paste", Value::Null));
                ui.close();
            }
            if ui.add(egui::Button::new("Select All").shortcut_text("⌘A")).clicked() {
                actions.push(Action::Command("terminal.selectAll", Value::Null));
                ui.close();
            }
            if ui.add(egui::Button::new("Clear").shortcut_text("⌘K")).clicked() {
                actions.push(Action::Command("terminal.clear", Value::Null));
                ui.close();
            }
        });
    }

    // Scrollbar.
    let screen = &tab.screen;
    let total = (screen.history + screen.rows).max(1) as f32;
    let thumb_h = (track.height() * screen.rows as f32 / total).clamp(18.0_f32.min(track.height()), track.height());
    let movable = (track.height() - thumb_h).max(0.0);
    if screen.history > 0 {
        if sb_response.drag_started() || sb_response.clicked() {
            st.terminal.scrollbar_drag = true;
        }
        if st.terminal.scrollbar_drag
            && let Some(p) = interact_pos
        {
            let frac =
                if movable > 0.0 { ((p.y - track.top() - thumb_h / 2.0) / movable).clamp(0.0, 1.0) } else { 1.0 };
            let offset = screen.history - (frac * screen.history as f32).round() as usize;
            if offset != screen.display_offset {
                tab.term.scroll_to(offset);
                tab.refresh();
            }
        }
    }
    if !down {
        st.terminal.scrollbar_drag = false;
    }

    // Paint.
    let screen = &tab.screen;
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme.editor_bg);
    paint_cells(ui, &painter, screen, &m, theme);
    if let Some((row, c0, c1, _)) = &link {
        let r = m.cell_rect(*row, *c0, c1.saturating_sub(*c0));
        painter.line_segment(
            [pos2(r.left(), r.bottom() - 2.0), pos2(r.right(), r.bottom() - 2.0)],
            Stroke::new(1.0, theme.link),
        );
    }
    let blink_on =
        st.terminal.blink_start.is_none_or(|t| (t.elapsed().as_millis() / BLINK.as_millis()).is_multiple_of(2));
    if let Some(c) = screen.cursor {
        let wide = screen.cell(c.row, c.col).is_some_and(|cell| cell.flags.has(CellFlags::WIDE));
        let r = m.cell_rect(c.row, c.col, if wide { 2 } else { 1 });
        if !focused {
            painter.rect_stroke(r.shrink(0.5), 0.0, Stroke::new(1.0, theme.caret), egui::StrokeKind::Inside);
        } else if !c.blinking || blink_on {
            match c.shape {
                CursorShape::Block => {
                    painter.rect_filled(r, 0.0, theme.caret);
                    if let Some(cell) = screen.cell(c.row, c.col).filter(|cell| cell.ch != ' ') {
                        let galley =
                            ui.fonts_mut(|f| f.layout_no_wrap(cell.ch.to_string(), m.font.clone(), theme.editor_bg));
                        painter.galley(pos2(r.left(), r.top() + (m.ch - m.glyph_h) / 2.0), galley, theme.editor_bg);
                    }
                }
                CursorShape::Beam => {
                    painter.rect_filled(Rect::from_min_size(r.min, vec2(2.0, r.height())), 0.0, theme.caret);
                }
                CursorShape::Underline => {
                    painter.rect_filled(Rect::from_min_max(pos2(r.left(), r.bottom() - 2.0), r.max), 0.0, theme.caret);
                }
                CursorShape::HollowBlock => {
                    painter.rect_stroke(r.shrink(0.5), 0.0, Stroke::new(1.0, theme.caret), egui::StrokeKind::Inside);
                }
            }
        }
        if focused && c.blinking {
            ui.ctx().request_repaint_after(BLINK);
        }
    }
    if screen.history > 0 {
        let top = track.top() + movable * (1.0 - screen.display_offset as f32 / screen.history as f32);
        let thumb = Rect::from_min_size(pos2(track.left() + 2.0, top), vec2(SCROLLBAR_W - 4.0, thumb_h));
        let color =
            if sb_response.hovered() || st.terminal.scrollbar_drag { theme.text_muted } else { theme.separator };
        painter.rect_filled(thumb, 3.0, color);
    }
    if focused {
        let cursor_rect = screen.cursor.map(|c| m.cell_rect(c.row, c.col, 1)).unwrap_or(text);
        ui.output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Terminal,
                rect,
                cursor_rect,
                should_interrupt_composition: false,
            })
        });
        ui.memory_mut(|mem| {
            mem.set_focus_lock_filter(
                id,
                egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
    }
    ui.advance_cursor_after_rect(rect);

    if let Some(desc) = exited {
        let tab_id = tab.id;
        let command = tab.command.clone();
        let cwd = tab.cwd.clone();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Process finished: {desc}")).color(theme.text_muted));
            if ui.small_button("Restart").clicked() {
                actions.push(Action::Command("terminal.close", json!({ "tab": tab_id })));
                let mut params = json!({ "cwd": cwd });
                if let Some(c) = command {
                    params["command"] = json!(c);
                }
                actions.push(Action::Command("terminal.new", params));
            }
            if ui.small_button("Close").clicked() {
                actions.push(Action::Command("terminal.close", json!({ "tab": tab_id })));
            }
        });
    }
}

fn open_link(s: &Session, target: &LinkTarget) -> Action {
    match target {
        LinkTarget::Url(url) => Action::OpenUrl(url.clone()),
        LinkTarget::File { path, line, column } => {
            let resolved = codecraft_engine::remote::terminal_path(s, s.terminal.active, path);
            Action::Command(
                "navigate.openLocation",
                json!({
                    "path": resolved.unwrap_or_else(|| path.into()),
                    "line": line.map(|l| l.saturating_sub(1)),
                    "column": column.map(|c| c.saturating_sub(1)),
                }),
            )
        }
    }
}

/// The link under a cell: `(row, first col, end col, target)`. Files must exist.
fn link_at(s: &Session, tab: usize, row: usize, col: usize) -> Option<(usize, usize, usize, LinkTarget)> {
    let t = s.terminal.tabs.get(tab)?;
    let screen = &t.screen;
    if let Some(uri) = screen.hyperlink_at(row, col) {
        let same = |c: usize| screen.hyperlink_at(row, c) == Some(uri);
        let start = (0..=col).rev().take_while(|c| same(*c)).last().unwrap_or(col);
        let end = (col..screen.cols).take_while(|c| same(*c)).last().unwrap_or(col) + 1;
        return Some((row, start, end, LinkTarget::Url(uri.to_string())));
    }
    let (text, cols) = screen.row_text_with_cols(row);
    for link in term::find_links(&text) {
        let (Some(&c0), Some(&c_last)) = (cols.get(link.start), cols.get(link.end.saturating_sub(1))) else { continue };
        let wide = screen.cell(row, c_last).is_some_and(|c| c.flags.has(CellFlags::WIDE));
        let c1 = c_last + if wide { 2 } else { 1 };
        if col < c0 || col >= c1 {
            continue;
        }
        if let LinkTarget::File { path, .. } = &link.target
            && codecraft_engine::remote::terminal_path(s, tab, path).is_none()
        {
            return None;
        }
        return Some((row, c0, c1, link.target));
    }
    None
}

fn resolve(theme: &Theme, c: TermColor) -> Color32 {
    match c {
        TermColor::Fg => theme.text_editor,
        TermColor::Bg => theme.editor_bg,
        TermColor::Indexed(i) if i < 16 => {
            theme.terminal_ansi.get(usize::from(i)).copied().unwrap_or(theme.text_editor)
        }
        TermColor::Indexed(i) => {
            let (r, g, b) = term::xterm_rgb(i);
            Color32::from_rgb(r, g, b)
        }
        TermColor::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

/// Foreground and (non-default) background of a cell.
fn cell_colors(theme: &Theme, cell: &term::Cell, selected: bool) -> (Color32, Option<Color32>) {
    let f = cell.flags;
    let mut fg = resolve(theme, cell.fg);
    let mut bg = resolve(theme, cell.bg);
    let mut custom_bg = cell.bg != TermColor::Bg;
    if f.has(CellFlags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
        custom_bg = true;
    }
    if f.has(CellFlags::DIM) {
        fg = fg.lerp_to_gamma(bg, 0.4);
    }
    if f.has(CellFlags::HIDDEN) {
        fg = bg;
    }
    if selected {
        return (fg, Some(theme.selection));
    }
    (fg, custom_bg.then_some(bg))
}

#[derive(Clone, Copy, PartialEq)]
struct Style {
    fg: Color32,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

fn paint_cells(ui: &Ui, painter: &egui::Painter, screen: &Screen, m: &Metrics, theme: &Theme) {
    let dy = ((m.ch - m.glyph_h) / 2.0).max(0.0);
    let paint_text = |text: &str, col: usize, row: usize, style: Style, width_cells: usize| {
        let mut job = LayoutJob::single_section(
            text.to_string(),
            TextFormat {
                font_id: m.font.clone(),
                color: style.fg,
                italics: style.italic,
                underline: if style.underline { Stroke::new(1.0, style.fg) } else { Stroke::NONE },
                strikethrough: if style.strike { Stroke::new(1.0, style.fg) } else { Stroke::NONE },
                ..TextFormat::default()
            },
        );
        job.wrap.max_width = f32::INFINITY;
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        let cell = m.cell_rect(row, col, width_cells);
        // Glyphs that are not one cell wide (wide chars, fallback fonts) are centred in their cells.
        let x = if width_cells == 1 && text.chars().count() > 1 || text.is_ascii() {
            cell.left()
        } else {
            cell.center().x - galley.size().x / 2.0
        };
        let pos = pos2(x, cell.top() + dy);
        if style.bold {
            painter.galley(pos + vec2(0.6, 0.0), galley.clone(), style.fg);
        }
        painter.galley(pos, galley, style.fg);
    };
    for row in 0..screen.rows {
        let cells = screen.row(row);
        // Backgrounds, merged into runs.
        let mut col = 0;
        while col < cells.len() {
            let Some(cell) = cells.get(col) else { break };
            let (_, bg) = cell_colors(theme, cell, screen.is_selected(row, col));
            let mut end = col + 1;
            while let Some(next) = cells.get(end) {
                if cell_colors(theme, next, screen.is_selected(row, end)).1 != bg {
                    break;
                }
                end += 1;
            }
            if let Some(bg) = bg {
                painter.rect_filled(m.cell_rect(row, col, end - col), 0.0, bg);
            }
            col = end;
        }
        // Text: runs of ASCII cells with one style; everything else glyph by glyph.
        let mut run = String::new();
        let mut run_start = 0;
        let mut run_style: Option<Style> = None;
        for (col, cell) in cells.iter().enumerate() {
            let f = cell.flags;
            if f.has(CellFlags::SPACER) {
                continue;
            }
            let (fg, _) = cell_colors(theme, cell, screen.is_selected(row, col));
            let style = Style {
                fg,
                bold: f.has(CellFlags::BOLD),
                italic: f.has(CellFlags::ITALIC),
                underline: f.has(CellFlags::UNDERLINE)
                    || f.has(CellFlags::DOUBLE_UNDERLINE)
                    || f.has(CellFlags::UNDERCURL),
                strike: f.has(CellFlags::STRIKEOUT),
            };
            let combining = screen.combining_at(row, col);
            let plain = cell.ch.is_ascii() && !cell.ch.is_ascii_control() && combining.is_none();
            let blank = cell.ch == ' ' && !style.underline && !style.strike;
            if (!plain || blank || run_style != Some(style))
                && let Some(rs) = run_style.take()
            {
                paint_text(&run, run_start, row, rs, 1);
                run.clear();
            }
            if blank || cell.ch.is_control() {
                continue;
            }
            if plain {
                if run_style.is_none() {
                    run_start = col;
                    run_style = Some(style);
                }
                run.push(cell.ch);
            } else {
                let mut glyph = cell.ch.to_string();
                if let Some(extra) = combining {
                    glyph.push_str(extra);
                }
                paint_text(&glyph, col, row, style, if f.has(CellFlags::WIDE) { 2 } else { 1 });
            }
        }
        if let Some(rs) = run_style {
            paint_text(&run, run_start, row, rs, 1);
        }
    }
}

// ---------------------------------------------------------------- keyboard

/// The terminal key for an egui key (characters unshifted unless `shift`).
fn term_key(key: Key, shift: bool) -> Option<term::Key> {
    use term::Key as T;
    let ch = |lower: char, upper: char| Some(T::Char(if shift { upper } else { lower }));
    match key {
        Key::Enter => Some(T::Enter),
        Key::Tab => Some(T::Tab),
        Key::Backspace => Some(T::Backspace),
        Key::Escape => Some(T::Escape),
        Key::ArrowUp => Some(T::Up),
        Key::ArrowDown => Some(T::Down),
        Key::ArrowLeft => Some(T::Left),
        Key::ArrowRight => Some(T::Right),
        Key::Home => Some(T::Home),
        Key::End => Some(T::End),
        Key::PageUp => Some(T::PageUp),
        Key::PageDown => Some(T::PageDown),
        Key::Insert => Some(T::Insert),
        Key::Delete => Some(T::Delete),
        Key::Space => Some(T::Char(' ')),
        Key::Minus => ch('-', '_'),
        Key::Equals => ch('=', '+'),
        Key::OpenBracket => ch('[', '{'),
        Key::CloseBracket => ch(']', '}'),
        Key::Backslash => ch('\\', '|'),
        Key::Semicolon => ch(';', ':'),
        Key::Quote => ch('\'', '"'),
        Key::Comma => ch(',', '<'),
        Key::Period => ch('.', '>'),
        Key::Slash => ch('/', '?'),
        Key::Backtick => ch('`', '~'),
        _ => {
            let name = key.name();
            if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
                return (1..=12).contains(&n).then_some(T::F(n));
            }
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphabetic() => ch(c.to_ascii_lowercase(), c.to_ascii_uppercase()),
                (Some(c), None) if c.is_ascii_digit() => Some(T::Char(c)),
                _ => None,
            }
        }
    }
}

/// Cmd shortcuts inside the terminal (macOS Terminal's, plus RustRover's tab keys).
fn terminal_cmd_shortcut(key: Key, m: &Modifiers) -> Option<(&'static str, Value)> {
    if m.ctrl || m.alt {
        return None;
    }
    Some(match (key, m.shift) {
        (Key::C, false) => ("terminal.copy", Value::Null),
        (Key::K, false) => ("terminal.clear", Value::Null),
        (Key::A, false) => ("terminal.selectAll", Value::Null),
        (Key::T, false) => ("terminal.new", Value::Null),
        (Key::W, false) => ("terminal.close", Value::Null),
        (Key::CloseBracket, true) => ("terminal.next", Value::Null),
        (Key::OpenBracket, true) => ("terminal.previous", Value::Null),
        (Key::ArrowUp, false) => ("terminal.scroll", json!({ "lines": 1 })),
        (Key::ArrowDown, false) => ("terminal.scroll", json!({ "lines": -1 })),
        (Key::Home, false) => ("terminal.scroll", json!({ "to": "top" })),
        (Key::End, false) => ("terminal.scroll", json!({ "to": "bottom" })),
        _ => return None,
    })
}

/// Keyboard input while the terminal has focus: every key goes to the PTY (Ctrl-C/D/Z, arrows,
/// Tab, Escape, function keys, Option as Meta when enabled), except Cmd shortcuts: Cmd+C/V/K/A
/// and the tab keys act on the terminal, other Cmd shortcuts (Cmd+1…9, ⇧⌘A, …) and Alt+F12 run
/// their IDE commands. Text-editing commands (`edit.*`) never run from here.
pub fn handle_keys(ctx: &egui::Context, events: &[Event], s: &mut Session, st: &mut UiState) {
    if s.terminal.tabs.is_empty() {
        return;
    }
    let alt_meta = s.terminal.alt_is_meta;
    let mac = cfg!(target_os = "macos");
    // A `Text` event right after a key press repeats that key (Option-composed characters when
    // Option is Meta, Cmd shortcuts on some platforms): skip it.
    let mut skip_next = false;
    for ev in events {
        let skip = std::mem::take(&mut skip_next);
        match ev {
            Event::Key { key, pressed: true, modifiers: m, .. } => {
                st.terminal.blink_start = Some(Instant::now());
                let ide_key = (m.mac_cmd && mac) || (*key == Key::F12 && m.alt && !m.ctrl && !m.mac_cmd);
                if ide_key {
                    skip_next = true;
                    if let Some((id, params)) = m.mac_cmd.then(|| terminal_cmd_shortcut(*key, m)).flatten() {
                        ctx.input_mut(|i| i.consume_key(*m, *key));
                        run_command(s, st, id, &params);
                        continue;
                    }
                    let found = st
                        .bindings
                        .iter()
                        .find(|b| {
                            b.context == Context::Global && b.shortcut.matches(*key, m) && !b.id.starts_with("edit.")
                        })
                        .map(|b| b.id);
                    if let Some(id) = found {
                        ctx.input_mut(|i| i.consume_key(*m, *key));
                        run_command(s, st, id, &Value::Null);
                    }
                    continue;
                }
                // Shift+PageUp/PageDown scroll the scrollback, like most terminals.
                if m.shift && !m.ctrl && !m.alt && matches!(key, Key::PageUp | Key::PageDown) {
                    let pages = if *key == Key::PageUp { 1 } else { -1 };
                    run_command(s, st, "terminal.scroll", &json!({ "pages": pages }));
                    continue;
                }
                let Some(tkey) = term_key(*key, m.shift) else { continue };
                let is_char = matches!(tkey, term::Key::Char(_));
                let meta = m.alt && (alt_meta || !is_char);
                // Plain characters (and Option-composed ones) arrive as `Text` events instead.
                if is_char && !m.ctrl && !meta {
                    continue;
                }
                let mods = Mods { ctrl: m.ctrl, alt: meta, shift: m.shift };
                if let Some(t) = s.terminal.active_tab()
                    && t.alive()
                {
                    t.term.send_key(tkey, mods);
                }
                skip_next = is_char;
            }
            Event::Text(text) => {
                if skip {
                    continue;
                }
                run_command(s, st, "terminal.sendText", &json!({ "text": text }));
            }
            Event::Ime(egui::ImeEvent::Commit(text)) if !text.is_empty() => {
                run_command(s, st, "terminal.sendText", &json!({ "text": text }));
            }
            Event::Paste(text) => run_command(s, st, "terminal.paste", &json!({ "text": text })),
            Event::Copy | Event::Cut => {
                let has_selection = s.terminal.active_tab().is_some_and(|t| t.screen.selection.is_some());
                if mac || has_selection {
                    run_command(s, st, "terminal.copy", &Value::Null);
                } else {
                    // Elsewhere Ctrl+C/Ctrl+X arrive as Copy/Cut: send them to the program.
                    let key = if matches!(ev, Event::Copy) { "Ctrl+C" } else { "Ctrl+X" };
                    run_command(s, st, "terminal.sendKey", &json!({ "key": key }));
                }
            }
            _ => {}
        }
    }
}
