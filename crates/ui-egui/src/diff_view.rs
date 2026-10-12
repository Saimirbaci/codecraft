//! A reusable diff widget: unified or side-by-side, with line numbers, hunk headers and
//! word-level emphasis. Used by agent reviews and approval cards (and later by VCS).
//!
//! The diff itself comes from `codecraft_agent::diff` (the `similar` crate) and is cached in egui
//! memory per widget id, so it is computed once per content change, not every frame. Rows are
//! virtualized (`show_rows`), so large files stay cheap to draw.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId, Id, Rect, ScrollArea, Sense, Ui, pos2, vec2};

use codecraft_engine::agent::diff::{self, DiffLine, DiffStats, Hunk, LineTag};

use crate::theme::Theme;

#[derive(Clone, Copy, Debug)]
pub struct DiffOptions {
    pub side_by_side: bool,
    /// Unchanged lines around each change.
    pub context: usize,
    /// Height of the scroll area (rows beyond it scroll).
    pub max_height: f32,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self { side_by_side: false, context: 3, max_height: 400.0 }
    }
}

struct Cached {
    key: u64,
    hunks: Vec<Hunk>,
    stats: DiffStats,
}

fn content_key(old: &str, new: &str, context: usize) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    old.hash(&mut h);
    new.hash(&mut h);
    context.hash(&mut h);
    h.finish()
}

/// The diff for `old` → `new`, cached under `id`.
fn hunks(ui: &Ui, id: Id, old: &str, new: &str, context: usize) -> Arc<Cached> {
    let key = content_key(old, new, context);
    if let Some(c) = ui.ctx().data(|d| d.get_temp::<Arc<Cached>>(id)).filter(|c| c.key == key) {
        return c;
    }
    let hunks = diff::diff(old, new, context);
    let stats = diff::stats(&hunks);
    let c = Arc::new(Cached { key, hunks, stats });
    ui.ctx().data_mut(|d| d.insert_temp(id, Arc::clone(&c)));
    c
}

/// `+added −removed` for a pair of texts (cached like the diff).
pub fn stats(ui: &Ui, id_salt: impl Hash + std::fmt::Debug, old: &str, new: &str) -> DiffStats {
    let id = Id::new(("diff-view", id_salt));
    hunks(ui, id, old, new, 3).stats.clone()
}

/// A small `+n −m` label.
pub fn stats_label(ui: &mut Ui, theme: &Theme, stats: &DiffStats) {
    ui.label(egui::RichText::new(format!("+{}", stats.added)).color(theme.diff_added).small());
    ui.label(egui::RichText::new(format!("−{}", stats.removed)).color(theme.diff_removed).small());
}

enum Row<'a> {
    Header(String),
    Line(&'a DiffLine),
    Pair(Option<&'a DiffLine>, Option<&'a DiffLine>),
}

/// Draw the diff of `old` → `new`.
pub fn show(
    ui: &mut Ui,
    theme: &Theme,
    font: &FontId,
    id_salt: impl Hash + std::fmt::Debug,
    old: &str,
    new: &str,
    opts: &DiffOptions,
) {
    let id = Id::new(("diff-view", id_salt));
    let cached = hunks(ui, id, old, new, opts.context);
    if cached.hunks.is_empty() {
        ui.label(egui::RichText::new("No differences.").color(theme.text_muted));
        return;
    }
    let mut rows: Vec<Row<'_>> = Vec::new();
    for h in &cached.hunks {
        rows.push(Row::Header(h.header()));
        if opts.side_by_side {
            rows.extend(diff::side_by_side(h).into_iter().map(|r| Row::Pair(r.left, r.right)));
        } else {
            rows.extend(h.lines.iter().map(Row::Line));
        }
    }
    let row_h = ui.fonts_mut(|f| f.row_height(font)) + 2.0;
    let digits = cached
        .hunks
        .last()
        .map(|h| (h.old_start + h.old_len).max(h.new_start + h.new_len))
        .unwrap_or(1)
        .max(1)
        .to_string()
        .len();
    let char_w = ui.fonts_mut(|f| f.glyph_width(font, '0'));
    let gutter = char_w * digits as f32 + 8.0;
    egui::Frame::new().fill(theme.editor_bg).corner_radius(4).show(ui, |ui| {
        ScrollArea::vertical()
            .id_salt(id.with("scroll"))
            .max_height(opts.max_height)
            .auto_shrink([false, true])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for row in rows.get(range).unwrap_or_default() {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::hover());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    match row {
                        Row::Header(text) => {
                            ui.painter().rect_filled(rect, 0.0, theme.hover);
                            paint_text(ui, rect.left() + 4.0, rect, text, font, theme.text_muted);
                        }
                        Row::Line(line) => {
                            let nums = [line.old, line.new];
                            paint_line(ui, theme, font, rect, &nums, gutter, line, true);
                        }
                        Row::Pair(left, right) => {
                            let half = rect.width() / 2.0;
                            let l = Rect::from_min_size(rect.min, vec2(half - 1.0, rect.height()));
                            let r = Rect::from_min_size(
                                pos2(rect.left() + half + 1.0, rect.top()),
                                vec2(half - 1.0, rect.height()),
                            );
                            ui.painter().vline(
                                rect.left() + half,
                                rect.y_range(),
                                egui::Stroke::new(1.0, theme.separator),
                            );
                            if let Some(line) = left {
                                paint_line(ui, theme, font, l, &[line.old], gutter, line, false);
                            }
                            if let Some(line) = right {
                                paint_line(ui, theme, font, r, &[line.new], gutter, line, false);
                            }
                        }
                    }
                }
            });
    });
}

fn paint_text(ui: &Ui, x: f32, rect: Rect, text: &str, font: &FontId, color: Color32) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), font.clone(), color);
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter().with_clip_rect(rect).galley(pos2(x, y), galley, color);
}

#[allow(clippy::too_many_arguments)]
fn paint_line(
    ui: &Ui,
    theme: &Theme,
    font: &FontId,
    rect: Rect,
    numbers: &[Option<usize>],
    gutter: f32,
    line: &DiffLine,
    sign: bool,
) {
    let (bg, fg, mark) = match line.tag {
        LineTag::Equal => (None, theme.text_editor, ' '),
        LineTag::Insert => (Some(theme.diff_added_bg), theme.text_editor, '+'),
        LineTag::Delete => (Some(theme.diff_removed_bg), theme.text_editor, '−'),
    };
    if let Some(bg) = bg {
        ui.painter().rect_filled(rect, 0.0, bg);
    }
    let mut x = rect.left() + 2.0;
    for n in numbers {
        let text = n.map(|n| n.to_string()).unwrap_or_default();
        let galley = ui.painter().layout_no_wrap(text, font.clone(), theme.line_number);
        let y = rect.center().y - galley.size().y / 2.0;
        ui.painter().galley(pos2(x + gutter - 6.0 - galley.size().x, y), galley, theme.line_number);
        x += gutter;
    }
    if sign {
        let color = match line.tag {
            LineTag::Insert => theme.diff_added,
            LineTag::Delete => theme.diff_removed,
            LineTag::Equal => theme.text_muted,
        };
        paint_text(ui, x, rect, &mark.to_string(), font, color);
    }
    x += ui.fonts_mut(|f| f.glyph_width(font, '0')) * 1.5;
    let mut job = LayoutJob::default();
    let emph_bg = match line.tag {
        LineTag::Insert => theme.diff_added.gamma_multiply(0.35),
        LineTag::Delete => theme.diff_removed.gamma_multiply(0.35),
        LineTag::Equal => Color32::TRANSPARENT,
    };
    let fmt = |background: Color32| TextFormat { font_id: font.clone(), color: fg, background, ..Default::default() };
    if line.segments.is_empty() {
        job.append(&expand_tabs(&line.text), 0.0, fmt(Color32::TRANSPARENT));
    } else {
        for (emph, seg) in &line.segments {
            job.append(&expand_tabs(seg), 0.0, fmt(if *emph { emph_bg } else { Color32::TRANSPARENT }));
        }
    }
    let galley = ui.painter().layout_job(job);
    let y = rect.center().y - galley.size().y / 2.0;
    let clip = Rect::from_min_max(pos2(x, rect.top()), rect.max);
    ui.painter().with_clip_rect(clip).galley(pos2(x, y), galley, fg);
}

fn expand_tabs(s: &str) -> String {
    if s.contains('\t') { s.replace('\t', "    ") } else { s.to_string() }
}
