//! The code editor widget: custom-painted, visible rows only. All edits and caret changes go
//! through engine commands; this module only paints and turns pointer input into commands.

use std::collections::BTreeSet;
use std::sync::Arc;

use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Id, Pos2, Rect, Sense, Shape, Stroke, pos2, vec2};
use serde_json::json;

use codecraft_engine::Session;
use codecraft_syntax::{FoldRange, HighlightKind, Highlights};
use codecraft_text::Document;
use codecraft_workspace::{EditorId, Problem, Severity};

use crate::UiState;
use crate::theme::Theme;

const MAX_LAYOUT_CHARS: usize = 20_000;
const STRIPE_W: f32 = 12.0;
const RIGHT_MARGIN_COL: usize = 100;

pub fn editor_id(e: EditorId) -> Id {
    Id::new(("codecraft-editor", e.0))
}

/// Rows of the editor after folding: row → document line.
struct DisplayMap {
    rows: Option<Vec<usize>>,
    lines: usize,
}

impl DisplayMap {
    fn build(lines: usize, folds: &[FoldRange], folded: &BTreeSet<usize>) -> Self {
        if folded.is_empty() {
            return Self { rows: None, lines };
        }
        let mut rows = Vec::with_capacity(lines);
        let mut line = 0;
        while line < lines {
            rows.push(line);
            let fold_end = folded
                .contains(&line)
                .then(|| folds.iter().filter(|f| f.start_line == line).map(|f| f.end_line).max())
                .flatten();
            line = match fold_end {
                Some(end) if end > line => end + 1,
                _ => line + 1,
            };
        }
        Self { rows: Some(rows), lines }
    }

    fn len(&self) -> usize {
        self.rows.as_ref().map_or(self.lines, Vec::len).max(1)
    }

    fn line(&self, row: usize) -> Option<usize> {
        match &self.rows {
            Some(r) => r.get(row).copied(),
            None => (row < self.lines).then_some(row),
        }
    }

    fn row_of(&self, line: usize) -> usize {
        match &self.rows {
            Some(r) => match r.binary_search(&line) {
                Ok(i) => i,
                Err(i) => i.saturating_sub(1),
            },
            None => line,
        }
    }
}

/// A laid-out line and the mapping from char columns to galley chars (tabs expand).
struct LineLayout {
    galley: Arc<Galley>,
    /// `xcols[c]` = galley char index of char column `c`; `None` = identity.
    xcols: Option<Vec<usize>>,
    len: usize,
}

impl LineLayout {
    fn xcol(&self, col: usize) -> usize {
        let col = col.min(self.len);
        match &self.xcols {
            Some(v) => v.get(col).copied().unwrap_or(col),
            None => col,
        }
    }

    fn x_of(&self, col: usize) -> f32 {
        self.galley.pos_from_cursor(CCursor::new(self.xcol(col))).min.x
    }

    fn col_at_x(&self, x: f32) -> usize {
        let xc = self.galley.cursor_from_pos(vec2(x, self.galley.size().y * 0.5)).index;
        let xc: usize = xc.into();
        match &self.xcols {
            None => xc.min(self.len),
            Some(v) => match v.binary_search(&xc) {
                Ok(i) => i,
                Err(i) => i.min(self.len),
            },
        }
    }
}

fn layout_line(
    ui: &egui::Ui,
    doc: &Document,
    hl: &Highlights,
    line: usize,
    font: &FontId,
    theme: &Theme,
    tab: usize,
) -> LineLayout {
    let text = doc.line_text(line).unwrap_or_default();
    let chars: Vec<char> = text.chars().take(MAX_LAYOUT_CHARS).collect();
    let n = chars.len();
    let has_tabs = chars.contains(&'\t');
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut xcols: Vec<usize> = if has_tabs { Vec::with_capacity(n + 1) } else { Vec::new() };
    let mut x = 0usize;
    let mut push = |job: &mut LayoutJob, from: usize, to: usize, color: Color32| {
        if from >= to {
            return;
        }
        let mut seg = String::new();
        for c in chars.get(from..to).unwrap_or(&[]) {
            if has_tabs {
                xcols.push(x);
            }
            if *c == '\t' {
                let w = tab - (x % tab.max(1));
                seg.extend(std::iter::repeat_n(' ', w));
                x += w;
            } else {
                seg.push(*c);
                x += 1;
            }
        }
        job.append(&seg, 0.0, TextFormat { font_id: font.clone(), color, ..TextFormat::default() });
    };
    let mut col = 0usize;
    for span in hl.line(line) {
        let (s, e) = (span.start.min(n), span.end.min(n));
        if s < col || s >= e {
            continue;
        }
        push(&mut job, col, s, theme.text_editor);
        push(&mut job, s, e, theme.syntax(span.kind));
        col = e;
    }
    push(&mut job, col, n, theme.text_editor);
    if has_tabs {
        xcols.push(x);
    }
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    LineLayout { galley, xcols: has_tabs.then_some(xcols), len: n }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whole-word occurrences of `word` in `line` (char column ranges).
fn word_occurrences(line: &str, word: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let w: Vec<char> = word.chars().collect();
    let mut out = Vec::new();
    if w.is_empty() || chars.len() < w.len() {
        return out;
    }
    let mut i = 0;
    while i + w.len() <= chars.len() {
        if chars.get(i..i + w.len()) == Some(w.as_slice())
            && !(i > 0 && chars.get(i - 1).copied().is_some_and(is_ident))
            && !chars.get(i + w.len()).copied().is_some_and(is_ident)
        {
            out.push((i, i + w.len()));
            i += w.len();
        } else {
            i += 1;
        }
    }
    out
}

fn wavy(painter: &egui::Painter, x1: f32, x2: f32, y: f32, color: Color32) {
    let mut pts = Vec::new();
    let mut x = x1;
    let mut up = true;
    while x < x2 {
        pts.push(pos2(x, if up { y - 1.5 } else { y + 1.0 }));
        x += 2.5;
        up = !up;
    }
    pts.push(pos2(x2, y));
    if pts.len() >= 2 {
        painter.add(Shape::line(pts, Stroke::new(1.0, color)));
    }
}

fn severity_color(theme: &Theme, s: Severity) -> Color32 {
    match s {
        Severity::Error => theme.error,
        Severity::Warning => theme.warning,
        Severity::Info | Severity::Hint => theme.weak,
    }
}

/// Lines with a gutter run icon: `fn main` and `#[test]` functions, plus `mod tests`.
fn run_lines(doc: &Document, lang: codecraft_syntax::Language) -> Vec<usize> {
    if lang != codecraft_syntax::Language::Rust {
        return Vec::new();
    }
    let text = doc.text();
    let outline = codecraft_syntax::outline(lang, &text);
    let mut out = Vec::new();
    for item in &outline {
        let is_main = item.kind == codecraft_syntax::SymbolKind::Function && item.name == "main" && item.depth == 0;
        let is_test_mod = item.kind == codecraft_syntax::SymbolKind::Module && item.name == "tests";
        let is_test = item.kind == codecraft_syntax::SymbolKind::Function
            && (item.line.saturating_sub(4)..item.line).any(|l| {
                doc.line_text(l).is_some_and(|t| {
                    let t = t.trim_start();
                    t.starts_with("#[test]") || t.starts_with("#[tokio::test") || t.contains("::test]")
                })
            });
        if is_main || is_test || is_test_mod {
            out.push(item.line);
        }
    }
    out
}

/// What the debugger shows in one file: breakpoints, the execution line, inline values and (while
/// suspended) the current frame's locals for value hovers.
#[derive(Default)]
struct DebugMarks {
    breakpoints: Vec<codecraft_engine::debug::Breakpoint>,
    execution: Option<usize>,
    inline: Vec<(usize, String)>,
    locals: Vec<(String, String)>,
}

impl DebugMarks {
    fn collect(s: &Session, path: &std::path::Path) -> Self {
        let debug = &s.debug;
        let breakpoints = debug.breakpoints_in(path).cloned().collect();
        let Some(session) = debug.session.as_ref().filter(|x| x.is_stopped()) else {
            return Self { breakpoints, ..Self::default() };
        };
        let execution = session.execution.as_ref().filter(|(p, _)| p == path).map(|(_, line)| *line);
        let inline =
            session.inline_values.iter().filter(|v| v.path == path).map(|v| (v.line, v.text.clone())).collect();
        let locals = match (execution, session.scopes.first()) {
            (Some(_), Some(scope)) => session
                .variables
                .get(&scope.reference)
                .map(|vars| vars.iter().map(|v| (v.name.clone(), v.value.clone())).collect())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        Self { breakpoints, execution, inline, locals }
    }
}

fn breakpoint_tooltip(bp: &codecraft_engine::debug::Breakpoint) -> String {
    let mut text = String::from(if bp.temporary { "Temporary line breakpoint" } else { "Line breakpoint" });
    if !bp.enabled {
        text.push_str(" (disabled)");
    }
    if let Some(c) = &bp.condition {
        text.push_str(&format!("\nCondition: {c}"));
    }
    if let Some(h) = &bp.hit_condition {
        text.push_str(&format!("\nHit count: {h}"));
    }
    if let Some(m) = &bp.log_message {
        text.push_str(&format!("\nLog: {m}"));
    }
    match (bp.verified, &bp.message) {
        (Some(false), Some(m)) => text.push_str(&format!("\nNot set: {m}")),
        (Some(false), None) => text.push_str("\nThe debugger has not set it (yet)."),
        _ => {}
    }
    text.push_str("\nClick to remove · right-click to edit");
    text
}

/// Paint the editor and handle pointer input. Returns the caret's screen position.
pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, eid: EditorId) {
    let theme = st.theme.clone();
    let settings = s.ws.settings.clone();
    let Some(editor) = s.ws.editor(eid).cloned() else { return };
    let path = s.ws.doc(editor.doc).and_then(|d| d.path.clone());
    let problems: Vec<Problem> =
        path.as_deref().map(|p| s.ws.problems_for(p).into_iter().cloned().collect()).unwrap_or_default();
    let (find_matches, find_current) = s.find_bar.as_ref().map(|b| (b.matches.clone(), b.current)).unwrap_or_default();
    let marks = path.as_deref().map(|p| DebugMarks::collect(s, p)).unwrap_or_default();
    let Some(od) = s.ws.doc_mut(editor.doc) else { return };
    od.refresh_caches();
    let od = &*od;
    let doc = &od.doc;
    let hl = od.cached_highlights();
    let folds = od.cached_folds();
    let tab = settings.tab_size.max(1);

    let font = FontId::monospace(settings.editor_font_size);
    let inline_font = FontId::proportional((settings.editor_font_size - 1.0).max(8.0));
    let (char_w, row_h) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
    let line_h = (settings.editor_font_size * settings.line_spacing).max(row_h).round();
    let rect = ui.available_rect_before_wrap();
    let id = editor_id(eid);
    let response = ui.interact(rect, id, Sense::click_and_drag());
    ui.advance_cursor_after_rect(rect);

    let map = DisplayMap::build(doc.len_lines(), folds, &editor.folded);
    let digits = doc.len_lines().max(1).ilog10() as f32 + 1.0;
    // Breakpoints sit over the line numbers (RustRover's New UI), so keep room for them.
    let numbers_w = if settings.show_line_numbers { (digits.max(2.0) + 1.0) * char_w } else { 16.0 };
    let gutter_w = numbers_w + 18.0 + 14.0 + 6.0;
    let text_rect =
        Rect::from_min_max(pos2(rect.left() + gutter_w, rect.top()), pos2(rect.right() - STRIPE_W, rect.bottom()));
    let visible_rows = ((rect.height() / line_h).floor() as usize).max(1);
    s_set_page(st, visible_rows);

    // ------------------------------------------------------------ scrolling
    let mut scroll = editor.scroll_line;
    let mut scroll_x = editor.scroll_x;
    if response.hovered() {
        let delta = ui.input(|i| i.smooth_scroll_delta);
        scroll -= delta.y / line_h;
        scroll_x -= delta.x;
    }
    let primary = editor.selections.primary();
    let caret_line = doc.char_to_line(primary.head);
    let caret_row = map.row_of(caret_line);
    let mut reveal = editor.reveal_caret;
    if reveal {
        let margin = 3.0f32.min(visible_rows as f32 / 3.0);
        if (caret_row as f32) < scroll + margin {
            scroll = caret_row as f32 - margin;
        } else if caret_row as f32 > scroll + visible_rows as f32 - 1.0 - margin {
            scroll = caret_row as f32 - visible_rows as f32 + 1.0 + margin;
        }
    }
    let max_scroll = (map.len() as f32 - 1.0).max(0.0);
    scroll = scroll.clamp(0.0, max_scroll);
    scroll_x = scroll_x.max(0.0);

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, theme.editor_bg);
    let text_painter = ui.painter_at(text_rect);
    let text_x0 = text_rect.left() + 4.0;

    // Caret line layout for horizontal reveal.
    if reveal {
        let ll = layout_line(ui, doc, hl, caret_line, &font, &theme, tab);
        let cx = ll.x_of(primary.head.saturating_sub(doc.line_start(caret_line)));
        let view_w = text_rect.width() - 8.0;
        if cx - scroll_x > view_w - 4.0 * char_w {
            scroll_x = cx - view_w + 8.0 * char_w;
        } else if cx < scroll_x + 2.0 * char_w {
            scroll_x = (cx - 8.0 * char_w).max(0.0);
        }
        reveal = false;
    }

    let first_row = scroll.floor() as usize;
    let frac = scroll - scroll.floor();
    let rows_to_draw = visible_rows + 2;
    let row_y = |row: usize| rect.top() + (row as f32 - first_row as f32 - frac) * line_h;

    // Identifier under caret for occurrence highlighting.
    let occurrence_word = if primary.is_empty() && editor.selections.len() == 1 {
        let r = codecraft_text::movement::word_range_at(doc, primary.head);
        let col = r.start.saturating_sub(doc.line_start(caret_line));
        // Only identifiers (not keywords, literals or comments), like RustRover.
        let kind = hl.line(caret_line).iter().find(|sp| sp.start <= col && col < sp.end).map(|sp| sp.kind);
        let identifier = matches!(
            kind,
            None | Some(
                HighlightKind::Ident
                    | HighlightKind::Function
                    | HighlightKind::Type
                    | HighlightKind::Field
                    | HighlightKind::Constant
                    | HighlightKind::Macro
                    | HighlightKind::Lifetime
            )
        );
        let w = doc.slice(r);
        (identifier && !w.is_empty() && w.chars().all(is_ident) && !w.chars().all(|c| c.is_ascii_digit())).then_some(w)
    } else {
        None
    };
    // Matching bracket (cached per revision + caret).
    let brackets = {
        let key = (editor.doc, doc.revision(), primary.head);
        if st.bracket_cache.as_ref().map(|(k, _)| *k) != Some(key) {
            let text = doc.text();
            let at = codecraft_syntax::matching_bracket(od.language, &text, primary.head);
            let own = at.and_then(|m| {
                [primary.head, primary.head.saturating_sub(1)].into_iter().find(|o| {
                    codecraft_syntax::matching_bracket(od.language, &text, *o) == Some(m)
                        && doc.char_at(*o).is_some_and(|c| "()[]{}".contains(c))
                })
            });
            st.bracket_cache = Some((key, own.zip(at)));
        }
        st.bracket_cache.as_ref().and_then(|(_, b)| *b)
    };
    let run_lines = {
        let key = (editor.doc, doc.revision());
        if st.run_lines_cache.as_ref().map(|(k, _)| *k) != Some(key) {
            st.run_lines_cache = Some((key, run_lines(doc, od.language)));
        }
        st.run_lines_cache.as_ref().map(|(_, v)| v.clone()).unwrap_or_default()
    };
    let fold_starts: Vec<usize> = folds.iter().filter(|f| f.end_line > f.start_line).map(|f| f.start_line).collect();

    let mut caret_screen = None;
    let focused = ui.memory(|m| m.has_focus(id));
    let blink_on = !focused || (ui.input(|i| i.time) * 2.0 % 2.0) < 1.3;
    let mut indent_prev: Option<usize> = None;
    let mut layouts: Vec<(usize, f32, LineLayout)> = Vec::with_capacity(rows_to_draw);

    for row in first_row..first_row + rows_to_draw {
        let Some(line) = map.line(row) else { break };
        let y = row_y(row);
        if y > rect.bottom() {
            break;
        }
        let ll = layout_line(ui, doc, hl, line, &font, &theme, tab);
        let line_start = doc.line_start(line);
        let line_len = doc.line_len(line);
        let text_y = y + (line_h - row_h) * 0.5;
        let line_rect = Rect::from_min_max(pos2(rect.left(), y), pos2(rect.right(), y + line_h));

        if line == caret_line && primary.is_empty() {
            painter.rect_filled(line_rect, CornerRadius::ZERO, theme.caret_row);
        }
        if marks.execution == Some(line) {
            painter.rect_filled(line_rect, CornerRadius::ZERO, theme.execution_line);
        }
        // Indent guides.
        let indent_cols = {
            let text = doc.line_text(line).unwrap_or_default();
            if text.trim().is_empty() {
                indent_prev.unwrap_or(0)
            } else {
                let w: usize =
                    text.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { tab } else { 1 }).sum();
                indent_prev = Some(w);
                w
            }
        };
        let mut level = tab;
        while level < indent_cols + 1 && level <= indent_cols {
            let gx = text_x0 - scroll_x + level as f32 * char_w - tab as f32 * char_w;
            if gx > text_rect.left() {
                text_painter.vline(gx, y..=y + line_h, Stroke::new(1.0, theme.indent_guide));
            }
            level += tab;
        }
        // Occurrences of the identifier under the caret.
        if let Some(w) = &occurrence_word {
            let text = doc.line_text(line).unwrap_or_default();
            for (a, b) in word_occurrences(&text, w) {
                let r = Rect::from_min_max(
                    pos2(text_x0 - scroll_x + ll.x_of(a), y + 1.0),
                    pos2(text_x0 - scroll_x + ll.x_of(b), y + line_h - 1.0),
                );
                text_painter.rect_filled(r, CornerRadius::same(2), theme.occurrence);
            }
        }
        // Find matches.
        let line_end_full = line_start + line_len;
        for (i, m) in find_matches.iter().enumerate() {
            if m.end < line_start || m.start > line_end_full {
                continue;
            }
            let a = m.start.max(line_start) - line_start;
            let b = m.end.min(line_end_full) - line_start;
            let color = if Some(i) == find_current { theme.find_match_current } else { theme.find_match };
            let r = Rect::from_min_max(
                pos2(text_x0 - scroll_x + ll.x_of(a), y + 1.0),
                pos2(text_x0 - scroll_x + ll.x_of(b).max(ll.x_of(a) + 2.0), y + line_h - 1.0),
            );
            text_painter.rect_filled(r, CornerRadius::same(2), color);
        }
        // Selections.
        for sel in editor.selections.ranges() {
            if sel.is_empty() || sel.end() < line_start || sel.start() > line_end_full + 1 {
                continue;
            }
            if sel.end() == line_start && line_len > 0 {
                continue;
            }
            let a = sel.start().max(line_start) - line_start;
            let b = sel.end().min(line_end_full) - line_start;
            let mut x2 = text_x0 - scroll_x + ll.x_of(b);
            if sel.end() > line_end_full {
                x2 += char_w * 0.6;
            }
            let r = Rect::from_min_max(pos2(text_x0 - scroll_x + ll.x_of(a), y), pos2(x2, y + line_h));
            text_painter.rect_filled(r, CornerRadius::ZERO, theme.selection);
        }
        // Matching brackets.
        if let Some((a, b)) = brackets {
            for o in [a, b] {
                if o >= line_start && o < line_end_full {
                    let c = o - line_start;
                    let r = Rect::from_min_max(
                        pos2(text_x0 - scroll_x + ll.x_of(c), y + 1.0),
                        pos2(text_x0 - scroll_x + ll.x_of(c + 1), y + line_h - 1.0),
                    );
                    text_painter.rect_filled(r, CornerRadius::same(2), theme.bracket_match);
                }
            }
        }
        // Text.
        text_painter.galley(pos2(text_x0 - scroll_x, text_y), ll.galley.clone(), theme.text_editor);
        // Inline debugger values after the line end (while suspended).
        for (_, text) in marks.inline.iter().filter(|(l, _)| *l == line) {
            let x = text_x0 - scroll_x + ll.x_of(ll.len) + 2.0 * char_w;
            text_painter.text(
                pos2(x, y + line_h * 0.5),
                Align2::LEFT_CENTER,
                text,
                inline_font.clone(),
                theme.inline_value,
            );
        }
        // Folded placeholder.
        if editor.folded.contains(&line) && fold_starts.contains(&line) {
            let x = text_x0 - scroll_x + ll.x_of(ll.len) + char_w;
            let r = Rect::from_min_size(pos2(x, y + 2.0), vec2(char_w * 3.5, line_h - 4.0));
            text_painter.rect_filled(r, CornerRadius::same(3), theme.bracket_match);
            text_painter.text(r.center(), Align2::CENTER_CENTER, "…", font.clone(), theme.text_muted);
        }
        // Right margin.
        let mx = text_x0 - scroll_x + RIGHT_MARGIN_COL as f32 * char_w;
        if mx > text_rect.left() {
            text_painter.vline(mx, y..=y + line_h, Stroke::new(1.0, theme.right_margin));
        }
        // Diagnostics.
        for p in problems.iter().filter(|p| p.start.line <= line && line <= p.end.line) {
            let a = if p.start.line == line { p.start.column } else { 0 };
            let mut b = if p.end.line == line { p.end.column } else { line_len };
            if b <= a {
                b = (a + 1).min(line_len.max(a + 1));
            }
            let x1 = text_x0 - scroll_x + ll.x_of(a);
            let x2 = (text_x0 - scroll_x + ll.x_of(b)).max(x1 + char_w);
            let color = severity_color(&theme, p.severity);
            if p.severity == Severity::Hint {
                continue;
            }
            wavy(&text_painter, x1, x2, y + line_h - 2.0, color);
        }
        // Carets.
        for sel in editor.selections.ranges() {
            let h = sel.head;
            if h >= line_start && h <= line_end_full && doc.char_to_line(h) == line {
                let x = text_x0 - scroll_x + ll.x_of(h - line_start);
                if sel == &primary {
                    caret_screen = Some(pos2(x, y + line_h));
                }
                if blink_on && x >= text_rect.left() - 1.0 {
                    text_painter.rect_filled(
                        Rect::from_min_size(pos2(x - 1.0, y + 1.0), vec2(2.0, line_h - 2.0)),
                        CornerRadius::ZERO,
                        theme.caret,
                    );
                }
            }
        }
        layouts.push((line, y, ll));
    }

    // ------------------------------------------------------------ gutter
    let gutter = Rect::from_min_max(rect.min, pos2(rect.left() + gutter_w, rect.bottom()));
    painter.rect_filled(gutter, CornerRadius::ZERO, theme.editor_bg);
    let num_font = FontId::monospace((settings.editor_font_size - 1.0).max(8.0));
    // The breakpoint area: the line numbers, between the run icons and the fold markers.
    let bp_area = (gutter.left() + 18.0)..(gutter.left() + 18.0 + numbers_w);
    let bp_x = (bp_area.start + bp_area.end) * 0.5;
    let bp_radius = (line_h * 0.32).clamp(4.0, 7.0);
    let hover_line = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| gutter.contains(*p) && bp_area.contains(&p.x) && path.is_some())
        .and_then(|p| map.line((((p.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize));
    for (line, y, _) in &layouts {
        let cy = y + line_h * 0.5;
        let breakpoint = marks.breakpoints.iter().find(|b| b.line == *line);
        if let Some(bp) = breakpoint {
            crate::debug_panel::paint_breakpoint(&painter, pos2(bp_x, cy), bp_radius, bp, &theme);
        } else if hover_line == Some(*line) {
            painter.circle_filled(pos2(bp_x, cy), bp_radius, theme.breakpoint.gamma_multiply(0.45));
        }
        if marks.execution == Some(*line) {
            crate::debug_panel::paint_exec_arrow(&painter, pos2(bp_x, cy), &theme);
        }
        if settings.show_line_numbers
            && breakpoint.is_none()
            && marks.execution != Some(*line)
            && hover_line != Some(*line)
        {
            let color = if *line == caret_line { theme.line_number_active } else { theme.line_number };
            painter.text(
                pos2(gutter.right() - 20.0, cy),
                Align2::RIGHT_CENTER,
                (line + 1).to_string(),
                num_font.clone(),
                color,
            );
        }
        if run_lines.contains(line) {
            let x = gutter.left() + 9.0;
            let tri = vec![pos2(x - 4.0, cy - 5.0), pos2(x + 5.0, cy), pos2(x - 4.0, cy + 5.0)];
            painter.add(Shape::convex_polygon(tri, theme.success, Stroke::NONE));
        }
        if fold_starts.contains(line) {
            let x = gutter.right() - 10.0;
            let collapsed = editor.folded.contains(line);
            let pts = if collapsed {
                vec![pos2(x - 2.5, cy - 4.0), pos2(x + 3.0, cy), pos2(x - 2.5, cy + 4.0)]
            } else {
                vec![pos2(x - 4.0, cy - 2.5), pos2(x + 4.0, cy - 2.5), pos2(x, cy + 3.0)]
            };
            painter.add(Shape::convex_polygon(pts, theme.text_disabled, Stroke::NONE));
        }
    }

    // ------------------------------------------------------------ error stripe + scrollbar
    let stripe = Rect::from_min_max(pos2(rect.right() - STRIPE_W, rect.top()), rect.max);
    let total_rows = map.len() as f32;
    let content_h = total_rows * line_h;
    if content_h > rect.height() {
        let thumb_h = (rect.height() / content_h * rect.height()).max(24.0);
        let thumb_y = rect.top() + scroll / total_rows.max(1.0) * rect.height();
        let thumb = Rect::from_min_size(pos2(stripe.left() + 3.0, thumb_y), vec2(STRIPE_W - 6.0, thumb_h));
        let sb_id = id.with("scrollbar");
        let sb = ui.interact(stripe, sb_id, Sense::click_and_drag());
        if (sb.dragged() || sb.clicked())
            && let Some(p) = sb.interact_pointer_pos()
        {
            scroll = ((p.y - rect.top() - thumb_h * 0.5) / rect.height() * total_rows).clamp(0.0, max_scroll);
        }
        let alpha = if sb.hovered() || sb.dragged() { 90 } else { 45 };
        painter.rect_filled(thumb, CornerRadius::same(4), Color32::from_white_alpha(alpha));
    }
    let lines_f = doc.len_lines().max(1) as f32;
    for p in problems.iter().filter(|p| p.severity <= Severity::Warning) {
        let y = rect.top()
            + map.row_of(p.start.line) as f32 / total_rows.max(lines_f.min(total_rows)).max(1.0) * rect.height();
        let r = Rect::from_min_size(pos2(stripe.left() + 2.0, y), vec2(STRIPE_W - 4.0, 2.5));
        painter.rect_filled(r, CornerRadius::ZERO, severity_color(&theme, p.severity));
    }

    // ------------------------------------------------------------ inspection widget (top right)
    let (errors, warnings) = problems.iter().fold((0, 0), |(e, w), p| match p.severity {
        Severity::Error => (e + 1, w),
        Severity::Warning => (e, w + 1),
        _ => (e, w),
    });
    if errors + warnings > 0 {
        let label = match (errors, warnings) {
            (0, w) => format!("⚠ {w}"),
            (e, 0) => format!("✖ {e}"),
            (e, w) => format!("✖ {e}  ⚠ {w}"),
        };
        let color = if errors > 0 { theme.error } else { theme.warning };
        let pos = pos2(stripe.left() - 8.0, rect.top() + 6.0);
        let galley = ui.fonts_mut(|f| f.layout_no_wrap(label, FontId::proportional(12.0), color));
        let bg = Rect::from_min_size(pos2(pos.x - galley.size().x - 8.0, pos.y - 2.0), galley.size() + vec2(8.0, 4.0));
        painter.rect_filled(bg, CornerRadius::same(4), theme.panel_bg);
        painter.galley(pos2(bg.left() + 4.0, pos.y), galley, color);
    }

    // ------------------------------------------------------------ pointer input
    let hit_scroll = scroll;
    let hit = |pos: Pos2, layouts: &[(usize, f32, LineLayout)], ui: &egui::Ui| -> usize {
        let row = (((pos.y - rect.top()) / line_h) + hit_scroll).floor().max(0.0) as usize;
        let row = row.min(map.len().saturating_sub(1));
        let line = map.line(row).unwrap_or(0);
        let x = pos.x - text_x0 + scroll_x;
        let col = match layouts.iter().find(|(l, _, _)| *l == line) {
            Some((_, _, ll)) => ll.col_at_x(x),
            None => layout_line(ui, doc, hl, line, &font, &theme, tab).col_at_x(x),
        };
        doc.line_start(line) + col.min(doc.line_len(line))
    };

    let mut commands: Vec<(&'static str, serde_json::Value)> = Vec::new();
    let pointer = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.interact_pos(), i.modifiers));
    if let (true, Some(pos)) = (pointer.0, pointer.1)
        && rect.contains(pos)
        && !stripe.contains(pos)
    {
        ui.memory_mut(|m| m.request_focus(id));
        if gutter.contains(pos) {
            let row = (((pos.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize;
            if let Some(line) = map.line(row) {
                if pos.x > gutter.right() - 18.0 && fold_starts.contains(&line) {
                    commands.push(("code.folding.toggle", json!({ "line": line })));
                } else if pos.x < gutter.left() + 18.0 && run_lines.contains(&line) {
                    if let Some(p) = &path {
                        commands.push(("run.contextConfiguration", json!({ "path": p, "line": line })));
                    }
                } else if let Some(p) = path.as_ref().filter(|_| bp_area.contains(&pos.x)) {
                    commands.push(("run.toggleBreakpoint.lineBreakpoint", json!({ "path": p, "line": line })));
                } else {
                    let start = doc.line_start(line);
                    let end = (start + doc.line_len(line) + 1).min(doc.len_chars());
                    commands.push(("editor.setSelection", json!({ "anchor": start, "head": end })));
                }
            }
        } else {
            let offset = hit(pos, &layouts, ui);
            let m = pointer.2;
            commands.push((
                "editor.click",
                json!({ "offset": offset, "extend": m.shift && !m.alt, "addCaret": m.alt && m.shift }),
            ));
            if m.mac_cmd || m.command {
                commands.push(("navigate.declarationOrUsages", json!({})));
            }
            st.drag_anchor_active = true;
        }
    }
    if response.double_clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && text_rect.contains(pos)
    {
        commands.push(("editor.click", json!({ "offset": hit(pos, &layouts, ui), "clicks": 2 })));
    }
    if response.triple_clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && text_rect.contains(pos)
    {
        commands.push(("editor.click", json!({ "offset": hit(pos, &layouts, ui), "clicks": 3 })));
    }
    if response.dragged()
        && st.drag_anchor_active
        && let Some(pos) = response.interact_pointer_pos()
    {
        // Auto-scroll when dragging past the edges.
        if pos.y < rect.top() {
            scroll = (scroll - 1.0).max(0.0);
        } else if pos.y > rect.bottom() {
            scroll = (scroll + 1.0).min(max_scroll);
        }
        commands.push(("editor.drag", json!({ "offset": hit(pos, &layouts, ui) })));
    }
    if response.drag_stopped() || !ui.input(|i| i.pointer.primary_down()) {
        st.drag_anchor_active = false;
    }

    // Diagnostic tooltip.
    let mut response = response;
    if let Some(pos) = response.hover_pos()
        && text_rect.contains(pos)
        && !ui.input(|i| i.pointer.any_down())
    {
        let off = hit(pos, &layouts, ui);
        let p = doc.offset_to_position(off);
        let msgs: Vec<String> = problems
            .iter()
            .filter(|pr| pr.start <= p && p <= pr.end && pr.severity <= Severity::Info)
            .map(|pr| pr.message.clone())
            .collect();
        if !msgs.is_empty() {
            response = response.on_hover_text_at_pointer(msgs.join("\n\n"));
        } else if !marks.locals.is_empty() {
            // While suspended: the value of the variable under the pointer.
            let word = doc.slice(codecraft_text::movement::word_range_at(doc, off));
            if let Some((name, value)) = marks.locals.iter().find(|(n, _)| *n == word) {
                response = response.on_hover_text_at_pointer(format!("{name} = {value}"));
            }
        }
    }
    let mut gutter_menu_breakpoint = None;
    if let Some(pos) = response.hover_pos()
        && gutter.contains(pos)
        && bp_area.contains(&pos.x)
        && let Some(line) = map.line((((pos.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize)
        && let Some(bp) = marks.breakpoints.iter().find(|b| b.line == line)
    {
        if response.secondary_clicked() {
            gutter_menu_breakpoint = Some(bp.id);
        }
        response = response.on_hover_text_at_pointer(breakpoint_tooltip(bp));
    }
    let _ = response;

    // Persist view state.
    if let Some(e) = s.ws.editor_mut(eid) {
        e.scroll_line = scroll;
        e.scroll_x = scroll_x;
        e.reveal_caret = reveal;
    }
    st.caret_screen = caret_screen;
    st.editor_rect = Some(rect);
    st.editor_line_h = line_h;
    if focused {
        let ime_rect =
            caret_screen.map(|p| Rect::from_min_size(p - vec2(0.0, line_h), vec2(2.0, line_h))).unwrap_or(rect);
        ui.output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                purpose: egui::IMEPurpose::Normal,
                rect,
                cursor_rect: ime_rect,
                should_interrupt_composition: false,
            })
        });
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
            )
        });
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
    for (cmd, params) in commands {
        crate::run_command(s, st, cmd, &params);
    }
    if let Some(id) = gutter_menu_breakpoint {
        crate::debug_panel::select_breakpoint(s, st, id);
    }
}

fn s_set_page(st: &mut UiState, rows: usize) {
    st.page_lines = rows.saturating_sub(2).max(1);
}
