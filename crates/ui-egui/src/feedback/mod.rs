//! The feedback overlay: draw on top of CodeCraft, describe a change, and send it to a coding
//! agent (Evolvo-style). OWNED BY the feedback track.
//!
//! The draft (shapes, text, target, backend, mode) lives in the engine (`Session::feedback`) and
//! every action is a `feedback.*` command; this module keeps view state only: the current tool,
//! ink, the gesture in progress and the screenshot capture.
//!
//! Capture: Submit hides the overlay for one frame and asks for a `ViewportCommand::Screenshot`
//! of that clean frame. When the `Event::Screenshot` arrives, a worker thread composites the
//! annotations onto it (`composite.rs`) and encodes both PNGs; then `feedback.submit` runs.

mod canvas;
pub(crate) mod composite;
mod panel;

use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use egui::{Event, Id, Key, LayerId, Modifiers, Order, Rect, Sense};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::feedback::{Capture, DEFAULT_COLOR, TOGGLE_SHORTCUT};

use crate::keymap::Shortcut;
use crate::{UiState, run_command};

/// Drawing tools (Evolvo's set, minus crop and pasted images).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    Select,
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Highlight,
    Text,
    Eraser,
}

impl Tool {
    pub const ALL: [Tool; 9] = [
        Tool::Select,
        Tool::Pen,
        Tool::Line,
        Tool::Arrow,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Highlight,
        Tool::Text,
        Tool::Eraser,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Select => "Select / Move",
            Tool::Pen => "Pen",
            Tool::Line => "Line",
            Tool::Arrow => "Arrow",
            Tool::Rect => "Rectangle",
            Tool::Ellipse => "Ellipse",
            Tool::Highlight => "Highlight",
            Tool::Text => "Text",
            Tool::Eraser => "Eraser",
        }
    }

    pub fn key(self) -> Key {
        match self {
            Tool::Select => Key::V,
            Tool::Pen => Key::P,
            Tool::Line => Key::L,
            Tool::Arrow => Key::A,
            Tool::Rect => Key::R,
            Tool::Ellipse => Key::O,
            Tool::Highlight => Key::H,
            Tool::Text => Key::T,
            Tool::Eraser => Key::E,
        }
    }
}

/// Ink colours. These are annotation data (written into the record as hex), not UI chrome; the
/// first one is the theme's `annotation` token.
pub const PALETTE: [&str; 8] =
    [DEFAULT_COLOR, "#f97316", "#eab308", "#22c55e", "#3b82f6", "#a855f7", "#ffffff", "#111827"];

/// Stroke widths in points.
pub const WIDTHS: [f32; 4] = [2.0, 3.0, 5.0, 8.0];

/// Alpha of the Highlight tool's fill (shared by the canvas and the compositor).
pub const HIGHLIGHT_ALPHA: u8 = 0x59;

/// Font size of a Text annotation for a stroke width.
pub fn text_size(width: f32) -> f32 {
    14.0 + width * 2.0
}

/// A gesture in progress, in screen coordinates.
enum Drag {
    Draw { tool: Tool, points: Vec<egui::Pos2> },
    Move { index: usize, origin: egui::Pos2, current: egui::Pos2 },
    Erase,
}

/// A Text annotation being typed.
struct TextInput {
    pos: egui::Pos2,
    text: String,
    focus_pending: bool,
}

enum CapturePhase {
    Idle,
    /// Hide the overlay this frame and request the screenshot.
    Hide,
    Waiting(Instant),
    Encoding(Receiver<Result<Capture, String>>),
}

/// Tag passed with our screenshot request (so other screenshots are ignored).
struct ShotTag;

/// View state of the overlay; the draft itself is in the engine.
pub struct FeedbackUi {
    pub tool: Tool,
    pub color: String,
    pub width: f32,
    drag: Option<Drag>,
    selected: Option<usize>,
    text_input: Option<TextInput>,
    capture: CapturePhase,
    stashed_pointer: Option<egui::PointerState>,
    was_active: bool,
}

impl Default for FeedbackUi {
    fn default() -> Self {
        Self {
            tool: Tool::Rect,
            color: DEFAULT_COLOR.to_string(),
            width: 3.0,
            drag: None,
            selected: None,
            text_input: None,
            capture: CapturePhase::Idle,
            stashed_pointer: None,
            was_active: false,
        }
    }
}

impl FeedbackUi {
    /// Capture/encode in progress (the panel is disabled meanwhile).
    fn busy(&self) -> bool {
        !matches!(self.capture, CapturePhase::Idle)
    }
}

/// Keyboard and pointer routing while the overlay is shown; called first in the shell's input
/// handling. Returns true when the overlay took the frame's input (the shell then skips its own
/// shortcuts, popups and editor typing).
///
/// Escape and the toggle shortcut are consumed here so no other Escape handler sees them. The
/// pointer is hidden from the widgets painted below the overlay and restored in [`overlay`].
pub(crate) fn intercept_input(ctx: &egui::Context, s: &mut Session, st: &mut UiState) -> bool {
    if !s.feedback.active {
        return false;
    }
    let typing = ctx.memory(|m| m.focused().is_some());
    let events = ctx.input(|i| i.events.clone());
    let toggle = Shortcut::parse(TOGGLE_SHORTCUT);
    for ev in &events {
        let Event::Key { key, pressed: true, modifiers, .. } = ev else { continue };
        let (key, m) = (*key, *modifiers);
        if key == Key::Escape {
            ctx.input_mut(|i| i.consume_key(m, key));
            escape(s, st);
            continue;
        }
        if toggle.is_some_and(|t| t.matches(key, &m)) {
            ctx.input_mut(|i| i.consume_key(m, key));
            run_command(s, st, "feedback.toggle", &json!({ "open": false }));
            continue;
        }
        if typing || st.feedback.busy() {
            continue;
        }
        let fb = &mut st.feedback;
        if m.command && !m.alt && key == Key::Z {
            ctx.input_mut(|i| i.consume_key(m, key));
            let id = if m.shift { "feedback.redo" } else { "feedback.undo" };
            fb.selected = None;
            run_command(s, st, id, &Value::Null);
        } else if m.command && !m.alt && !m.shift && key == Key::Y {
            fb.selected = None;
            run_command(s, st, "feedback.redo", &Value::Null);
        } else if m.is_none() || m == Modifiers::SHIFT {
            if let Some(tool) = Tool::ALL.into_iter().find(|t| t.key() == key) {
                fb.tool = tool;
                fb.selected = None;
            } else if key == Key::OpenBracket || key == Key::CloseBracket {
                let i = WIDTHS.iter().position(|w| *w >= fb.width).unwrap_or(1);
                let i = if key == Key::OpenBracket { i.saturating_sub(1) } else { (i + 1).min(WIDTHS.len() - 1) };
                fb.width = WIDTHS.get(i).copied().unwrap_or(3.0);
            } else if (key == Key::Backspace || key == Key::Delete)
                && let Some(index) = fb.selected.take()
            {
                run_command(s, st, "feedback.removeShape", &json!({ "index": index }));
            }
        }
    }
    st.feedback.stashed_pointer = Some(ctx.input_mut(|i| std::mem::take(&mut i.pointer)));
    true
}

/// Escape: cancel the innermost thing (text being typed, a gesture, a selection), else close.
fn escape(s: &mut Session, st: &mut UiState) {
    let fb = &mut st.feedback;
    if fb.text_input.take().is_some() || fb.drag.take().is_some() || fb.selected.take().is_some() {
        return;
    }
    if fb.busy() {
        return;
    }
    run_command(s, st, "feedback.toggle", &json!({ "open": false }));
}

/// Draw the overlay when active (no-op otherwise). Called last in the frame.
pub fn overlay(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    if let Some(p) = st.feedback.stashed_pointer.take() {
        ctx.input_mut(|i| i.pointer = p);
    }
    poll_capture(&ctx, s, st);
    if !s.feedback.active {
        if st.feedback.was_active {
            let fb = &mut st.feedback;
            fb.was_active = false;
            fb.drag = None;
            fb.selected = None;
            fb.text_input = None;
            st.focus_editor = true;
        }
        if s.feedback.history_open {
            panel::history(&ctx, s, st, false);
        }
        return;
    }
    if !st.feedback.was_active {
        st.feedback.was_active = true;
        ctx.memory_mut(|m| m.stop_text_input());
    }
    let screen = ctx.viewport_rect();
    s.feedback.window = [screen.width(), screen.height()];
    match st.feedback.capture {
        CapturePhase::Hide => {
            blocker(&ctx, screen);
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(ShotTag)));
            st.feedback.capture = CapturePhase::Waiting(Instant::now());
            ctx.request_repaint();
            return;
        }
        CapturePhase::Waiting(_) => {
            blocker(&ctx, screen);
            ctx.request_repaint_after(Duration::from_millis(30));
            return;
        }
        CapturePhase::Idle | CapturePhase::Encoding(_) => {}
    }
    canvas::show(&ctx, s, st, screen);
    panel::show(&ctx, s, st, screen);
    if s.feedback.history_open {
        panel::history(&ctx, s, st, true);
    }
}

/// Swallow pointer input without painting (the frame being captured must look clean).
fn blocker(ctx: &egui::Context, screen: Rect) {
    let id = Id::new("codecraft-feedback-canvas");
    egui::Area::new(id).order(Order::Foreground).fixed_pos(screen.min).constrain(false).show(ctx, |ui| {
        ui.allocate_rect(Rect::from_min_size(screen.min, screen.size()), Sense::click_and_drag());
    });
    ctx.move_to_top(LayerId::new(Order::Foreground, id));
}

/// Start a submission: capture first (unless the draft is invalid).
fn begin_submit(s: &mut Session, st: &mut UiState) {
    if st.feedback.busy() {
        return;
    }
    if s.feedback.draft.text.trim().is_empty() {
        s.status = "Describe the change first".into();
        return;
    }
    st.feedback.text_input = None;
    st.feedback.drag = None;
    st.feedback.selected = None;
    s.feedback.capture = None;
    st.feedback.capture = CapturePhase::Hide;
}

fn finish_submit(s: &mut Session, st: &mut UiState, capture: Option<Capture>) {
    st.feedback.capture = CapturePhase::Idle;
    s.feedback.capture = capture;
    run_command(s, st, "feedback.submit", &json!({ "confirm": true }));
}

fn poll_capture(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    match &st.feedback.capture {
        CapturePhase::Waiting(since) => {
            let shot = ctx.input(|i| {
                i.events.iter().find_map(|e| match e {
                    Event::Screenshot { user_data, image, .. }
                        if user_data.data.as_ref().is_some_and(|d| d.downcast_ref::<ShotTag>().is_some()) =>
                    {
                        Some(Arc::clone(image))
                    }
                    _ => None,
                })
            });
            if let Some(image) = shot {
                let shapes = s.feedback.draft.shapes.clone();
                let window = s.feedback.window;
                let (tx, rx) = std::sync::mpsc::channel();
                let spawned = std::thread::Builder::new().name("feedback-capture".into()).spawn(move || {
                    let _ = tx.send(composite::encode_capture(&image, &shapes, window));
                });
                match spawned {
                    Ok(_) => st.feedback.capture = CapturePhase::Encoding(rx),
                    Err(e) => {
                        log::warn!("feedback: cannot start the capture thread: {e}");
                        finish_submit(s, st, None);
                    }
                }
                ctx.request_repaint();
            } else if since.elapsed() > Duration::from_secs(3) {
                log::warn!("feedback: no screenshot arrived; submitting without images");
                finish_submit(s, st, None);
            }
        }
        CapturePhase::Encoding(rx) => match rx.try_recv() {
            Ok(Ok(capture)) => finish_submit(s, st, Some(capture)),
            Ok(Err(e)) => {
                log::warn!("feedback: screenshot failed: {e}");
                finish_submit(s, st, None);
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(30)),
            Err(TryRecvError::Disconnected) => finish_submit(s, st, None),
        },
        CapturePhase::Idle | CapturePhase::Hide => {}
    }
}

/// Parse a shape colour for painting (`alpha` multiplies the colour's own alpha).
pub(crate) fn color32(hex: &str, alpha: u8) -> egui::Color32 {
    let [r, g, b, a] = codecraft_engine::feedback::parse_color(hex).unwrap_or([0xef, 0x44, 0x44, 255]);
    let a = ((u16::from(a) * u16::from(alpha)) / 255) as u8;
    egui::Color32::from_rgba_unmultiplied(r, g, b, a)
}

/// The two barb endpoints of an arrow head at `to` (shared by the canvas and the compositor).
pub(crate) fn arrow_barbs(from: [f32; 2], to: [f32; 2], width: f32) -> Option<([f32; 2], [f32; 2])> {
    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len < 0.5 {
        return None;
    }
    let head = (width * 4.0).max(12.0).min(len * 0.6);
    let (ux, uy) = (dx / len, dy / len);
    let (c, s) = (0.5_f32.cos(), 0.5_f32.sin());
    let rot = |sin: f32| [to[0] - head * (ux * c - uy * sin), to[1] - head * (uy * c + ux * sin)];
    Some((rot(s), rot(-s)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_shortcut_is_bound_once() {
        let sc = Shortcut::parse(TOGGLE_SHORTCUT).unwrap();
        let hits: Vec<_> = crate::keymap::bindings().into_iter().filter(|b| b.shortcut == sc).collect();
        assert_eq!(hits.len(), 1, "{:?}", hits.iter().map(|b| b.id).collect::<Vec<_>>());
        assert_eq!(hits[0].id, "feedback.toggle");
        for item in codecraft_engine::catalog::CATALOG {
            if let Some(c) = item.shortcut.and_then(Shortcut::parse) {
                assert_ne!(c, sc, "catalog item {} uses the feedback shortcut", item.id);
            }
        }
    }

    #[test]
    fn arrow_head_points_back() {
        let (a, b) = arrow_barbs([0.0, 0.0], [100.0, 0.0], 3.0).unwrap();
        assert!(a[0] < 100.0 && b[0] < 100.0 && (a[1] + b[1]).abs() < 1e-3);
        assert!(arrow_barbs([1.0, 1.0], [1.0, 1.0], 3.0).is_none());
        assert_eq!(color32("#ff0000", 255), egui::Color32::from_rgb(255, 0, 0));
    }
}
