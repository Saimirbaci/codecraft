//! The drawing surface: paints the draft's shapes over the whole window and turns pointer
//! gestures into `feedback.*` commands.

use egui::{
    Align2, Color32, CornerRadius, FontId, Id, LayerId, Order, Painter, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2,
    pos2, vec2,
};
use serde_json::json;

use codecraft_engine::Session;
use codecraft_engine::feedback::Shape;

use super::{Drag, HIGHLIGHT_ALPHA, TextInput, Tool, arrow_barbs, color32, text_size};
use crate::{UiState, run_command};

const CANVAS: &str = "codecraft-feedback-canvas";
const TEXT_INPUT: &str = "codecraft-feedback-text";

fn p(a: [f32; 2]) -> Pos2 {
    pos2(a[0], a[1])
}

/// A normalized shape in screen coordinates.
fn on_screen(shape: &Shape, screen: Rect) -> Shape {
    shape.denormalized([screen.width(), screen.height()]).translated([screen.min.x, screen.min.y])
}

/// Paint one shape given in screen coordinates.
pub(crate) fn paint_shape(painter: &Painter, shape: &Shape) {
    match shape {
        Shape::Path { points, color, width } => {
            let stroke = Stroke::new(*width, color32(color, 255));
            match points.as_slice() {
                [one] => {
                    painter.circle_filled(p(*one), width / 2.0, stroke.color);
                }
                pts => {
                    painter.add(egui::Shape::line(pts.iter().map(|q| p(*q)).collect(), stroke));
                }
            }
        }
        Shape::Line { from, to, color, width } => {
            painter.line_segment([p(*from), p(*to)], Stroke::new(*width, color32(color, 255)));
        }
        Shape::Arrow { from, to, color, width } => {
            let stroke = Stroke::new(*width, color32(color, 255));
            painter.line_segment([p(*from), p(*to)], stroke);
            if let Some((a, b)) = arrow_barbs(*from, *to, *width) {
                painter.add(egui::Shape::line(vec![p(a), p(*to), p(b)], stroke));
            }
        }
        Shape::Rect { x, y, w, h, color, width } => {
            let r = Rect::from_min_size(pos2(*x, *y), vec2(*w, *h));
            painter.rect_stroke(r, CornerRadius::ZERO, Stroke::new(*width, color32(color, 255)), StrokeKind::Middle);
        }
        Shape::Ellipse { x, y, w, h, color, width } => {
            let r = Rect::from_min_size(pos2(*x, *y), vec2(*w, *h));
            painter.add(egui::Shape::ellipse_stroke(
                r.center(),
                r.size() / 2.0,
                Stroke::new(*width, color32(color, 255)),
            ));
        }
        Shape::Highlight { x, y, w, h, color } => {
            let r = Rect::from_min_size(pos2(*x, *y), vec2(*w, *h));
            painter.rect_filled(r, CornerRadius::ZERO, color32(color, HIGHLIGHT_ALPHA));
        }
        Shape::Text { x, y, text, color, font_size } => {
            painter.text(pos2(*x, *y), Align2::LEFT_TOP, text, FontId::proportional(*font_size), color32(color, 255));
        }
    }
}

/// Screen-space bounds (text is measured).
fn bounds(painter: &Painter, shape: &Shape) -> Rect {
    if let Shape::Text { x, y, text, font_size, .. } = shape {
        let g = painter.layout_no_wrap(text.clone(), FontId::proportional(*font_size), Color32::WHITE);
        return Rect::from_min_size(pos2(*x, *y), g.size());
    }
    let b = shape.bounds();
    Rect::from_min_max(pos2(b[0], b[1]), pos2(b[2], b[3]))
}

fn segment_distance(q: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    if len2 <= f32::EPSILON {
        return q.distance(a);
    }
    let t = ((q - a).dot(ab) / len2).clamp(0.0, 1.0);
    q.distance(a + ab * t)
}

fn hits(painter: &Painter, shape: &Shape, q: Pos2, tol: f32) -> bool {
    match shape {
        Shape::Path { points, width, .. } => {
            let tol = tol + width / 2.0;
            match points.as_slice() {
                [one] => q.distance(p(*one)) <= tol,
                pts => pts.windows(2).any(|w| matches!(w, [a, b] if segment_distance(q, p(*a), p(*b)) <= tol)),
            }
        }
        Shape::Line { from, to, width, .. } | Shape::Arrow { from, to, width, .. } => {
            segment_distance(q, p(*from), p(*to)) <= tol + width / 2.0
        }
        _ => bounds(painter, shape).expand(tol).contains(q),
    }
}

/// Topmost shape under `q` (screen coordinates).
fn pick(painter: &Painter, shapes: &[Shape], screen: Rect, q: Pos2) -> Option<usize> {
    shapes.iter().enumerate().rev().find(|(_, s)| hits(painter, &on_screen(s, screen), q, 6.0)).map(|(i, _)| i)
}

fn add_shape(s: &mut Session, st: &mut UiState, shape: Shape, screen: Rect) {
    let n = shape.translated([-screen.min.x, -screen.min.y]).normalized([screen.width(), screen.height()]);
    match serde_json::to_value(&n) {
        Ok(v) => run_command(s, st, "feedback.addShape", &json!({ "shape": v })),
        Err(e) => log::warn!("feedback: {e}"),
    }
}

/// The shape a drawing gesture produces (screen coordinates), if it is big enough.
fn gesture_shape(tool: Tool, points: &[Pos2], color: &str, width: f32) -> Option<Shape> {
    let (first, last) = (*points.first()?, *points.last()?);
    let color = color.to_string();
    let r = Rect::from_two_pos(first, last);
    let big = r.width() >= 3.0 && r.height() >= 3.0;
    let long = first.distance(last) >= 3.0;
    let (from, to) = ([first.x, first.y], [last.x, last.y]);
    match tool {
        Tool::Pen if points.len() >= 2 => {
            Some(Shape::Path { points: points.iter().map(|q| [q.x, q.y]).collect(), color, width })
        }
        Tool::Line if long => Some(Shape::Line { from, to, color, width }),
        Tool::Arrow if long => Some(Shape::Arrow { from, to, color, width }),
        Tool::Rect if big => Some(Shape::Rect { x: r.min.x, y: r.min.y, w: r.width(), h: r.height(), color, width }),
        Tool::Ellipse if big => {
            Some(Shape::Ellipse { x: r.min.x, y: r.min.y, w: r.width(), h: r.height(), color, width })
        }
        Tool::Highlight if big => Some(Shape::Highlight { x: r.min.x, y: r.min.y, w: r.width(), h: r.height(), color }),
        _ => None,
    }
}

fn erase_at(painter: &Painter, s: &mut Session, st: &mut UiState, screen: Rect, q: Pos2) {
    if let Some(index) = pick(painter, &s.feedback.draft.shapes, screen, q) {
        st.feedback.selected = None;
        run_command(s, st, "feedback.removeShape", &json!({ "index": index }));
    }
}

fn commit_text(s: &mut Session, st: &mut UiState, screen: Rect) {
    let Some(t) = st.feedback.text_input.take() else { return };
    if t.text.trim().is_empty() {
        return;
    }
    let shape = Shape::Text {
        x: t.pos.x,
        y: t.pos.y,
        text: t.text,
        color: st.feedback.color.clone(),
        font_size: text_size(st.feedback.width),
    };
    add_shape(s, st, shape, screen);
}

/// The full-window drawing surface (drawn above everything except the panel).
pub(super) fn show(ctx: &egui::Context, s: &mut Session, st: &mut UiState, screen: Rect) {
    let id = Id::new(CANVAS);
    egui::Area::new(id).order(Order::Foreground).fixed_pos(screen.min).constrain(false).show(ctx, |ui| {
        let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let theme = &st.theme;
        // A frame shows the mode; the hint says how to leave it.
        painter.rect_stroke(
            rect.shrink(1.0),
            CornerRadius::ZERO,
            Stroke::new(2.0, theme.annotation),
            StrokeKind::Inside,
        );
        let hint = format!(
            "Drawing feedback: {} · Esc to close",
            crate::keymap::display(codecraft_engine::feedback::TOGGLE_SHORTCUT)
        );
        let g = painter.layout_no_wrap(hint, FontId::proportional(12.0), theme.text);
        let tag = Rect::from_min_size(
            pos2(rect.center().x - g.size().x / 2.0 - 8.0, rect.top() + 2.0),
            g.size() + vec2(16.0, 6.0),
        );
        painter.rect_filled(tag, CornerRadius::same(4), theme.annotation);
        painter.galley(tag.min + vec2(8.0, 3.0), g, theme.text);

        // Shapes (the one being moved follows the pointer).
        let moving = match &st.feedback.drag {
            Some(Drag::Move { index, origin, current }) => Some((*index, *current - *origin)),
            _ => None,
        };
        for (i, shape) in s.feedback.draft.shapes.iter().enumerate() {
            let mut sh = on_screen(shape, screen);
            if let Some((mi, d)) = moving
                && mi == i
            {
                sh = sh.translated([d.x, d.y]);
            }
            paint_shape(&painter, &sh);
            if st.feedback.selected == Some(i) {
                let b = bounds(&painter, &sh).expand(4.0);
                painter.rect_stroke(b, CornerRadius::same(2), Stroke::new(1.0, theme.accent), StrokeKind::Outside);
            }
        }
        // The gesture in progress.
        if let Some(Drag::Draw { tool, points }) = &st.feedback.drag
            && let Some(sh) = gesture_shape(*tool, points, &st.feedback.color, st.feedback.width)
        {
            paint_shape(&painter, &sh);
        }

        handle_pointer(ui, &painter, &resp, s, st, screen);
        if st.feedback.tool != Tool::Select && resp.hovered() {
            ui.ctx().set_cursor_icon(match st.feedback.tool {
                Tool::Text => egui::CursorIcon::Text,
                Tool::Eraser => egui::CursorIcon::Cell,
                _ => egui::CursorIcon::Crosshair,
            });
        }
    });
    ctx.move_to_top(LayerId::new(Order::Foreground, id));
    text_input(ctx, s, st, screen);
}

fn handle_pointer(
    ui: &egui::Ui,
    painter: &Painter,
    resp: &egui::Response,
    s: &mut Session,
    st: &mut UiState,
    screen: Rect,
) {
    let pos = resp.interact_pointer_pos();
    let tool = st.feedback.tool;
    if resp.drag_started() {
        let origin = ui.input(|i| i.pointer.press_origin()).or(pos).unwrap_or(screen.center());
        commit_text(s, st, screen);
        match tool {
            Tool::Select => {
                let hit = pick(painter, &s.feedback.draft.shapes, screen, origin);
                st.feedback.selected = hit;
                st.feedback.drag = hit.map(|index| Drag::Move { index, origin, current: origin });
            }
            Tool::Eraser => {
                erase_at(painter, s, st, screen, origin);
                st.feedback.drag = Some(Drag::Erase);
            }
            Tool::Text => {}
            _ => st.feedback.drag = Some(Drag::Draw { tool, points: vec![origin] }),
        }
    }
    if resp.dragged()
        && let Some(q) = pos
    {
        let mut erase = false;
        match &mut st.feedback.drag {
            Some(Drag::Draw { tool: Tool::Pen, points }) => {
                if points.last().is_none_or(|l| l.distance(q) >= 1.5) && points.len() < 10_000 {
                    points.push(q);
                }
            }
            Some(Drag::Draw { points, .. }) => {
                points.truncate(1);
                points.push(q);
            }
            Some(Drag::Move { current, .. }) => *current = q,
            Some(Drag::Erase) => erase = true,
            None => {}
        }
        if erase {
            erase_at(painter, s, st, screen, q);
        }
    }
    if resp.drag_stopped() {
        match st.feedback.drag.take() {
            Some(Drag::Draw { tool, mut points }) => {
                if let Some(q) = pos
                    && tool != Tool::Pen
                {
                    points.truncate(1);
                    points.push(q);
                }
                if let Some(sh) = gesture_shape(tool, &points, &st.feedback.color.clone(), st.feedback.width) {
                    add_shape(s, st, sh, screen);
                }
            }
            Some(Drag::Move { index, origin, current }) => {
                let d: Vec2 = current - origin;
                if d.length() > 0.5 {
                    let (dx, dy) = (d.x / screen.width().max(1.0), d.y / screen.height().max(1.0));
                    run_command(s, st, "feedback.moveShape", &json!({ "index": index, "dx": dx, "dy": dy }));
                }
            }
            Some(Drag::Erase) | None => {}
        }
    }
    if resp.clicked()
        && let Some(q) = pos
    {
        match tool {
            Tool::Text => {
                commit_text(s, st, screen);
                st.feedback.text_input = Some(TextInput { pos: q, text: String::new(), focus_pending: true });
            }
            Tool::Select => st.feedback.selected = pick(painter, &s.feedback.draft.shapes, screen, q),
            Tool::Eraser => erase_at(painter, s, st, screen, q),
            _ => commit_text(s, st, screen),
        }
    }
}

/// The text box of a Text annotation being typed (Enter or clicking away commits).
fn text_input(ctx: &egui::Context, s: &mut Session, st: &mut UiState, screen: Rect) {
    let Some(t) = &mut st.feedback.text_input else { return };
    let id = Id::new(TEXT_INPUT);
    let size = text_size(st.feedback.width);
    let color = color32(&st.feedback.color, 255);
    let mut done = false;
    egui::Area::new(id).order(Order::Foreground).fixed_pos(t.pos).show(ctx, |ui| {
        let edit = egui::TextEdit::singleline(&mut t.text)
            .font(FontId::proportional(size))
            .text_color(color)
            .hint_text("Type, then Enter")
            .desired_width(240.0);
        let r = ui.add(edit);
        if t.focus_pending {
            r.request_focus();
            t.focus_pending = false;
        } else if r.lost_focus() {
            done = true;
        }
    });
    ctx.move_to_top(LayerId::new(Order::Foreground, id));
    if done {
        commit_text(s, st, screen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gestures_make_shapes_only_when_big_enough() {
        let pts = [pos2(10.0, 10.0), pos2(11.0, 11.0)];
        assert!(gesture_shape(Tool::Rect, &pts, "#ef4444", 3.0).is_none());
        let pts = [pos2(50.0, 40.0), pos2(10.0, 10.0)];
        let Some(Shape::Rect { x, y, w, h, .. }) = gesture_shape(Tool::Rect, &pts, "#ef4444", 3.0) else { panic!() };
        assert_eq!((x, y, w, h), (10.0, 10.0, 40.0, 30.0));
        assert!(matches!(gesture_shape(Tool::Arrow, &pts, "#ef4444", 3.0), Some(Shape::Arrow { .. })));
        assert!(gesture_shape(Tool::Text, &pts, "#ef4444", 3.0).is_none());
        assert!((segment_distance(pos2(5.0, 5.0), pos2(0.0, 0.0), pos2(10.0, 0.0)) - 5.0).abs() < 1e-4);
    }
}
