//! Text painted with characters that none of the installed fonts has (egui draws them as an
//! empty box, "tofu"). Shared by the QA galleries and layout tests (`#[path]`-included).
//!
//! `Fonts::has_glyph` can't be used: it says "no" for every character of the font that also
//! provides the replacement glyph (egui's emoji fonts), so a glyph counts as missing when it was
//! drawn from the same place in the font atlas as the replacement character `◻`.

use egui::epaint::{ClippedShape, Shape};
use egui::{Color32, FontId};
use egui_kittest::Harness;

/// Each painted text that has characters drawn as the replacement box: (the text, the missing
/// characters).
#[allow(dead_code)]
pub fn missing_glyphs<S>(h: &Harness<'_, S>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let ctx = h.ctx.clone();
    let mut boxes: Vec<(FontId, [u16; 2])> = Vec::new();
    let mut box_of = |font: &FontId| -> Option<[u16; 2]> {
        if let Some((_, uv)) = boxes.iter().find(|(f, _)| f == font) {
            return Some(*uv);
        }
        let g = ctx.fonts_mut(|f| f.layout_no_wrap("◻".into(), font.clone(), Color32::WHITE));
        let uv = g.rows.first()?.row.glyphs.first()?.uv_rect.min;
        boxes.push((font.clone(), uv));
        Some(uv)
    };
    let mut visit = |s: &Shape| {
        let Shape::Text(t) = s else { return };
        let fonts: Vec<FontId> = t.galley.job.sections.iter().map(|s| s.format.font_id.clone()).collect();
        let replacement: Vec<[u16; 2]> = fonts.iter().filter_map(&mut box_of).collect();
        let mut missing = String::new();
        for row in &t.galley.rows {
            for g in &row.row.glyphs {
                if g.chr != '◻'
                    && !g.chr.is_whitespace()
                    && g.uv_rect.size.x > 0.0
                    && replacement.contains(&g.uv_rect.min)
                {
                    missing.push(g.chr);
                }
            }
        }
        let text = t.galley.job.text.clone();
        if !missing.is_empty() && !out.iter().any(|(t, _)| *t == text) {
            out.push((text, missing));
        }
    };
    for ClippedShape { shape, .. } in &h.output().shapes {
        match shape {
            Shape::Vec(v) => v.iter().for_each(&mut visit),
            s => visit(s),
        }
    }
    out
}
