//! The code editor widget: custom-painted, visible rows only. All edits and caret changes go
//! through engine commands; this module only paints and turns pointer input into commands.

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{Align2, Color32, CornerRadius, FontId, Galley, Id, Pos2, Rect, Sense, Shape, Stroke, pos2, vec2};
use serde_json::json;

use codecraft_engine::Session;
use codecraft_syntax::{FoldRange, HighlightKind, Highlights};
use codecraft_text::Document;
use codecraft_text::wrap::WrapConfig;
use codecraft_workspace::{EditorId, InlayKind, Problem, Severity, WhitespaceMode};

use crate::UiState;
use crate::theme::Theme;

const MAX_LAYOUT_CHARS: usize = 20_000;
const STRIPE_W: f32 = 12.0;

pub fn editor_id(e: EditorId) -> Id {
    Id::new(("codecraft-editor", e.0))
}

/// Visual rows of the editor after folding and soft wrap: row → (document line, first char
/// column of the row).
#[derive(Clone)]
struct DisplayMap {
    /// `None` = one row per line (nothing folded or wrapped).
    rows: Option<Vec<(usize, usize)>>,
    lines: usize,
}

/// Soft wrap is off for documents with more lines than this (the map is rebuilt per edit).
const MAX_WRAP_LINES: usize = 200_000;

impl DisplayMap {
    fn build(doc: &Document, folds: &[FoldRange], folded: &BTreeSet<usize>, wrap: Option<WrapConfig>) -> Self {
        let lines = doc.len_lines();
        if folded.is_empty() && wrap.is_none() {
            return Self { rows: None, lines };
        }
        let mut rows = Vec::with_capacity(lines);
        let mut line = 0;
        while line < lines {
            match wrap {
                Some(cfg) => {
                    let text = doc.line_text(line).unwrap_or_default();
                    rows.extend(codecraft_text::wrap::wrap_line(&text, cfg).into_iter().map(|c| (line, c)));
                }
                None => rows.push((line, 0)),
            }
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

    /// `(line, first column)` of a row.
    fn row(&self, row: usize) -> Option<(usize, usize)> {
        match &self.rows {
            Some(r) => r.get(row).copied(),
            None => (row < self.lines).then_some((row, 0)),
        }
    }

    fn line(&self, row: usize) -> Option<usize> {
        self.row(row).map(|(l, _)| l)
    }

    /// The first row showing `line` (or the row of the fold hiding it).
    fn row_of(&self, line: usize) -> usize {
        match &self.rows {
            Some(r) => {
                let i = r.partition_point(|(l, _)| *l < line);
                if r.get(i).is_some_and(|(l, _)| *l == line) { i } else { i.saturating_sub(1) }
            }
            None => line,
        }
    }

    /// The row showing char column `col` of `line`.
    fn row_of_col(&self, line: usize, col: usize) -> usize {
        let mut row = self.row_of(line);
        if let Some(r) = &self.rows {
            while r.get(row + 1).is_some_and(|(l, c)| *l == line && *c <= col) {
                row += 1;
            }
        }
        row
    }

    /// End column of a row: the next row's start on the same line, else the line length.
    fn row_end(&self, row: usize, line_len: usize) -> usize {
        let Some((line, _)) = self.row(row) else { return line_len };
        match self.row(row + 1) {
            Some((l, c)) if l == line => c.min(line_len),
            _ => line_len,
        }
    }

    /// Whether a row is the last one of its line.
    fn is_last_of_line(&self, row: usize) -> bool {
        let line = self.line(row);
        self.line(row + 1) != line || line.is_none()
    }
}

/// Galley text that is not document text: an inlay hint or the IME pre-edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Virtual {
    /// Index into [`Decor::inlays`].
    Inlay(usize),
    Preedit,
}

#[derive(Clone, Debug)]
struct VirtualSpan {
    what: Virtual,
    /// Galley chars it occupies.
    galley: std::ops::Range<usize>,
}

/// A whitespace char Show Whitespaces marks: its galley chars (a tab spans several).
#[derive(Clone, Copy, Debug)]
struct WsMark {
    galley: usize,
    width: usize,
    tab: bool,
}

/// A laid-out line and the mapping from char columns to galley chars (tabs expand).
struct LineLayout {
    galley: Arc<Galley>,
    /// `xcols[c]` = galley char index of char column `c`; `None` = identity.
    xcols: Option<Vec<usize>>,
    len: usize,
    virtuals: Vec<VirtualSpan>,
    whitespace: Vec<WsMark>,
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
        self.gx(self.xcol(col))
    }

    /// x of a galley char index.
    fn gx(&self, index: usize) -> f32 {
        self.galley.pos_from_cursor(CCursor::new(index)).min.x
    }

    /// The virtual span (inlay or pre-edit) under x, with the galley char index there.
    fn virtual_at(&self, x: f32) -> Option<(&VirtualSpan, usize)> {
        self.virtuals.iter().find_map(|v| {
            let (x0, x1) = (self.gx(v.galley.start), self.gx(v.galley.end));
            if x < x0 || x >= x1 {
                return None;
            }
            let i: usize = self.galley.cursor_from_pos(vec2(x, self.galley.size().y * 0.5)).index.into();
            Some((v, i.clamp(v.galley.start, v.galley.end.saturating_sub(1).max(v.galley.start))))
        })
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

/// Language-server decorations for the current document revision (stale ones are not passed),
/// plus what else the view draws into the text: the IME pre-edit and whitespace marks.
#[derive(Clone, Copy, Default)]
struct Decor<'a> {
    semantic: Option<&'a [Vec<codecraft_workspace::SemanticSpan>]>,
    /// SQL highlighting inside Rust string literals (over the string and semantic colours).
    injected: Option<&'a Highlights>,
    /// Inlay hints sorted by offset.
    inlays: &'a [codecraft_workspace::InlayHint],
    /// Uncommitted IME composition: char offset and text.
    preedit: Option<(usize, &'a str)>,
    /// `preedit` is an inline completion proposal (ghost text: muted, caret before it).
    ghost: bool,
    /// Show Whitespaces (and which).
    whitespace: Option<WhitespaceMode>,
}

/// Which whitespace chars of a line Show Whitespaces marks: `(leading end, trailing start)` char
/// columns; whitespace before the first or after the last is leading/trailing. A blank line is
/// all trailing.
fn whitespace_bounds(chars: &[char]) -> (usize, usize) {
    let first = chars.iter().position(|c| !c.is_whitespace());
    let last = chars.iter().rposition(|c| !c.is_whitespace());
    match (first, last) {
        (Some(f), Some(l)) => (f, l + 1),
        _ => (0, 0),
    }
}

fn marks_whitespace(mode: WhitespaceMode, col: usize, bounds: (usize, usize)) -> bool {
    match mode {
        WhitespaceMode::All => true,
        WhitespaceMode::LeadingTrailing => col < bounds.0 || col >= bounds.1,
        WhitespaceMode::Trailing => col >= bounds.1,
    }
}

#[allow(clippy::too_many_arguments)]
fn layout_line(
    ui: &egui::Ui,
    doc: &Document,
    hl: &Highlights,
    decor: Decor<'_>,
    line: usize,
    cols: std::ops::Range<usize>,
    font: &FontId,
    theme: &Theme,
    tab: usize,
) -> LineLayout {
    let text = doc.line_text(line).unwrap_or_default();
    let full: Vec<char> = text.chars().take(MAX_LAYOUT_CHARS).collect();
    let n = full.len();
    // Per-char style: lexical class, overridden by semantic tokens.
    let mut kinds: Vec<Option<HighlightKind>> = vec![None; n];
    for span in hl.line(line) {
        for k in kinds.iter_mut().take(span.end.min(n)).skip(span.start) {
            *k = Some(span.kind);
        }
    }
    let mut mutable = vec![false; n];
    let mut unresolved = vec![false; n];
    if let Some(sem) = decor.semantic.and_then(|v| v.get(line)) {
        for sp in sem {
            for i in sp.start.min(n)..sp.end.min(n) {
                if let Some(k) = kinds.get_mut(i) {
                    *k = Some(sp.kind);
                }
                if let Some(m) = mutable.get_mut(i) {
                    *m = sp.mutable;
                }
                if let Some(u) = unresolved.get_mut(i) {
                    *u = sp.unresolved;
                }
            }
        }
    }
    if let Some(inj) = decor.injected {
        for span in inj.line(line) {
            for k in kinds.iter_mut().take(span.end.min(n)).skip(span.start) {
                *k = Some(span.kind);
            }
        }
    }
    // The row's part of the line (soft wrap shows a line as several rows).
    let seg = cols.start.min(n)..cols.end.min(n).max(cols.start.min(n));
    let last_row = seg.end == n;
    // Tab stops continue from the row's display column.
    let mut x = full.get(..seg.start).unwrap_or(&[]).iter().fold(0usize, |x, c| {
        if *c == '\t' { x + tab - (x % tab.max(1)) } else { x + 1 }
    });
    let chars: Vec<char> = full.get(seg.clone()).unwrap_or(&[]).to_vec();
    let kinds: Vec<Option<HighlightKind>> = kinds.get(seg.clone()).unwrap_or(&[]).to_vec();
    let mutable: Vec<bool> = mutable.get(seg.clone()).unwrap_or(&[]).to_vec();
    let unresolved: Vec<bool> = unresolved.get(seg.clone()).unwrap_or(&[]).to_vec();
    let n = chars.len();
    let has_tabs = chars.contains(&'\t');
    // Inlay hints on this row, as (column within the row, index into `decor.inlays`).
    let line_start = doc.line_start(line);
    let in_row = |offset: usize| {
        offset >= line_start + seg.start
            && (offset < line_start + seg.end || (last_row && offset == line_start + seg.end))
    };
    let first = decor.inlays.partition_point(|h| h.offset < line_start + seg.start);
    let inlays: Vec<(usize, usize)> = decor
        .inlays
        .get(first..)
        .unwrap_or(&[])
        .iter()
        .take_while(|h| in_row(h.offset))
        .enumerate()
        .map(|(i, h)| (h.offset.saturating_sub(line_start + seg.start), first + i))
        .collect();
    // The IME pre-edit, if the caret is on this row.
    let preedit = decor
        .preedit
        .filter(|(o, t)| !t.is_empty() && in_row(*o))
        .map(|(o, t)| (o.saturating_sub(line_start + seg.start), t));
    let mapped = has_tabs || !inlays.is_empty() || preedit.is_some();
    let mut job = LayoutJob::default();
    job.wrap.max_width = f32::INFINITY;
    let mut xcols: Vec<usize> = if mapped { Vec::with_capacity(n + 1) } else { Vec::new() };
    let mut virtuals: Vec<VirtualSpan> = Vec::new();
    let mut gi = 0usize;
    let hint_font = FontId::new((font.size * 0.86).max(8.0), font.family.clone());
    let mut next_inlay = 0usize;
    // Pre-edit text (at the caret) first, then the inlay hints at a column.
    let mut emit_virtuals = |job: &mut LayoutJob, virtuals: &mut Vec<VirtualSpan>, col: usize, gi: &mut usize| {
        if let Some((c, text)) = preedit
            && c == col
        {
            let start = *gi;
            *gi += text.chars().count();
            let fmt = if decor.ghost {
                TextFormat { font_id: font.clone(), color: theme.text_muted, italics: true, ..TextFormat::default() }
            } else {
                TextFormat {
                    font_id: font.clone(),
                    color: theme.text_editor,
                    underline: Stroke::new(1.0, theme.text_editor),
                    ..TextFormat::default()
                }
            };
            job.append(text, 0.0, fmt);
            virtuals.push(VirtualSpan { what: Virtual::Preedit, galley: start..*gi });
        }
        while let Some((c, idx)) = inlays.get(next_inlay) {
            if *c != col {
                break;
            }
            let Some(h) = decor.inlays.get(*idx) else { break };
            let label =
                format!("{}{}{}", if h.pad_left { " " } else { "" }, h.label, if h.pad_right { " " } else { "" });
            let start = *gi;
            *gi += label.chars().count();
            let fmt = TextFormat {
                font_id: hint_font.clone(),
                color: theme.inlay_text,
                background: theme.inlay_bg,
                valign: egui::Align::Center,
                ..TextFormat::default()
            };
            job.append(&label, 0.0, fmt);
            virtuals.push(VirtualSpan { what: Virtual::Inlay(*idx), galley: start..*gi });
            next_inlay += 1;
        }
    };
    // Show Whitespaces: which of the row's whitespace chars get a mark.
    let bounds = whitespace_bounds(&full);
    let mut whitespace: Vec<WsMark> = Vec::new();
    let mut col = 0usize;
    while col < n {
        if mapped {
            xcols.push(gi);
        }
        emit_virtuals(&mut job, &mut virtuals, col, &mut gi);
        // A run of chars with the same style.
        let style = |i: usize| (kinds.get(i).copied().flatten(), mutable.get(i).copied(), unresolved.get(i).copied());
        let st0 = style(col);
        let mut end = col + 1;
        while end < n
            && style(end) == st0
            && !inlays.iter().any(|(c, _)| *c == end)
            && preedit.is_none_or(|(c, _)| c != end)
        {
            end += 1;
        }
        let mut seg_text = String::new();
        for (i, c) in chars.get(col..end).unwrap_or(&[]).iter().enumerate() {
            if mapped && i > 0 {
                xcols.push(gi);
            }
            let w = if *c == '\t' { tab - (x % tab.max(1)) } else { 1 };
            if let Some(mode) = decor.whitespace
                && (*c == ' ' || *c == '\t')
                && marks_whitespace(mode, seg.start + col + i, bounds)
            {
                whitespace.push(WsMark { galley: gi, width: w, tab: *c == '\t' });
            }
            if *c == '\t' {
                seg_text.extend(std::iter::repeat_n(' ', w));
            } else {
                seg_text.push(*c);
            }
            x += w;
            gi += w;
        }
        let (kind, is_mut, is_unresolved) = st0;
        let mut color = kind.map(|k| theme.syntax(k)).unwrap_or(theme.text_editor);
        if is_unresolved == Some(true) {
            color = theme.error;
        }
        let mut fmt = TextFormat { font_id: font.clone(), color, ..TextFormat::default() };
        if is_mut == Some(true) {
            fmt.underline = Stroke::new(1.0, color.gamma_multiply(0.7));
        }
        job.append(&seg_text, 0.0, fmt);
        col = end;
    }
    if mapped {
        xcols.push(gi);
    }
    emit_virtuals(&mut job, &mut virtuals, n, &mut gi);
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    LineLayout { galley, xcols: mapped.then_some(xcols), len: n, virtuals, whitespace }
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

/// Lines with a gutter run icon: `fn main` and `#[test]` functions, plus `mod tests` (other
/// languages: `run_configs::gutter_run_lines`).
fn run_lines(doc: &Document, lang: codecraft_syntax::Language, path: Option<&std::path::Path>) -> Vec<usize> {
    if lang != codecraft_syntax::Language::Rust {
        return path.map(|p| codecraft_engine::run_configs::gutter_run_lines(p, &doc.text())).unwrap_or_default();
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

/// One painted visual row: a line, or (with soft wrap) a part of one.
struct RowLayout {
    line: usize,
    /// Char columns of the line shown on this row.
    start: usize,
    end: usize,
    /// The row ends its line.
    last: bool,
    y: f32,
    /// Continuation rows of a wrapped line start right of the text origin by the line's
    /// indentation (pixels).
    indent_x: f32,
    ll: LineLayout,
}

impl RowLayout {
    /// The part of the line columns `a..b` on this row, if any.
    fn clip(&self, a: usize, b: usize) -> Option<(usize, usize)> {
        let (a, b) = (a.max(self.start), b.min(self.end));
        (a < b).then_some((a, b))
    }

    /// Whether a caret at line column `col` is drawn on this row.
    fn shows(&self, col: usize) -> bool {
        self.start <= col && (col < self.end || (col == self.end && self.last))
    }

    /// x of a line column within this row (relative to the text origin).
    fn x(&self, col: usize) -> f32 {
        self.indent_x + self.ll.x_of(col.saturating_sub(self.start).min(self.end.saturating_sub(self.start)))
    }

    /// x right after the row's last char and any hints after it (relative to the text origin).
    fn x_end(&self) -> f32 {
        self.indent_x + self.ll.galley.size().x
    }

    /// x of a galley char index (relative to the text origin).
    fn gx(&self, index: usize) -> f32 {
        self.indent_x + self.ll.gx(index)
    }
}

/// Pixels a row is indented by: continuation rows of a wrapped line keep its indentation.
fn row_indent_x(doc: &Document, line: usize, start: usize, wrap: Option<WrapConfig>, char_w: f32) -> f32 {
    match wrap {
        Some(cfg) if start > 0 => {
            codecraft_text::wrap::line_indent(&doc.line_text(line).unwrap_or_default(), cfg) as f32 * char_w
        }
        _ => 0.0,
    }
}

/// Paint Show Whitespaces marks of a row: a dot per space, an arrow per tab.
fn paint_whitespace(painter: &egui::Painter, r: &RowLayout, origin: f32, cy: f32, char_w: f32, color: Color32) {
    let radius = (char_w * 0.09).clamp(0.8, 1.6);
    for m in &r.ll.whitespace {
        let x0 = origin + r.gx(m.galley);
        let x1 = origin + r.gx(m.galley + m.width);
        if m.tab {
            let (a, b) = (x0 + char_w * 0.25, (x1 - char_w * 0.25).max(x0 + char_w * 0.5));
            let stroke = Stroke::new(1.0, color);
            let head = (char_w * 0.3).clamp(2.0, 4.0);
            painter.line_segment([pos2(a, cy), pos2(b, cy)], stroke);
            painter.line_segment([pos2(b - head, cy - head), pos2(b, cy)], stroke);
            painter.line_segment([pos2(b - head, cy + head), pos2(b, cy)], stroke);
        } else {
            painter.circle_filled(pos2((x0 + x1) * 0.5, cy), radius, color);
        }
    }
}

type MapCache = Arc<(u64, Arc<DisplayMap>)>;

/// An inlay hint under the pointer.
struct InlayHit {
    /// The hint's char offset (its identity in the document's hint list).
    offset: usize,
    /// The label part under the pointer, if the hint has parts.
    part: Option<usize>,
    /// Cmd+click goes somewhere (a part location, or a type's declaration).
    navigable: bool,
    /// Row top and the screen x range of the part (or whole hint).
    y: f32,
    x: (f32, f32),
}

/// The IME composition shown in a focused editor, with the caret position in it (chars).
/// `Preedit` events set it and `Commit` clears it (the shell's input routing types the
/// committed text); losing focus drops it.
fn ime_preedit(ui: &egui::Ui, id: Id) -> Option<(String, usize)> {
    let key = id.with("ime-preedit");
    if !ui.memory(|m| m.has_focus(id)) {
        ui.data_mut(|d| d.remove::<(String, usize)>(key));
        return None;
    }
    let events: Vec<egui::ImeEvent> = ui.input(|i| {
        i.events
            .iter()
            .filter_map(|e| match e {
                egui::Event::Ime(ev) => Some(ev.clone()),
                _ => None,
            })
            .collect()
    });
    for ev in events {
        match ev {
            egui::ImeEvent::Preedit { text, active_range_chars } if !text.is_empty() => {
                let n = text.chars().count();
                let cursor = active_range_chars.map_or(n, |r| r.end.min(n));
                ui.data_mut(|d| d.insert_temp(key, (text, cursor)));
            }
            egui::ImeEvent::Preedit { .. } | egui::ImeEvent::Commit(_) => {
                ui.data_mut(|d| d.remove::<(String, usize)>(key));
            }
            _ => {}
        }
    }
    ui.data(|d| d.get_temp::<(String, usize)>(key))
}

/// Paint the editor and handle pointer input. Returns the caret's screen position.
pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, eid: EditorId) {
    let theme = st.theme.clone();
    let mut settings = s.ws.settings.clone();
    // Presentation Mode enlarges the editor font without touching the setting.
    settings.editor_font_size = codecraft_engine::appearance::chrome(s).editor_font_size;
    let Some(editor) = s.ws.editor(eid).cloned() else { return };
    // A Jupyter notebook that parsed shows as cells (M12 track PY).
    if codecraft_engine::jupyter::shows_notebook(s, editor.doc) {
        crate::notebook_ui::show(ui, s, st, eid, editor.doc);
        return;
    }
    let path = s.ws.doc(editor.doc).and_then(|d| d.path.clone());
    let problems: Vec<Problem> =
        path.as_deref().map(|p| s.ws.highlighted_problems_for(p).into_iter().cloned().collect()).unwrap_or_default();
    let (find_matches, find_current) = s.find_bar.as_ref().map(|b| (b.matches.clone(), b.current)).unwrap_or_default();
    // Highlight Usages in File (until Escape), while the document is unchanged.
    let usage_ranges: Vec<std::ops::Range<usize>> = s
        .ws
        .doc(editor.doc)
        .and_then(|d| s.usages.highlights_for(eid, d.doc.revision()))
        .map(<[_]>::to_vec)
        .unwrap_or_default();
    let marks = path.as_deref().map(|p| DebugMarks::collect(s, p)).unwrap_or_default();
    let tests = path.as_deref().map(|p| crate::testing_panel::GutterTests::collect(s, p)).unwrap_or_default();
    // M14 track PF: samples per line from the open profiler snapshot.
    let hot = path.as_deref().map(|p| crate::profiler_ui::GutterHits::collect(s, p)).unwrap_or_default();
    let vcs = crate::vcs_panels::GutterVcs::collect(s, editor.doc);
    let bookmark_lines = path.as_deref().map(|p| s.bookmarks.lines_in(p)).unwrap_or_default();
    // Live template variables (current one highlighted), column mode, breadcrumbs.
    let doc_rev = s.ws.doc(editor.doc).map(|d| d.doc.revision()).unwrap_or(0);
    let template_ranges: Vec<(std::ops::Range<usize>, bool)> = s
        .editing
        .session_for(eid, doc_rev)
        .map(|t| {
            t.vars
                .iter()
                .enumerate()
                .flat_map(|(i, occ)| occ.iter().map(move |r| (r.clone(), i == t.current)))
                .collect()
        })
        .unwrap_or_default();
    let column_mode = s.editing.column_mode.contains(&eid);
    // A box selection reaching past line ends (its carets are drawn in virtual space).
    let vbox = s.column_box(eid);
    let preedit = ime_preedit(ui, editor_id(eid));
    // An inline completion proposal draws like a pre-edit (unless the IME is composing).
    let ghost = if preedit.is_none() { codecraft_engine::inline_completion::ghost_text(s, eid) } else { None };
    let crumbs_shown = s.ws.doc(editor.doc).is_some_and(|d| crate::navbar::has_breadcrumbs(d.language));
    codecraft_engine::sql_lang::refresh(s, editor.doc);
    let Some(od) = s.ws.doc_mut(editor.doc) else { return };
    od.refresh_caches();
    let od = &*od;
    let doc = &od.doc;
    let hl = od.cached_highlights();
    // Server decorations only when they match the current text (never misplaced while typing).
    let revision = od.doc.revision();
    let decor = Decor {
        semantic: od.semantic.as_ref().filter(|(r, _)| *r == revision).map(|(_, v)| v.as_slice()),
        injected: s.sql_lang.highlights_for(editor.doc, revision),
        inlays: if settings.show_inlay_hints {
            od.inlay_hints.as_ref().filter(|(r, _)| *r == revision).map(|(_, v)| v.as_slice()).unwrap_or(&[])
        } else {
            &[]
        },
        preedit: preedit
            .as_ref()
            .map(|(t, _)| (editor.selections.primary().head, t.as_str()))
            .or_else(|| ghost.as_ref().map(|(o, t)| (*o, t.as_str()))),
        ghost: ghost.is_some(),
        whitespace: settings.show_whitespace.then_some(settings.whitespace_mode),
    };
    let folds = od.cached_folds();
    // Settings › Editor › Code Style: the language's tab size.
    let tab = codecraft_engine::settings::code_style::indent(&settings, od.language).tab_size.max(1);

    let font = FontId::monospace(settings.editor_font_size);
    let inline_font = FontId::proportional((settings.editor_font_size - 1.0).max(8.0));
    let (char_w, row_h) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
    let line_h = (settings.editor_font_size * settings.line_spacing).max(row_h).round();
    let full_rect = ui.available_rect_before_wrap();
    let crumb_h = if crumbs_shown { crate::navbar::BREADCRUMBS_H } else { 0.0 };
    let rect =
        Rect::from_min_max(full_rect.min, pos2(full_rect.right(), (full_rect.bottom() - crumb_h).max(full_rect.top())));
    let crumb_rect = Rect::from_min_max(pos2(full_rect.left(), rect.bottom()), full_rect.max);
    let id = editor_id(eid);
    let response = ui.interact(rect, id, Sense::click_and_drag());
    ui.advance_cursor_after_rect(full_rect);

    let digits = doc.len_lines().max(1).ilog10() as f32 + 1.0;
    // Breakpoints sit over the line numbers (RustRover's New UI), so keep room for them.
    let numbers_w = if settings.show_line_numbers { (digits.max(2.0) + 1.0) * char_w } else { 16.0 };
    // Git annotations (blame) take a column at the gutter's left edge while shown.
    let ann_w = vcs.annotation_width(char_w);
    let gutter_w = ann_w + numbers_w + 18.0 + 14.0 + 6.0;
    let text_rect =
        Rect::from_min_max(pos2(rect.left() + gutter_w, rect.top()), pos2(rect.right() - STRIPE_W, rect.bottom()));
    let visible_rows = ((rect.height() / line_h).floor() as usize).max(1);
    s_set_page(st, visible_rows);

    // ------------------------------------------------------------ soft wrap
    let wrap_cols = ((text_rect.width() - 8.0) / char_w.max(1.0)).floor().max(10.0) as usize;
    // The tab's own View › Active Editor › Soft-Wrap choice, else the setting.
    let soft_wrap = editor.soft_wrap.unwrap_or(settings.soft_wrap);
    let wrap = (soft_wrap && doc.len_lines() <= MAX_WRAP_LINES).then(|| WrapConfig::new(wrap_cols, tab));
    let map: Arc<DisplayMap> = match wrap {
        None => Arc::new(DisplayMap::build(doc, folds, &editor.folded, None)),
        Some(cfg) => {
            let key = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                (editor.doc, revision, cfg.width, cfg.tab, &editor.folded).hash(&mut h);
                h.finish()
            };
            let cache_id = id.with("wrap-map");
            match ui.data(|d| d.get_temp::<MapCache>(cache_id)).filter(|c| c.0 == key) {
                Some(c) => c.1.clone(),
                None => {
                    let m = Arc::new(DisplayMap::build(doc, folds, &editor.folded, Some(cfg)));
                    ui.data_mut(|d| d.insert_temp(cache_id, Arc::new((key, m.clone()))));
                    m
                }
            }
        }
    };

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
    let caret_row = map.row_of_col(caret_line, primary.head.saturating_sub(doc.line_start(caret_line)));
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
    scroll_x = if wrap.is_some() { 0.0 } else { scroll_x.max(0.0) };

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, theme.editor_bg);
    let text_painter = ui.painter_at(text_rect);
    let text_x0 = text_rect.left() + 4.0;

    // Caret line layout for horizontal reveal.
    if reveal {
        if wrap.is_none() {
            let ll = layout_line(ui, doc, hl, decor, caret_line, 0..usize::MAX, &font, &theme, tab);
            let cx = ll.x_of(primary.head.saturating_sub(doc.line_start(caret_line)));
            let view_w = text_rect.width() - 8.0;
            if cx - scroll_x > view_w - 4.0 * char_w {
                scroll_x = cx - view_w + 8.0 * char_w;
            } else if cx < scroll_x + 2.0 * char_w {
                scroll_x = (cx - 8.0 * char_w).max(0.0);
            }
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
            st.run_lines_cache = Some((key, run_lines(doc, od.language, od.path.as_deref())));
        }
        st.run_lines_cache.as_ref().map(|(_, v)| v.clone()).unwrap_or_default()
    };
    let fold_starts: Vec<usize> = folds.iter().filter(|f| f.end_line > f.start_line).map(|f| f.start_line).collect();

    let mut caret_screen = None;
    let focused = ui.memory(|m| m.has_focus(id));
    let blink_on = !focused || (ui.input(|i| i.time) * 2.0 % 2.0) < 1.3;
    let mut indent_prev: Option<usize> = None;
    let mut rows: Vec<RowLayout> = Vec::with_capacity(rows_to_draw);

    for row in first_row..first_row + rows_to_draw {
        let Some((line, start)) = map.row(row) else { break };
        let y = row_y(row);
        if y > rect.bottom() {
            break;
        }
        let line_len = doc.line_len(line);
        let end = map.row_end(row, line_len).max(start);
        let last = map.is_last_of_line(row);
        let ll = layout_line(ui, doc, hl, decor, line, start..end, &font, &theme, tab);
        let indent_x = row_indent_x(doc, line, start, wrap, char_w);
        let r = RowLayout { line, start, end, last, y, indent_x, ll };
        let line_start = doc.line_start(line);
        let text_y = y + (line_h - row_h) * 0.5;
        let line_rect = Rect::from_min_max(pos2(rect.left(), y), pos2(rect.right(), y + line_h));
        let x_at = |c: usize| text_x0 - scroll_x + r.x(c);

        if row == caret_row && primary.is_empty() {
            painter.rect_filled(line_rect, CornerRadius::ZERO, theme.caret_row);
        }
        if let Some(bg) = vcs.line_bg(line, &theme) {
            painter.rect_filled(line_rect, CornerRadius::ZERO, bg);
        }
        if marks.execution == Some(line) {
            painter.rect_filled(line_rect, CornerRadius::ZERO, theme.execution_line);
        }
        // Indent guides (on a line's first row).
        if start == 0 {
            let indent_cols = {
                let text = doc.line_text(line).unwrap_or_default();
                if text.trim().is_empty() {
                    indent_prev.unwrap_or(0)
                } else {
                    let w: usize = text
                        .chars()
                        .take_while(|c| *c == ' ' || *c == '\t')
                        .map(|c| if c == '\t' { tab } else { 1 })
                        .sum();
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
        }
        // Occurrences of the identifier under the caret.
        if let Some(w) = &occurrence_word {
            let text = doc.line_text(line).unwrap_or_default();
            for (a, b) in word_occurrences(&text, w) {
                if let Some((a, b)) = r.clip(a, b) {
                    let rr = Rect::from_min_max(pos2(x_at(a), y + 1.0), pos2(x_at(b), y + line_h - 1.0));
                    text_painter.rect_filled(rr, CornerRadius::same(2), theme.occurrence);
                }
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
            let span = if a == b { r.shows(a).then_some((a, a)) } else { r.clip(a, b) };
            let Some((a, b)) = span else { continue };
            let color = if Some(i) == find_current { theme.find_match_current } else { theme.find_match };
            let rr = Rect::from_min_max(pos2(x_at(a), y + 1.0), pos2(x_at(b).max(x_at(a) + 2.0), y + line_h - 1.0));
            text_painter.rect_filled(rr, CornerRadius::same(2), color);
        }
        // Highlighted usages (sorted, so skip those ending before this line).
        let first_usage = usage_ranges.partition_point(|u| u.end < line_start);
        for u in usage_ranges.get(first_usage..).unwrap_or_default() {
            if u.start > line_end_full {
                break;
            }
            let a = u.start.max(line_start) - line_start;
            let b = u.end.min(line_end_full) - line_start;
            let Some((a, b)) = r.clip(a, b) else { continue };
            let rr = Rect::from_min_max(pos2(x_at(a), y + 1.0), pos2(x_at(b).max(x_at(a) + 2.0), y + line_h - 1.0));
            text_painter.rect_filled(rr, CornerRadius::same(2), theme.find_match);
        }
        // Selections.
        for sel in editor.selections.ranges() {
            if sel.is_empty() || sel.end() < line_start || sel.start() > line_end_full + 1 {
                continue;
            }
            if sel.end() == line_start && line_len > 0 {
                continue;
            }
            let a = (sel.start().max(line_start) - line_start).max(start);
            let b = (sel.end().min(line_end_full) - line_start).min(end);
            let tail = last && sel.end() > line_end_full;
            if a > b || (a == b && !tail) {
                continue;
            }
            let mut x2 = x_at(b);
            if tail {
                x2 += char_w * 0.6;
            }
            let rr = Rect::from_min_max(pos2(x_at(a), y), pos2(x2, y + line_h));
            text_painter.rect_filled(rr, CornerRadius::ZERO, theme.selection);
        }
        // A box selection's part in virtual space (past the end of a short line).
        let mut virtual_caret: Option<f32> = None;
        if let Some((a, h)) = vbox
            && last
            && (a.line.min(h.line)..=a.line.max(h.line)).contains(&line)
        {
            let width = codecraft_text::column::line_width(doc, line, tab);
            let vx = |c: usize| x_at(line_len) + c.saturating_sub(width) as f32 * char_w;
            let (left, right) = (a.column.min(h.column), a.column.max(h.column));
            if right > width && right > left {
                let rr = Rect::from_min_max(pos2(vx(left.max(width)), y), pos2(vx(right), y + line_h));
                text_painter.rect_filled(rr, CornerRadius::ZERO, theme.selection);
            }
            if h.column > width {
                virtual_caret = Some(vx(h.column));
            }
        }
        // Live template variables: the current one framed in the link colour.
        for (tr, current) in &template_ranges {
            if tr.end < line_start || tr.start > line_end_full {
                continue;
            }
            let a = tr.start.max(line_start) - line_start;
            let b = tr.end.min(line_end_full) - line_start;
            let span = if a == b { r.shows(a).then_some((a, a)) } else { r.clip(a, b) };
            let Some((a, b)) = span else { continue };
            let rr = Rect::from_min_max(pos2(x_at(a) - 1.0, y + 1.0), pos2(x_at(b).max(x_at(a) + 2.0) + 1.0, y + line_h - 1.0));
            let color = if *current { theme.link } else { theme.text_disabled };
            text_painter.rect_stroke(rr, CornerRadius::same(2), Stroke::new(1.0, color), egui::StrokeKind::Inside);
        }
        // Matching brackets.
        if let Some((a, b)) = brackets {
            for o in [a, b] {
                if o >= line_start && o < line_end_full && r.clip(o - line_start, o - line_start + 1).is_some() {
                    let c = o - line_start;
                    let rr = Rect::from_min_max(pos2(x_at(c), y + 1.0), pos2(x_at(c + 1), y + line_h - 1.0));
                    text_painter.rect_filled(rr, CornerRadius::same(2), theme.bracket_match);
                }
            }
        }
        // Text.
        text_painter.galley(pos2(text_x0 - scroll_x + r.indent_x, text_y), r.ll.galley.clone(), theme.text_editor);
        if decor.whitespace.is_some() {
            paint_whitespace(&text_painter, &r, text_x0 - scroll_x, y + line_h * 0.5, char_w, theme.whitespace);
        }
        if last {
            // Inline debugger values after the line end (while suspended).
            for (_, text) in marks.inline.iter().filter(|(l, _)| *l == line) {
                let x = text_x0 - scroll_x + r.x_end() + 2.0 * char_w;
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
                let x = text_x0 - scroll_x + r.x_end() + char_w;
                let rr = Rect::from_min_size(pos2(x, y + 2.0), vec2(char_w * 3.5, line_h - 4.0));
                text_painter.rect_filled(rr, CornerRadius::same(3), theme.bracket_match);
                text_painter.text(rr.center(), Align2::CENTER_CENTER, "…", font.clone(), theme.text_muted);
            }
        }
        // Right margin (not while soft-wrapping).
        if wrap.is_none() {
            let mx = text_x0 - scroll_x + settings.right_margin.min(10_000) as f32 * char_w;
            if mx > text_rect.left() {
                text_painter.vline(mx, y..=y + line_h, Stroke::new(1.0, theme.right_margin));
            }
        }
        // Diagnostics.
        for p in problems.iter().filter(|p| p.start.line <= line && line <= p.end.line) {
            if p.severity == Severity::Hint {
                continue;
            }
            let a = if p.start.line == line { p.start.column } else { 0 };
            let mut b = if p.end.line == line { p.end.column } else { line_len };
            if b <= a {
                b = (a + 1).min(line_len.max(a + 1));
            }
            let Some((a, b)) = r.clip(a, b).or_else(|| (last && a >= end && start <= a).then_some((a, b))) else {
                continue;
            };
            let x1 = x_at(a);
            let x2 = x_at(b).max(x1 + char_w);
            wavy(&text_painter, x1, x2, y + line_h - 2.0, severity_color(&theme, p.severity));
        }
        // Carets (the primary one after the IME pre-edit; box carets in virtual space).
        let preedit_x = r.ll.virtuals.iter().find(|v| v.what == Virtual::Preedit).map(|v| {
            // The caret stays before ghost text (an inline proposal).
            let cursor =
                if ghost.is_some() { 0 } else { preedit.as_ref().map_or(v.galley.len(), |(_, c)| (*c).min(v.galley.len())) };
            text_x0 - scroll_x + r.gx(v.galley.start + cursor)
        });
        for sel in editor.selections.ranges() {
            let h = sel.head;
            if h >= line_start && h <= line_end_full && doc.char_to_line(h) == line && r.shows(h - line_start) {
                let x = match (virtual_caret, preedit_x) {
                    (_, Some(px)) if sel == &primary => px,
                    (Some(vx), _) if h == line_end_full => vx,
                    _ => x_at(h - line_start),
                };
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
        rows.push(r);
    }

    // ------------------------------------------------------------ gutter
    let gutter = Rect::from_min_max(pos2(rect.left() + ann_w, rect.top()), pos2(rect.left() + gutter_w, rect.bottom()));
    painter.rect_filled(gutter, CornerRadius::ZERO, theme.editor_bg);
    let ann_rect = Rect::from_min_max(rect.min, pos2(rect.left() + ann_w, rect.bottom()));
    if ann_w > 0.0 {
        painter.rect_filled(ann_rect, CornerRadius::ZERO, theme.vcs_annotation_bg);
    }
    let num_font = FontId::monospace((settings.editor_font_size - 1.0).max(8.0));
    // The breakpoint area: the line numbers, between the run icons and the fold markers.
    let bp_area = (gutter.left() + 18.0)..(gutter.left() + 18.0 + numbers_w);
    let bp_x = (bp_area.start + bp_area.end) * 0.5;
    let bp_radius = (line_h * 0.32).clamp(4.0, 7.0);
    let hover_line = ui
        .input(|i| i.pointer.hover_pos())
        .filter(|p| gutter.contains(*p) && bp_area.contains(&p.x) && path.is_some())
        .and_then(|p| map.line((((p.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize));
    // Gutter marks go on a line's first row only.
    for r in rows.iter().filter(|r| r.start == 0) {
        let (line, y) = (&r.line, &r.y);
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
        let bookmark = bookmark_lines.iter().find(|(l, _)| l == line).map(|(_, m)| *m);
        if let Some(m) = bookmark {
            let x = if breakpoint.is_some() || marks.execution == Some(*line) { bp_area.start + 5.0 } else { bp_x };
            crate::bookmarks_panel::paint_badge(&painter, &theme, pos2(x, cy), line_h, m);
        }
        if settings.show_line_numbers
            && breakpoint.is_none()
            && bookmark.is_none()
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
        vcs.paint_line(&painter, &theme, *line, *y, line_h, gutter, ann_rect, &num_font);
        tests.paint_coverage(&painter, &theme, *line, *y, line_h, gutter.left() + 15.0);
        hot.paint(&painter, &theme, *line, *y, line_h, gutter.left() + 1.0, rect.right() - STRIPE_W - 8.0);
        if run_lines.contains(line) {
            let x = gutter.left() + 9.0;
            let tri = vec![pos2(x - 4.0, cy - 5.0), pos2(x + 5.0, cy), pos2(x - 4.0, cy + 5.0)];
            painter.add(Shape::convex_polygon(tri, tests.run_icon_color(*line, &theme), Stroke::NONE));
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
    // The char offset under a point, and how many display columns it lies past its line's end.
    let hit_ex = |pos: Pos2, rows: &[RowLayout], ui: &egui::Ui| -> (usize, usize) {
        let row = (((pos.y - rect.top()) / line_h) + hit_scroll).floor().max(0.0) as usize;
        let row = row.min(map.len().saturating_sub(1));
        let (line, start) = map.row(row).unwrap_or((0, 0));
        let line_len = doc.line_len(line);
        let end = map.row_end(row, line_len).max(start);
        let last = map.is_last_of_line(row);
        let x = pos.x - text_x0 + scroll_x;
        let (rel, x_end) = match rows.iter().find(|r| r.line == line && r.start == start) {
            Some(r) => (r.ll.col_at_x(x - r.indent_x), r.indent_x + r.ll.x_of(r.ll.len)),
            None => {
                let ll = layout_line(ui, doc, hl, decor, line, start..end, &font, &theme, tab);
                let ix = row_indent_x(doc, line, start, wrap, char_w);
                (ll.col_at_x(x - ix), ix + ll.x_of(ll.len))
            }
        };
        let max = if last { end } else { end.saturating_sub(1).max(start) };
        let col = (start + rel).min(max);
        let beyond = if last && col == line_len && x > x_end {
            ((x - x_end) / char_w.max(1.0)).round().max(0.0) as usize
        } else {
            0
        };
        (doc.line_start(line) + col.min(line_len), beyond)
    };
    let hit = |pos: Pos2, rows: &[RowLayout], ui: &egui::Ui| hit_ex(pos, rows, ui).0;
    // A box-selection corner: line and display column (may lie past the line end).
    let box_point = |pos: Pos2, rows: &[RowLayout], ui: &egui::Ui| -> (usize, usize) {
        let (offset, beyond) = hit_ex(pos, rows, ui);
        let p = codecraft_text::column::box_point_of(doc, offset, tab);
        (p.line, p.column + beyond)
    };
    let box_key = id.with("box-anchor");
    // The inlay hint (and label part) under a point.
    let inlay_at = |pos: Pos2, rows: &[RowLayout]| -> Option<InlayHit> {
        let r = rows.iter().find(|r| pos.y >= r.y && pos.y < r.y + line_h)?;
        let (v, gi) = r.ll.virtual_at(pos.x - text_x0 + scroll_x - r.indent_x)?;
        let Virtual::Inlay(index) = v.what else { return None };
        let h = decor.inlays.get(index)?;
        let pad = usize::from(h.pad_left);
        let k = gi.saturating_sub(v.galley.start).checked_sub(pad);
        let mut acc = 0usize;
        let mut part = None;
        for (i, p) in h.parts.iter().enumerate() {
            let n = p.value.chars().count();
            if k.is_some_and(|k| k >= acc && k < acc + n) {
                part = Some((i, v.galley.start + pad + acc..v.galley.start + pad + acc + n));
                break;
            }
            acc += n;
        }
        let navigable = h.kind == InlayKind::Type
            || match &part {
                Some((i, _)) => h.parts.get(*i).is_some_and(|p| p.location.is_some()),
                None => h.parts.iter().any(|p| p.location.is_some()),
            };
        let underline = part.as_ref().map_or(v.galley.clone(), |(_, g)| g.clone());
        Some(InlayHit {
            offset: h.offset,
            part: part.map(|(i, _)| i),
            navigable,
            y: r.y,
            x: (text_x0 - scroll_x + r.gx(underline.start), text_x0 - scroll_x + r.gx(underline.end)),
        })
    };

    let mut commands: Vec<(&'static str, serde_json::Value)> = Vec::new();
    let pointer = ui.input(|i| (i.pointer.primary_pressed(), i.pointer.interact_pos(), i.modifiers));
    if let (true, Some(pos)) = (pointer.0, pointer.1)
        && rect.contains(pos)
        && !stripe.contains(pos)
        // Not a click on a window above the editor (Settings, dialogs).
        && ui.ctx().layer_id_at(pos).is_none_or(|l| l == ui.layer_id())
    {
        ui.memory_mut(|m| m.request_focus(id));
        if ann_w > 0.0 && pos.x < gutter.left() {
            // An annotation: show its commit in the Git log.
            let row = (((pos.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize;
            if let Some((hash, _)) = map.line(row).and_then(|l| vcs.annotation(l)) {
                commands.push(("git.showGitLog", json!({})));
                commands.push(("vcs.selectCommit", json!({ "hash": hash })));
            }
        } else if gutter.contains(pos) {
            let row = (((pos.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize;
            if let Some(line) = map.line(row) {
                if pos.x >= gutter.right() - 6.0 && vcs.has_change(line) {
                    // A change marker: its popup (old content, rollback, diff).
                    st.vcs.hunk_anchor = Some(pos);
                    commands.push(("vcs.showHunk", json!({ "line": line })));
                } else if pos.x > gutter.right() - 18.0 && fold_starts.contains(&line) {
                    commands.push(("code.folding.toggleFolding", json!({ "line": line })));
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
        } else if let Some(h) =
            inlay_at(pos, &rows).filter(|h| h.navigable && (pointer.2.mac_cmd || pointer.2.command))
        {
            // Cmd+click on an inlay hint: its label part's target (or the type's declaration).
            commands.push(("editor.click", json!({ "offset": h.offset })));
            commands.push(("editor.inlayHint.navigate", json!({ "offset": h.offset, "part": h.part })));
        } else {
            let offset = hit(pos, &rows, ui);
            let m = pointer.2;
            // Alt+drag (or any drag in column selection mode) selects a box.
            if (m.alt && !m.shift || column_mode) && !(m.mac_cmd || m.command) {
                let p = box_point(pos, &rows, ui);
                ui.data_mut(|d| d.insert_temp(box_key, p));
                commands.push(("editor.click", json!({ "offset": offset })));
            } else {
                ui.data_mut(|d| d.remove::<(usize, usize)>(box_key));
                commands.push((
                    "editor.click",
                    json!({ "offset": offset, "extend": m.shift && !m.alt, "addCaret": m.alt && m.shift }),
                ));
                if m.mac_cmd || m.command {
                    commands.push(("navigate.declarationOrUsages", json!({})));
                }
            }
            st.drag_anchor_active = true;
        }
    }
    if response.double_clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && text_rect.contains(pos)
    {
        commands.push(("editor.click", json!({ "offset": hit(pos, &rows, ui), "clicks": 2 })));
    }
    if response.triple_clicked()
        && let Some(pos) = response.interact_pointer_pos()
        && text_rect.contains(pos)
    {
        commands.push(("editor.click", json!({ "offset": hit(pos, &rows, ui), "clicks": 3 })));
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
        match ui.data(|d| d.get_temp::<(usize, usize)>(box_key)) {
            Some((line, column)) => {
                let (hl_, hc) = box_point(pos, &rows, ui);
                commands.push((
                    "editor.boxSelect",
                    json!({ "anchor": { "line": line, "column": column }, "head": { "line": hl_, "column": hc } }),
                ));
            }
            None => commands.push(("editor.drag", json!({ "offset": hit(pos, &rows, ui) }))),
        }
    }
    if response.drag_stopped() || !ui.input(|i| i.pointer.primary_down()) {
        st.drag_anchor_active = false;
    }

    // Inlay hint tooltip; Cmd shows which hints navigate.
    let mut response = response;
    let inlay_hover = response
        .hover_pos()
        .filter(|p| text_rect.contains(*p) && !ui.input(|i| i.pointer.any_down()))
        .and_then(|p| inlay_at(p, &rows));
    if let Some(h) = &inlay_hover {
        let hint = decor.inlays.iter().find(|x| x.offset == h.offset);
        let tip = hint.and_then(|x| {
            h.part.and_then(|p| x.parts.get(p)).and_then(|p| p.tooltip.clone()).or_else(|| x.tooltip.clone())
        });
        if h.navigable && ui.input(|i| i.modifiers.command) {
            text_painter.hline(h.x.0..=h.x.1, h.y + line_h - 2.0, Stroke::new(1.0, theme.link));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let text = match (tip, h.navigable) {
            (Some(t), true) => Some(format!("{t}\n\nCmd+click to navigate")),
            (Some(t), false) => Some(t),
            (None, true) => Some("Cmd+click to navigate".to_string()),
            (None, false) => None,
        };
        if let Some(t) = text {
            response = response.on_hover_text_at_pointer(t);
        }
    }
    // Diagnostic tooltip.
    if let Some(pos) = response.hover_pos()
        && inlay_hover.is_none()
        && text_rect.contains(pos)
        && !ui.input(|i| i.pointer.any_down())
    {
        let off = hit(pos, &rows, ui);
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
    if let Some(pos) = response.hover_pos()
        && ann_w > 0.0
        && pos.x < gutter.left()
        && let Some((_, tip)) = map
            .line((((pos.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize)
            .and_then(|l| vcs.annotation(l))
    {
        response = response.on_hover_text_at_pointer(tip);
    }
    // Right-click on a ▶ run icon: Run / Debug / all tests in the file (M13 track RC).
    let mut run_menu = None;
    if let Some(pos) = response.hover_pos()
        && response.secondary_clicked()
        && gutter.contains(pos)
        && pos.x < gutter.left() + 18.0
        && let Some(line) = map.line((((pos.y - rect.top()) / line_h) + scroll).floor().max(0.0) as usize)
        && run_lines.contains(&line)
    {
        run_menu = Some((pos, line));
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
    s.editing.set_wrap_width(eid, wrap.map(|w| w.width).unwrap_or(0));
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
    crate::rc_ui::gutter_run_menu(ui, s, st, path.as_deref(), run_menu);
    if crumbs_shown {
        crate::navbar::breadcrumbs(ui, s, st, eid, crumb_rect);
    }
    crate::navbar::overlay(ui, s, st, eid, rect);
}

fn s_set_page(st: &mut UiState, rows: usize) {
    st.page_lines = rows.saturating_sub(2).max(1);
}
