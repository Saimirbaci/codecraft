//! Floating windows (dialogs, settings, feedback) are resizable and remember their size across restarts.
//!
//! egui keeps a window's size in its area state for the session only, so the sizes are mirrored into
//! [`UiState::window_sizes`] (persisted by `PersistedState`) and fed back as the window's default size.

use egui::{Context, Id, Vec2, Window};

use crate::UiState;

/// Smallest/largest remembered size; anything else in the state file is ignored.
const MIN: f32 = 120.0;
const MAX: f32 = 8000.0;

/// Every floating window that remembers its size: `(key in the state file, egui id)`.
pub(crate) const REMEMBERED: &[(&str, &str)] = &[
    ("settings", "settings-window"),
    ("about", "about-window"),
    ("vcs-dialog", "vcs-dialog"),
    ("vcs-compare", "vcs-compare"),
    ("db-datasource", "db-datasource-dialog"),
    ("feedback", "codecraft-feedback-panel"),
    ("feedback-history", "codecraft-feedback-history"),
];

fn valid(v: f32) -> bool {
    v.is_finite() && (MIN..=MAX).contains(&v)
}

/// Make `w` resizable, starting from the remembered size of window `key` when there is one.
pub(crate) fn resizable<'a>(w: Window<'a>, st: &UiState, key: &str) -> Window<'a> {
    let w = w.resizable(true);
    match st.window_sizes.get(key) {
        Some([x, y]) if valid(*x) && valid(*y) => w.default_size(Vec2::new(*x, *y)),
        _ => w,
    }
}

/// Record the current size of every remembered window that has been shown this session.
pub(crate) fn capture(ctx: &Context, st: &mut UiState) {
    for (key, id) in REMEMBERED {
        let size = egui::AreaState::load(ctx, Id::new(*id)).and_then(|a| a.size);
        if let Some(size) = size
            && valid(size.x)
            && valid(size.y)
        {
            st.window_sizes.insert((*key).to_string(), [size.x, size.y]);
        }
    }
}
