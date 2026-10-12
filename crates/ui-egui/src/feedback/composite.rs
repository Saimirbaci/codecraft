//! Software compositing: draw the annotations onto the clean screenshot (tiny-skia for strokes and
//! fills, ab_glyph with egui's built-in font for text) and encode PNGs. Runs on a worker thread.

use std::io::Cursor;
use std::sync::OnceLock;

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use egui::ColorImage;
use tiny_skia::{LineCap, LineJoin, Paint, PathBuilder, Pixmap, Transform};

use codecraft_engine::feedback::{Capture, Shape, parse_color};

use super::{HIGHLIGHT_ALPHA, arrow_barbs};

/// Composite `shapes` (normalized) onto `image`; `window` is the logical window size the shapes
/// were drawn in (stroke widths and font sizes scale by image pixels per point).
pub fn composite(image: &ColorImage, shapes: &[Shape], window: [f32; 2]) -> Result<Pixmap, String> {
    let [w, h] = image.size;
    let (wu, hu) = (u32::try_from(w).map_err(|e| e.to_string())?, u32::try_from(h).map_err(|e| e.to_string())?);
    let mut pm = Pixmap::new(wu, hu).ok_or_else(|| format!("bad image size {w}x{h}"))?;
    for (dst, c) in pm.pixels_mut().iter_mut().zip(&image.pixels) {
        // Both are premultiplied RGBA.
        if let Some(px) = tiny_skia::PremultipliedColorU8::from_rgba(c.r(), c.g(), c.b(), c.a()) {
            *dst = px;
        }
    }
    let ppp = if window[0] > 0.0 { w as f32 / window[0] } else { 1.0 };
    let ppp = if ppp.is_finite() && ppp > 0.0 { ppp.min(8.0) } else { 1.0 };
    for shape in shapes {
        draw(&mut pm, &shape.denormalized([w as f32, h as f32]), ppp);
    }
    Ok(pm)
}

fn paint(hex: &str, alpha: u8) -> Paint<'static> {
    let [r, g, b, a] = parse_color(hex).unwrap_or([0xef, 0x44, 0x44, 255]);
    let mut p = Paint::default();
    p.set_color_rgba8(r, g, b, ((u16::from(a) * u16::from(alpha)) / 255) as u8);
    p.anti_alias = true;
    p
}

fn stroke(width: f32) -> tiny_skia::Stroke {
    tiny_skia::Stroke {
        width: width.max(0.5),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

fn polyline(points: &[[f32; 2]]) -> Option<tiny_skia::Path> {
    let (first, rest) = points.split_first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first[0], first[1]);
    for q in rest {
        pb.line_to(q[0], q[1]);
    }
    pb.finish()
}

/// Draw one shape in pixel coordinates.
fn draw(pm: &mut Pixmap, shape: &Shape, ppp: f32) {
    let id = Transform::identity();
    match shape {
        Shape::Path { points, color, width } => {
            if let [one] = points.as_slice() {
                if let Some(c) = PathBuilder::from_circle(one[0], one[1], width * ppp / 2.0) {
                    pm.fill_path(&c, &paint(color, 255), tiny_skia::FillRule::Winding, id, None);
                }
            } else if let Some(path) = polyline(points) {
                pm.stroke_path(&path, &paint(color, 255), &stroke(width * ppp), id, None);
            }
        }
        Shape::Line { from, to, color, width } => {
            if let Some(path) = polyline(&[*from, *to]) {
                pm.stroke_path(&path, &paint(color, 255), &stroke(width * ppp), id, None);
            }
        }
        Shape::Arrow { from, to, color, width } => {
            let (p, s) = (paint(color, 255), stroke(width * ppp));
            if let Some(path) = polyline(&[*from, *to]) {
                pm.stroke_path(&path, &p, &s, id, None);
            }
            if let Some((a, b)) = arrow_barbs(*from, *to, width * ppp)
                && let Some(head) = polyline(&[a, *to, b])
            {
                pm.stroke_path(&head, &p, &s, id, None);
            }
        }
        Shape::Rect { x, y, w, h, color, width } => {
            if let Some(path) = tiny_skia::Rect::from_xywh(*x, *y, *w, *h).map(PathBuilder::from_rect) {
                pm.stroke_path(&path, &paint(color, 255), &stroke(width * ppp), id, None);
            }
        }
        Shape::Ellipse { x, y, w, h, color, width } => {
            if let Some(path) = tiny_skia::Rect::from_xywh(*x, *y, *w, *h).and_then(PathBuilder::from_oval) {
                pm.stroke_path(&path, &paint(color, 255), &stroke(width * ppp), id, None);
            }
        }
        Shape::Highlight { x, y, w, h, color } => {
            if let Some(r) = tiny_skia::Rect::from_xywh(*x, *y, *w, *h) {
                pm.fill_rect(r, &paint(color, HIGHLIGHT_ALPHA), id, None);
            }
        }
        Shape::Text { x, y, text, color, font_size } => draw_text(pm, *x, *y, text, color, font_size * ppp),
    }
}

/// egui's built-in proportional font (the same one the canvas falls back to).
fn font() -> Option<&'static FontArc> {
    static FONT: OnceLock<Option<FontArc>> = OnceLock::new();
    FONT.get_or_init(|| {
        let defs = egui::FontDefinitions::default();
        let names = defs.families.get(&egui::FontFamily::Proportional)?;
        names.iter().find_map(|n| defs.font_data.get(n).and_then(|d| FontArc::try_from_vec(d.font.to_vec()).ok()))
    })
    .as_ref()
}

/// Draw text with its top-left at (x, y) (like the canvas' `Align2::LEFT_TOP`).
fn draw_text(pm: &mut Pixmap, x: f32, y: f32, text: &str, color: &str, size: f32) {
    let Some(font) = font() else { return };
    let [r, g, b, a] = parse_color(color).unwrap_or([0xef, 0x44, 0x44, 255]);
    let size = size.clamp(1.0, 1000.0);
    let scaled = font.as_scaled(PxScale::from(size));
    let (w, h) = (pm.width() as i64, pm.height() as i64);
    let stride = pm.width() as usize;
    let data = pm.data_mut();
    let mut baseline = y + scaled.ascent();
    for line in text.lines() {
        let mut caret = x;
        let mut prev = None;
        for ch in line.chars() {
            let gid = scaled.glyph_id(ch);
            if let Some(p) = prev {
                caret += scaled.kern(p, gid);
            }
            prev = Some(gid);
            let glyph = gid.with_scale_and_position(size, ab_glyph::point(caret, baseline));
            caret += scaled.h_advance(gid);
            let Some(outline) = font.outline_glyph(glyph) else { continue };
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let px = bounds.min.x as i64 + i64::from(gx);
                let py = bounds.min.y as i64 + i64::from(gy);
                if px < 0 || py < 0 || px >= w || py >= h {
                    return;
                }
                let i = (py as usize * stride + px as usize) * 4;
                let Some(dst) = data.get_mut(i..i + 4) else { return };
                let sa = (f32::from(a) / 255.0) * cov.clamp(0.0, 1.0);
                for (k, src) in [r, g, b].into_iter().enumerate() {
                    if let Some(d) = dst.get_mut(k) {
                        *d = (f32::from(src) * sa + f32::from(*d) * (1.0 - sa)).round().clamp(0.0, 255.0) as u8;
                    }
                }
                if let Some(d) = dst.get_mut(3) {
                    *d = (255.0 * sa + f32::from(*d) * (1.0 - sa)).round().clamp(0.0, 255.0) as u8;
                }
            });
        }
        baseline += scaled.height() + scaled.line_gap();
    }
}

fn encode_png(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    let img = image::RgbaImage::from_raw(width, height, rgba).ok_or("image buffer has the wrong size")?;
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// Encode the clean screenshot and the annotated composite as PNGs.
pub fn encode_capture(image: &ColorImage, shapes: &[Shape], window: [f32; 2]) -> Result<Capture, String> {
    let annotated = composite(image, shapes, window)?;
    let (width, height) = (annotated.width(), annotated.height());
    let clean: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    let ann: Vec<u8> = annotated
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Ok(Capture {
        screenshot_png: encode_png(width, height, clean)?,
        annotated_png: encode_png(width, height, ann)?,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gray(w: usize, h: usize) -> ColorImage {
        ColorImage::new([w, h], vec![egui::Color32::from_rgb(40, 40, 40); w * h])
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let c = pm.pixel(x, y).map(|p| p.demultiply()).unwrap();
        [c.red(), c.green(), c.blue(), c.alpha()]
    }

    #[test]
    fn annotations_change_only_the_annotated_region() {
        // A 2x (Retina) image of a 200x100 pt window.
        let img = gray(400, 200);
        let rect = Shape::Rect { x: 0.25, y: 0.25, w: 0.5, h: 0.5, color: "#ef4444".into(), width: 3.0 };
        let hl = Shape::Highlight { x: 0.8, y: 0.1, w: 0.1, h: 0.2, color: "#eab308".into() };
        let pm = composite(&img, &[rect, hl], [200.0, 100.0]).unwrap();
        // On the rect's left edge (x = 100 px): red.
        let edge = px(&pm, 100, 100);
        assert!(edge[0] > 180 && edge[1] < 120, "{edge:?}");
        // Inside the rect and far away: untouched.
        assert_eq!(px(&pm, 200, 100), [40, 40, 40, 255]);
        assert_eq!(px(&pm, 5, 5), [40, 40, 40, 255]);
        // The highlight tints but stays translucent.
        let tint = px(&pm, 340, 40);
        assert!(tint[0] > 40 && tint[0] < 234, "{tint:?}");
    }

    #[test]
    fn text_and_strokes_render_and_png_round_trips() {
        let img = gray(300, 150);
        let shapes = vec![
            Shape::Text { x: 0.1, y: 0.1, text: "Bug".into(), color: "#ffffff".into(), font_size: 24.0 },
            Shape::Arrow { from: [0.5, 0.8], to: [0.9, 0.5], color: "#3b82f6".into(), width: 4.0 },
            Shape::Path { points: vec![[0.1, 0.9]], color: "#22c55e".into(), width: 6.0 },
        ];
        let pm = composite(&img, &shapes, [300.0, 150.0]).unwrap();
        let changed_in = |x0: u32, y0: u32, x1: u32, y1: u32| {
            (x0..x1)
                .flat_map(|x| (y0..y1).map(move |y| (x, y)))
                .filter(|(x, y)| px(&pm, *x, *y) != [40, 40, 40, 255])
                .count()
        };
        assert!(font().is_some());
        assert!(changed_in(30, 15, 90, 50) > 20, "text drew nothing");
        assert!(changed_in(150, 75, 270, 120) > 50, "arrow drew nothing");
        assert_eq!(changed_in(200, 0, 300, 40), 0);

        let cap = encode_capture(&img, &shapes, [300.0, 150.0]).unwrap();
        assert_eq!((cap.width, cap.height), (300, 150));
        let clean = image::load_from_memory(&cap.screenshot_png).unwrap().to_rgba8();
        let ann = image::load_from_memory(&cap.annotated_png).unwrap().to_rgba8();
        assert_eq!(clean.dimensions(), (300, 150));
        assert_eq!(clean.get_pixel(40, 30).0, [40, 40, 40, 255]);
        assert_ne!(clean.as_raw(), ann.as_raw());
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        let img = gray(10, 10);
        let shapes = vec![
            Shape::Rect { x: 0.5, y: 0.5, w: 0.0, h: 0.0, color: "#ef4444".into(), width: 3.0 },
            Shape::Path { points: vec![], color: "nope".into(), width: 3.0 },
            Shape::Text { x: 2.0, y: -1.0, text: "far away".into(), color: "#fff".into(), font_size: 999.0 },
            Shape::Ellipse { x: f32::NAN, y: 0.0, w: 1.0, h: 1.0, color: "#ef4444".into(), width: 3.0 },
        ];
        assert!(composite(&img, &shapes, [0.0, 0.0]).is_ok());
        assert!(composite(&gray(0, 0), &shapes, [10.0, 10.0]).is_err());
    }
}
