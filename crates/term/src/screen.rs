//! A snapshot of the visible terminal grid (what the UI paints), detached from the emulator.

use std::sync::Arc;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{self, NamedColor};
use serde::Serialize;

/// A cell colour before theming: the UI maps `Fg`/`Bg` and indices 0–15 to theme tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Color {
    /// The default foreground.
    Fg,
    /// The default background.
    Bg,
    /// xterm palette index (0–15 themed, 16–255 the standard cube and grey ramp).
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Cell attributes (a small bit set).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
pub struct CellFlags(pub u16);

impl CellFlags {
    pub const BOLD: u16 = 1;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 3;
    pub const UNDERCURL: u16 = 1 << 4;
    pub const DIM: u16 = 1 << 5;
    pub const INVERSE: u16 = 1 << 6;
    pub const HIDDEN: u16 = 1 << 7;
    pub const STRIKEOUT: u16 = 1 << 8;
    /// The first cell of a double-width character.
    pub const WIDE: u16 = 1 << 9;
    /// The second cell of a double-width character (paint nothing).
    pub const SPACER: u16 = 1 << 10;
    /// Part of an OSC 8 hyperlink.
    pub const HYPERLINK: u16 = 1 << 11;

    pub fn has(self, flag: u16) -> bool {
        self.0 & flag != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', fg: Color::Fg, bg: Color::Bg, flags: CellFlags::default() }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Beam,
    HollowBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ScreenCursor {
    pub row: usize,
    pub col: usize,
    pub shape: CursorShape,
    pub blinking: bool,
}

/// Terminal modes the UI needs for input encoding and scrolling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Modes {
    pub app_cursor: bool,
    pub bracketed_paste: bool,
    pub alt_screen: bool,
    /// Mouse tracking: clicks (1000), drags (1002), any motion (1003).
    pub mouse_click: bool,
    pub mouse_drag: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    /// Wheel → arrow keys on the alternate screen (1007).
    pub alternate_scroll: bool,
    pub focus_reporting: bool,
}

impl Modes {
    pub fn mouse_reporting(&self) -> bool {
        self.mouse_click || self.mouse_drag || self.mouse_motion
    }
}

/// A selection in viewport rows (may extend above or below the viewport).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ScreenSelection {
    pub start: (i64, usize),
    pub end: (i64, usize),
    pub block: bool,
}

/// The visible grid at one moment.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Screen {
    pub cols: usize,
    pub rows: usize,
    /// `rows × cols` cells, row-major.
    pub cells: Vec<Cell>,
    /// Zero-width characters (combining marks, emoji modifiers) appended to a cell, by index.
    pub combining: Vec<(usize, String)>,
    /// OSC 8 hyperlink targets by cell index (cells flagged `HYPERLINK`).
    pub hyperlinks: Vec<(usize, Arc<str>)>,
    /// Rows that continue on the next row (soft wrap).
    pub wrapped: Vec<bool>,
    /// `None` when hidden or scrolled out of view.
    pub cursor: Option<ScreenCursor>,
    /// Lines scrolled back from the bottom (0 = following output).
    pub display_offset: usize,
    /// Lines of scrollback above the screen.
    pub history: usize,
    pub selection: Option<ScreenSelection>,
    pub modes: Modes,
}

impl Screen {
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        if col >= self.cols {
            return None;
        }
        self.cells.get(row.checked_mul(self.cols)?.checked_add(col)?)
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        let start = row.saturating_mul(self.cols).min(self.cells.len());
        let end = start.saturating_add(self.cols).min(self.cells.len());
        self.cells.get(start..end).unwrap_or(&[])
    }

    /// Combining characters appended to a cell.
    pub fn combining_at(&self, row: usize, col: usize) -> Option<&str> {
        let idx = row.checked_mul(self.cols)?.checked_add(col)?;
        let i = self.combining.binary_search_by_key(&idx, |(i, _)| *i).ok()?;
        self.combining.get(i).map(|(_, s)| s.as_str())
    }

    /// The OSC 8 hyperlink under a cell.
    pub fn hyperlink_at(&self, row: usize, col: usize) -> Option<&str> {
        let idx = row.checked_mul(self.cols)?.checked_add(col)?;
        let i = self.hyperlinks.binary_search_by_key(&idx, |(i, _)| *i).ok()?;
        self.hyperlinks.get(i).map(|(_, s)| &**s)
    }

    /// The row's text and, for each char of it, the column it starts at (wide-char spacers are
    /// skipped; combining marks map to their base cell). Trailing blanks are trimmed.
    pub fn row_text_with_cols(&self, row: usize) -> (String, Vec<usize>) {
        let mut text = String::new();
        let mut cols = Vec::new();
        for (col, cell) in self.row(row).iter().enumerate() {
            if cell.flags.has(CellFlags::SPACER) {
                continue;
            }
            text.push(cell.ch);
            cols.push(col);
            if let Some(extra) = self.combining_at(row, col) {
                for c in extra.chars() {
                    text.push(c);
                    cols.push(col);
                }
            }
        }
        let trimmed = text.trim_end().chars().count();
        cols.truncate(trimmed);
        (text.chars().take(trimmed).collect(), cols)
    }

    pub fn row_text(&self, row: usize) -> String {
        self.row_text_with_cols(row).0
    }

    /// All visible rows, joined with `\n` (soft-wrapped rows are not joined).
    pub fn text(&self) -> String {
        (0..self.rows).map(|r| self.row_text(r)).collect::<Vec<_>>().join("\n")
    }

    /// Whether a viewport cell is selected.
    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(sel) = self.selection else { return false };
        let row = row as i64;
        if row < sel.start.0 || row > sel.end.0 {
            return false;
        }
        if sel.block {
            return col >= sel.start.1.min(sel.end.1) && col <= sel.start.1.max(sel.end.1);
        }
        (row > sel.start.0 || col >= sel.start.1) && (row < sel.end.0 || col <= sel.end.1)
    }
}

fn convert_color(color: ansi::Color, colors: &alacritty_terminal::term::color::Colors, flags: &mut u16) -> Color {
    let indexed = |i: usize, fallback: Color| -> Color {
        match colors[i] {
            Some(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
            None => fallback,
        }
    };
    match color {
        ansi::Color::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        ansi::Color::Indexed(i) => indexed(usize::from(i), Color::Indexed(i)),
        ansi::Color::Named(n) => {
            let idx = n as usize;
            match n {
                NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::Cursor => indexed(idx, Color::Fg),
                NamedColor::DimForeground => {
                    *flags |= CellFlags::DIM;
                    indexed(NamedColor::Foreground as usize, Color::Fg)
                }
                NamedColor::Background => indexed(idx, Color::Bg),
                _ if idx < 16 => indexed(idx, Color::Indexed(idx as u8)),
                _ => {
                    // DimBlack..DimWhite follow Cursor.
                    let base = idx.saturating_sub(NamedColor::DimBlack as usize).min(7);
                    *flags |= CellFlags::DIM;
                    indexed(base, Color::Indexed(base as u8))
                }
            }
        }
    }
}

fn convert_flags(f: Flags) -> u16 {
    let mut out = 0;
    for (from, to) in [
        (Flags::BOLD, CellFlags::BOLD),
        (Flags::ITALIC, CellFlags::ITALIC),
        (Flags::UNDERLINE, CellFlags::UNDERLINE),
        (Flags::DOTTED_UNDERLINE, CellFlags::UNDERLINE),
        (Flags::DASHED_UNDERLINE, CellFlags::UNDERLINE),
        (Flags::DOUBLE_UNDERLINE, CellFlags::DOUBLE_UNDERLINE),
        (Flags::UNDERCURL, CellFlags::UNDERCURL),
        (Flags::DIM, CellFlags::DIM),
        (Flags::INVERSE, CellFlags::INVERSE),
        (Flags::HIDDEN, CellFlags::HIDDEN),
        (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
        (Flags::WIDE_CHAR, CellFlags::WIDE),
        (Flags::WIDE_CHAR_SPACER, CellFlags::SPACER),
        (Flags::LEADING_WIDE_CHAR_SPACER, CellFlags::SPACER),
    ] {
        if f.intersects(from) {
            out |= to;
        }
    }
    out
}

fn convert_modes(m: TermMode) -> Modes {
    Modes {
        app_cursor: m.contains(TermMode::APP_CURSOR),
        bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
        alt_screen: m.contains(TermMode::ALT_SCREEN),
        mouse_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
        mouse_drag: m.contains(TermMode::MOUSE_DRAG),
        mouse_motion: m.contains(TermMode::MOUSE_MOTION),
        sgr_mouse: m.contains(TermMode::SGR_MOUSE),
        alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
        focus_reporting: m.contains(TermMode::FOCUS_IN_OUT),
    }
}

/// Copy the visible part of the emulator's grid.
pub(crate) fn snapshot<T: EventListener>(term: &Term<T>) -> Screen {
    let content = term.renderable_content();
    let grid = term.grid();
    let cols = alacritty_terminal::grid::Dimensions::columns(grid);
    let rows = alacritty_terminal::grid::Dimensions::screen_lines(grid);
    let display_offset = content.display_offset;
    let mut screen = Screen {
        cols,
        rows,
        cells: vec![Cell::default(); cols.saturating_mul(rows)],
        wrapped: vec![false; rows],
        display_offset,
        history: alacritty_terminal::grid::Dimensions::history_size(grid),
        modes: convert_modes(content.mode),
        ..Screen::default()
    };
    let offset = display_offset as i64;
    for indexed in content.display_iter {
        let row = i64::from(indexed.point.line.0) + offset;
        let col = indexed.point.column.0;
        let (Ok(row), true) = (usize::try_from(row), col < cols) else { continue };
        if row >= rows {
            continue;
        }
        let idx = row * cols + col;
        let src = indexed.cell;
        let mut flags = convert_flags(src.flags);
        let fg = convert_color(src.fg, content.colors, &mut flags);
        let mut ignored = 0;
        let bg = convert_color(src.bg, content.colors, &mut ignored);
        if let Some(link) = src.hyperlink() {
            flags |= CellFlags::HYPERLINK;
            let uri: Arc<str> = match screen.hyperlinks.last() {
                Some((_, last)) if **last == *link.uri() => last.clone(),
                _ => Arc::from(link.uri()),
            };
            screen.hyperlinks.push((idx, uri));
        }
        if let Some(zw) = src.zerowidth() {
            screen.combining.push((idx, zw.iter().collect()));
        }
        if src.flags.contains(Flags::WRAPLINE)
            && let Some(w) = screen.wrapped.get_mut(row)
        {
            *w = true;
        }
        if let Some(cell) = screen.cells.get_mut(idx) {
            *cell = Cell { ch: src.c, fg, bg, flags: CellFlags(flags) };
        }
    }
    let cursor = content.cursor;
    let shape = match cursor.shape {
        ansi::CursorShape::Hidden => None,
        ansi::CursorShape::Block => Some(CursorShape::Block),
        ansi::CursorShape::Underline => Some(CursorShape::Underline),
        ansi::CursorShape::Beam => Some(CursorShape::Beam),
        ansi::CursorShape::HollowBlock => Some(CursorShape::HollowBlock),
    };
    let crow = i64::from(cursor.point.line.0) + offset;
    screen.cursor = match (shape, usize::try_from(crow)) {
        (Some(shape), Ok(row)) if row < rows => Some(ScreenCursor {
            row,
            col: cursor.point.column.0.min(cols.saturating_sub(1)),
            shape,
            blinking: term.cursor_style().blinking,
        }),
        _ => None,
    };
    screen.selection = content.selection.map(|r| ScreenSelection {
        start: (i64::from(r.start.line.0) + offset, r.start.column.0),
        end: (i64::from(r.end.line.0) + offset, r.end.column.0),
        block: r.is_block,
    });
    screen
}

/// The RGB of an xterm 256-colour index (16–255; 0–15 are themed by the UI).
pub fn xterm_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        232..=255 => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
        // The standard xterm defaults for 0–15 (used for OSC colour queries without a theme).
        _ => {
            const BASE: [(u8, u8, u8); 16] = [
                (0, 0, 0),
                (205, 0, 0),
                (0, 205, 0),
                (205, 205, 0),
                (0, 0, 238),
                (205, 0, 205),
                (0, 205, 205),
                (229, 229, 229),
                (127, 127, 127),
                (255, 0, 0),
                (0, 255, 0),
                (255, 255, 0),
                (92, 92, 255),
                (255, 0, 255),
                (0, 255, 255),
                (255, 255, 255),
            ];
            BASE.get(usize::from(i)).copied().unwrap_or((0, 0, 0))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cube_and_greys() {
        assert_eq!(xterm_rgb(16), (0, 0, 0));
        assert_eq!(xterm_rgb(196), (255, 0, 0));
        assert_eq!(xterm_rgb(231), (255, 255, 255));
        assert_eq!(xterm_rgb(232), (8, 8, 8));
        assert_eq!(xterm_rgb(255), (238, 238, 238));
        assert_eq!(xterm_rgb(1), (205, 0, 0));
    }

    #[test]
    fn selection_membership() {
        let s = Screen {
            cols: 10,
            rows: 3,
            selection: Some(ScreenSelection { start: (0, 5), end: (1, 2), block: false }),
            ..Screen::default()
        };
        assert!(!s.is_selected(0, 4));
        assert!(s.is_selected(0, 5));
        assert!(s.is_selected(0, 9));
        assert!(s.is_selected(1, 0));
        assert!(s.is_selected(1, 2));
        assert!(!s.is_selected(1, 3));
        assert!(!s.is_selected(2, 0));
    }
}
