//! The ER diagram window (Diagrams › Show Visualization, track DA). Paints the engine's
//! [`Diagram`] (model and layout come from the engine): tables with their columns and PK/FK
//! markers, foreign key edges, zoom (pinch or Ctrl/Cmd+wheel) and pan (wheel or dragging the
//! background), dragging tables, click to select (the related tables stay highlighted),
//! double-click to open a table's data, and export as PNG (drawn here on a worker thread) or SVG.

use std::sync::{Arc, Mutex, PoisonError};

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::database::schema_tools::{
    DIAGRAM_HEADER, DIAGRAM_MAX_COLUMNS, DIAGRAM_ROW, Diagram, DiagramNode, ToolState, edge_points,
};

use crate::theme::Theme;
use crate::{UiState, run_command};

type Cmds = Vec<(&'static str, Value)>;

/// What a drag on the canvas moves.
#[derive(Clone, Debug, PartialEq)]
enum Drag {
    Table(String),
    View,
}

fn drag_id() -> egui::Id {
    egui::Id::new("db-diagram-drag")
}

fn exports_id() -> egui::Id {
    egui::Id::new("db-diagram-exports")
}

/// Messages of finished PNG exports, shared with the export threads.
type Exports = Arc<Mutex<Vec<String>>>;

fn exports(ctx: &egui::Context) -> Exports {
    ctx.data_mut(|d| d.get_temp_mut_or_insert_with(exports_id(), Exports::default).clone())
}

/// Draw the diagram window when a diagram is open (and serve PNG export requests).
pub fn show(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    for msg in exports(ctx).lock().unwrap_or_else(PoisonError::into_inner).drain(..) {
        s.status = msg;
    }
    if let Some(path) = s.db.schema.png_request.take() {
        start_png(ctx, s, st, path);
    }
    if s.db.schema.diagram.is_none() {
        return;
    }
    let mut cmds: Cmds = Vec::new();
    let mut open = true;
    let title = s.db.schema.diagram.as_ref().map(|d| d.title.clone()).unwrap_or_default();
    egui::Window::new(format!("Diagram: {title}"))
        .id(egui::Id::new("db-diagram"))
        .collapsible(false)
        .resizable(true)
        .default_size(vec2(920.0, 600.0))
        .min_size(vec2(360.0, 240.0))
        .open(&mut open)
        .show(ctx, |ui| window(ui, s, st, &mut cmds));
    if !open {
        cmds.push(("db.diagram.close", Value::Null));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn window(ui: &mut egui::Ui, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let theme = &st.theme;
    let Some(d) = s.db.schema.diagram.as_mut() else { return };
    ui.horizontal(|ui| {
        let ready = d.state == ToolState::Ready;
        if ui.add_enabled(ready, egui::Button::new("−")).on_hover_text("Zoom out").clicked() {
            d.zoom = (d.zoom / 1.25).clamp(0.1, 4.0);
        }
        ui.label(RichText::new(format!("{:.0}%", d.zoom * 100.0)).color(theme.text_muted));
        if ui.add_enabled(ready, egui::Button::new("+")).on_hover_text("Zoom in").clicked() {
            d.zoom = (d.zoom * 1.25).clamp(0.1, 4.0);
        }
        if ui.add_enabled(ready, egui::Button::new("Fit")).on_hover_text("Show every table").clicked() {
            d.fit_pending = true;
        }
        if ui.add_enabled(ready, egui::Button::new("Layout")).on_hover_text("Arrange the tables again").clicked() {
            cmds.push(("db.diagram.layout", Value::Null));
        }
        ui.separator();
        for (label, ext) in [("Export PNG…", "png"), ("Export SVG…", "svg")] {
            if ui.add_enabled(ready, egui::Button::new(label)).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title("Export Diagram")
                    .set_file_name(format!("{}.{ext}", d.schema))
                    .add_filter(ext.to_ascii_uppercase(), &[ext])
                    .save_file()
            {
                cmds.push(("db.diagram.export", json!({ "path": path, "format": ext })));
            }
        }
        ui.separator();
        match &d.state {
            ToolState::Loading => {
                ui.spinner();
                ui.label(RichText::new("Reading the schema…").color(theme.text_muted));
            }
            ToolState::Failed(e) => {
                ui.label(RichText::new(e).color(theme.error));
            }
            _ => {
                let n = d.nodes.len();
                ui.label(RichText::new(format!("{n} tables · {} references", d.edges.len())).color(theme.text_muted));
            }
        }
    });
    let size = ui.available_size().max(vec2(200.0, 120.0));
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(2), theme.editor_bg);
    if d.state != ToolState::Ready {
        return;
    }
    if d.nodes.is_empty() {
        painter.text(rect.center(), Align2::CENTER_CENTER, "No tables", FontId::proportional(14.0), theme.text_muted);
        return;
    }
    if d.fit_pending {
        d.fit(rect.width(), rect.height());
    }
    let origin = rect.min.to_vec2();
    let to_screen = |zoom: f32, pan: [f32; 2], x: f32, y: f32| pos2(x * zoom + pan[0], y * zoom + pan[1]) + origin;

    // Input: zoom around the pointer, pan with the wheel or by dragging the background, drag tables.
    let pointer = resp.hover_pos();
    let to_diagram =
        |p: Pos2, zoom: f32, pan: [f32; 2]| ((p.x - origin.x - pan[0]) / zoom, (p.y - origin.y - pan[1]) / zoom);
    let hit = |d: &Diagram, p: Pos2| {
        let (x, y) = to_diagram(p, d.zoom, d.pan);
        d.nodes.iter().rposition(|n| n.contains(x, y))
    };
    if resp.hovered() {
        let (zoom_delta, scroll) = ui.input(|i| (i.zoom_delta(), i.smooth_scroll_delta));
        if (zoom_delta - 1.0).abs() > f32::EPSILON
            && let Some(p) = pointer
        {
            let z = (d.zoom * zoom_delta).clamp(0.1, 4.0);
            let k = z / d.zoom;
            let local = p - origin;
            d.pan = [local.x - (local.x - d.pan[0]) * k, local.y - (local.y - d.pan[1]) * k];
            d.zoom = z;
        } else if scroll != Vec2::ZERO {
            d.pan[0] += scroll.x;
            d.pan[1] += scroll.y;
        }
    }
    if resp.drag_started() {
        let drag = match resp.interact_pointer_pos().and_then(|p| hit(d, p)) {
            Some(i) => d.nodes.get(i).map(|n| Drag::Table(n.name.clone())).unwrap_or(Drag::View),
            None => Drag::View,
        };
        ui.ctx().data_mut(|m| m.insert_temp(drag_id(), drag));
    }
    if resp.dragged() {
        let delta = resp.drag_delta();
        match ui.ctx().data(|m| m.get_temp::<Drag>(drag_id())) {
            Some(Drag::Table(name)) => {
                if let Some(n) = d.nodes.iter_mut().find(|n| n.name == name) {
                    n.x += delta.x / d.zoom;
                    n.y += delta.y / d.zoom;
                }
            }
            _ => {
                d.pan[0] += delta.x;
                d.pan[1] += delta.y;
            }
        }
    }
    if resp.drag_stopped() {
        ui.ctx().data_mut(|m| m.remove::<Drag>(drag_id()));
    }
    if resp.clicked() {
        let picked =
            resp.interact_pointer_pos().and_then(|p| hit(d, p)).and_then(|i| d.nodes.get(i)).map(|n| n.name.clone());
        d.selected = picked;
    }
    if resp.double_clicked()
        && let Some(name) =
            resp.interact_pointer_pos().and_then(|p| hit(d, p)).and_then(|i| d.nodes.get(i)).map(|n| n.name.clone())
    {
        cmds.push(("db.diagram.openTable", json!({ "table": name })));
    }

    // Paint: edges under the tables.
    let related = d.selected.as_deref().map(|n| d.related(n)).unwrap_or_default();
    let focus = |name: &str| d.selected.is_none() || d.selected.as_deref() == Some(name) || related.contains(name);
    let (zoom, pan) = (d.zoom, d.pan);
    for e in &d.edges {
        let (Some(a), Some(b)) = (d.nodes.get(e.from), d.nodes.get(e.to)) else { continue };
        let lit = d.selected.as_deref().is_some_and(|s| s == a.name || s == b.name);
        let color = if lit {
            theme.accent
        } else if d.selected.is_some() {
            theme.text_disabled
        } else {
            theme.text_muted
        };
        let pts: Vec<Pos2> = edge_points(a, b, e).into_iter().map(|(x, y)| to_screen(zoom, pan, x, y)).collect();
        let width = if lit { 2.0 } else { 1.2 };
        painter.add(egui::Shape::line(pts.clone(), Stroke::new(width, color)));
        if let [.., p1, p2] = pts.as_slice() {
            let dir = (*p2 - *p1).normalized();
            let side = vec2(-dir.y, dir.x) * 4.0;
            let back = *p2 - dir * 9.0;
            painter.add(egui::Shape::convex_polygon(vec![*p2, back + side, back - side], color, Stroke::NONE));
        }
    }
    for n in &d.nodes {
        let r = Rect::from_min_max(to_screen(zoom, pan, n.x, n.y), to_screen(zoom, pan, n.x + n.width, n.y + n.height));
        if !r.intersects(rect) {
            continue;
        }
        let selected = d.selected.as_deref() == Some(n.name.as_str());
        let dim = !focus(&n.name);
        paint_node(&painter, theme, n, r, zoom, selected, related.contains(&n.name), dim);
    }
    if let Some(p) = pointer
        && let Some(n) = hit(d, p).and_then(|i| d.nodes.get(i))
    {
        let pk: Vec<&str> = n.columns.iter().filter(|c| c.primary_key).map(|c| c.name.as_str()).collect();
        resp.on_hover_text(format!(
            "{}\n{} columns{}\nDouble-click to open the data",
            n.name,
            n.columns.len(),
            if pk.is_empty() { String::new() } else { format!("; key ({})", pk.join(", ")) }
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_node(
    painter: &egui::Painter,
    theme: &Theme,
    n: &DiagramNode,
    r: Rect,
    zoom: f32,
    selected: bool,
    related: bool,
    dim: bool,
) {
    let fade = |c: Color32| if dim { c.gamma_multiply(0.35) } else { c };
    let radius = CornerRadius::same((4.0 * zoom).clamp(1.0, 8.0) as u8);
    painter.rect_filled(r, radius, fade(theme.panel_bg));
    let header = Rect::from_min_size(r.min, vec2(r.width(), DIAGRAM_HEADER * zoom));
    painter.rect_filled(header, radius, fade(theme.header_bg));
    let stroke = if selected {
        Stroke::new(2.0, theme.accent)
    } else if related {
        Stroke::new(1.5, theme.link)
    } else {
        Stroke::new(1.0, fade(theme.border))
    };
    painter.rect_stroke(r, radius, stroke, egui::StrokeKind::Inside);
    let title_size = 13.0 * zoom;
    if title_size >= 4.0 {
        painter.text(
            header.left_center() + vec2(8.0 * zoom, 0.0),
            Align2::LEFT_CENTER,
            &n.name,
            FontId::proportional(title_size),
            fade(theme.text),
        );
    }
    let size = 12.0 * zoom;
    if size < 5.0 {
        return;
    }
    let font = FontId::monospace(size);
    for (i, c) in n.columns.iter().take(DIAGRAM_MAX_COLUMNS).enumerate() {
        let y = r.top() + (DIAGRAM_HEADER + DIAGRAM_ROW * (i as f32 + 0.5)) * zoom;
        let (mark, color) = match (c.primary_key, c.foreign_key) {
            (true, _) => ("🔑", theme.warning),
            (false, true) => ("🔗", theme.link),
            _ => ("▪", theme.field),
        };
        let x = r.left() + 8.0 * zoom;
        painter.text(pos2(x, y), Align2::LEFT_CENTER, mark, FontId::proportional(size * 0.9), fade(color));
        let name_color = if c.nullable { theme.text } else { theme.text_editor };
        painter.text(pos2(x + 16.0 * zoom, y), Align2::LEFT_CENTER, &c.name, font.clone(), fade(name_color));
        painter.text(
            pos2(r.right() - 8.0 * zoom, y),
            Align2::RIGHT_CENTER,
            short_type(&c.data_type),
            font.clone(),
            fade(theme.text_muted),
        );
    }
    if n.columns.len() > DIAGRAM_MAX_COLUMNS {
        let y = r.top() + (DIAGRAM_HEADER + DIAGRAM_ROW * (DIAGRAM_MAX_COLUMNS as f32 + 0.5)) * zoom;
        painter.text(
            pos2(r.left() + 8.0 * zoom, y),
            Align2::LEFT_CENTER,
            format!("… {} more", n.columns.len() - DIAGRAM_MAX_COLUMNS),
            font,
            fade(theme.text_muted),
        );
    }
}

/// Long types are cut so they fit the box.
fn short_type(t: &str) -> String {
    if t.chars().count() > 28 { format!("{}…", t.chars().take(27).collect::<String>()) } else { t.to_string() }
}

// ---------------------------------------------------------------- PNG export

/// Colours of the exported image, from the theme.
#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    panel: Color32,
    header: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    edge: Color32,
    pk: Color32,
    fk: Color32,
}

impl Palette {
    fn of(t: &Theme) -> Self {
        Self {
            bg: t.editor_bg,
            panel: t.panel_bg,
            header: t.header_bg,
            border: t.border,
            text: t.text,
            muted: t.text_muted,
            edge: t.text_muted,
            pk: t.warning,
            fk: t.link,
        }
    }
}

fn start_png(ctx: &egui::Context, s: &mut Session, st: &UiState, path: std::path::PathBuf) {
    let Some(d) = s.db.schema.diagram.clone().filter(|d| d.state == ToolState::Ready) else {
        s.status = "No diagram to export".into();
        return;
    };
    let palette = Palette::of(&st.theme);
    let out = exports(ctx);
    let ctx2 = ctx.clone();
    let spawned = std::thread::Builder::new().name("db-diagram-png".into()).spawn(move || {
        let msg = match render_png(&d, palette).and_then(|png| {
            codecraft_fs::write_file_atomic(&path, &png).map_err(|e| format!("{}: {e}", path.display()))
        }) {
            Ok(()) => format!("Diagram saved to {}", path.display()),
            Err(e) => format!("Cannot export the diagram: {e}"),
        };
        out.lock().unwrap_or_else(PoisonError::into_inner).push(msg);
        ctx2.request_repaint();
    });
    s.status = match spawned {
        Ok(_) => "Exporting the diagram…".into(),
        Err(e) => format!("Cannot export the diagram: {e}"),
    };
}

/// Most pixels an exported image has (larger diagrams are scaled down).
const MAX_PIXELS: f32 = 48_000_000.0;

/// Draw the diagram into a PNG (tiny-skia for shapes, ab_glyph with egui's font for text).
fn render_png(d: &Diagram, c: Palette) -> Result<Vec<u8>, String> {
    use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};
    let [x0, y0, x1, y1] = d.bounds();
    let pad = 24.0;
    let (w, h) = ((x1 - x0) + 2.0 * pad, (y1 - y0) + 2.0 * pad);
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return Err("the diagram is empty".into());
    }
    let scale = (MAX_PIXELS / (w * h)).sqrt().clamp(0.05, 2.0);
    let (pw, ph) = ((w * scale).ceil() as u32, (h * scale).ceil() as u32);
    let mut pm = Pixmap::new(pw.max(1), ph.max(1)).ok_or_else(|| format!("cannot make a {pw}×{ph} image"))?;
    let rgba = |col: Color32| tiny_skia::Color::from_rgba8(col.r(), col.g(), col.b(), col.a());
    pm.fill(rgba(c.bg));
    let tf = |x: f32, y: f32| ((x - x0 + pad) * scale, (y - y0 + pad) * scale);
    let paint = |col: Color32| {
        let mut p = Paint::default();
        p.set_color(rgba(col));
        p.anti_alias = true;
        p
    };
    let id = Transform::identity();
    for e in &d.edges {
        let (Some(a), Some(b)) = (d.nodes.get(e.from), d.nodes.get(e.to)) else { continue };
        let pts: Vec<(f32, f32)> = edge_points(a, b, e).into_iter().map(|(x, y)| tf(x, y)).collect();
        let mut pb = PathBuilder::new();
        if let Some((first, rest)) = pts.split_first() {
            pb.move_to(first.0, first.1);
            for p in rest {
                pb.line_to(p.0, p.1);
            }
        }
        if let Some(path) = pb.finish() {
            let stroke = tiny_skia::Stroke { width: 1.2 * scale, ..Default::default() };
            pm.stroke_path(&path, &paint(c.edge), &stroke, id, None);
        }
        if let [.., p1, p2] = pts.as_slice() {
            let (dx, dy) = (p2.0 - p1.0, p2.1 - p1.1);
            let len = (dx * dx + dy * dy).sqrt().max(0.001);
            let (ux, uy) = (dx / len, dy / len);
            let (bx, by) = (p2.0 - ux * 9.0 * scale, p2.1 - uy * 9.0 * scale);
            let (sx, sy) = (-uy * 4.0 * scale, ux * 4.0 * scale);
            let mut pb = PathBuilder::new();
            pb.move_to(p2.0, p2.1);
            pb.line_to(bx + sx, by + sy);
            pb.line_to(bx - sx, by - sy);
            pb.close();
            if let Some(path) = pb.finish() {
                pm.fill_path(&path, &paint(c.edge), FillRule::Winding, id, None);
            }
        }
    }
    let mut texts: Vec<(f32, f32, String, Color32, f32, bool)> = Vec::new();
    for n in &d.nodes {
        let (x, y) = tf(n.x, n.y);
        let (w, h) = (n.width * scale, n.height * scale);
        let rect = |x: f32, y: f32, w: f32, h: f32| tiny_skia::Rect::from_xywh(x, y, w.max(1.0), h.max(1.0));
        if let Some(r) = rect(x, y, w, h) {
            pm.fill_rect(r, &paint(c.panel), id, None);
        }
        if let Some(r) = rect(x, y, w, DIAGRAM_HEADER * scale) {
            pm.fill_rect(r, &paint(c.header), id, None);
        }
        if let Some(r) = rect(x, y, w, h) {
            let path = PathBuilder::from_rect(r);
            let stroke = tiny_skia::Stroke { width: scale.max(1.0), ..Default::default() };
            pm.stroke_path(&path, &paint(c.border), &stroke, id, None);
        }
        texts.push((x + 8.0 * scale, y + 6.0 * scale, n.name.clone(), c.text, 13.0 * scale, false));
        for (i, col) in n.columns.iter().take(DIAGRAM_MAX_COLUMNS).enumerate() {
            let ry = y + (DIAGRAM_HEADER + DIAGRAM_ROW * i as f32 + 3.0) * scale;
            let (mark, mc) = match (col.primary_key, col.foreign_key) {
                (true, _) => ("PK", c.pk),
                (false, true) => ("FK", c.fk),
                _ => ("", c.muted),
            };
            texts.push((x + 8.0 * scale, ry, mark.into(), mc, 10.0 * scale, false));
            texts.push((x + 28.0 * scale, ry, col.name.clone(), c.text, 12.0 * scale, false));
            texts.push((x + w - 8.0 * scale, ry, short_type(&col.data_type), c.muted, 12.0 * scale, true));
        }
        if n.columns.len() > DIAGRAM_MAX_COLUMNS {
            let ry = y + (DIAGRAM_HEADER + DIAGRAM_ROW * DIAGRAM_MAX_COLUMNS as f32 + 3.0) * scale;
            let more = format!("… {} more", n.columns.len() - DIAGRAM_MAX_COLUMNS);
            texts.push((x + 8.0 * scale, ry, more, c.muted, 12.0 * scale, false));
        }
    }
    for (x, y, t, col, size, right) in texts {
        draw_text(&mut pm, x, y, &t, col, size, right);
    }
    let raw: Vec<u8> = pm
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    let img = image::RgbaImage::from_raw(pm.width(), pm.height(), raw).ok_or("image buffer has the wrong size")?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

/// egui's built-in proportional font.
fn font() -> Option<&'static ab_glyph::FontArc> {
    static FONT: std::sync::OnceLock<Option<ab_glyph::FontArc>> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        let defs = egui::FontDefinitions::default();
        let names = defs.families.get(&egui::FontFamily::Proportional)?;
        names
            .iter()
            .find_map(|n| defs.font_data.get(n).and_then(|d| ab_glyph::FontArc::try_from_vec(d.font.to_vec()).ok()))
    })
    .as_ref()
}

/// Text with its top-left (or top-right when `right`) at (x, y).
fn draw_text(pm: &mut tiny_skia::Pixmap, x: f32, y: f32, text: &str, color: Color32, size: f32, right: bool) {
    use ab_glyph::{Font, PxScale, ScaleFont};
    let Some(font) = font() else { return };
    let size = size.clamp(1.0, 400.0);
    let scaled = font.as_scaled(PxScale::from(size));
    let width: f32 = text.chars().map(|ch| scaled.h_advance(scaled.glyph_id(ch))).sum();
    let mut caret = if right { x - width } else { x };
    let baseline = y + scaled.ascent();
    let (w, h) = (i64::from(pm.width()), i64::from(pm.height()));
    let stride = pm.width() as usize;
    let data = pm.data_mut();
    let [r, g, b, a] = [color.r(), color.g(), color.b(), color.a()];
    for ch in text.chars() {
        let gid = scaled.glyph_id(ch);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_engine::database::schema_tools::{DiagramColumn, DiagramEdge};

    #[test]
    fn png_export_draws_tables_and_edges() {
        let col = |name: &str, pk: bool, fk: bool| DiagramColumn {
            name: name.into(),
            data_type: "integer".into(),
            primary_key: pk,
            foreign_key: fk,
            nullable: true,
        };
        let node = |name: &str, x: f32, cols: Vec<DiagramColumn>| DiagramNode {
            name: name.into(),
            x,
            y: 0.0,
            width: 160.0,
            height: DIAGRAM_HEADER + DIAGRAM_ROW * cols.len() as f32 + 6.0,
            columns: cols,
        };
        let mut d = Diagram::from_parts("main", Vec::new(), Vec::new());
        d.nodes = vec![
            node("a", 0.0, vec![col("id", true, false)]),
            node("b", 300.0, vec![col("id", true, false), col("a_id", false, true)]),
        ];
        d.edges = vec![DiagramEdge {
            from: 1,
            to: 0,
            name: "fk".into(),
            columns: vec!["a_id".into()],
            ref_columns: vec!["id".into()],
        }];
        let png = render_png(&d, Palette::of(&Theme::dark())).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
        let img = image::load_from_memory(&png).unwrap();
        assert!(img.width() > 400 && img.height() > 40);
        assert_eq!(short_type(&"x".repeat(40)).chars().count(), 28);
    }
}
