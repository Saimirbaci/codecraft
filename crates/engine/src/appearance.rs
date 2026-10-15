//! View › Appearance: Presentation, Distraction Free, Full Screen and Zen modes, Compact Mode,
//! Zoom IDE, and the visibility of the toolbar, navigation bar, tool window bars, status bar and
//! each status bar widget.
//!
//! The persistent switches live in [`Settings`]; the modes belong to one window's session (like
//! RustRover's frame). [`chrome`] turns both into what the shell shows, and [`checked`] gives the
//! menus their checkmarks, so agents and both menu bars read the same state.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::{DocId, NavigationBar, Settings};

use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, spec};
use crate::params::bool_or;
use crate::structure::{self, Extent};
use crate::{Effect, Session};

/// Zoom IDE limits and step.
pub const ZOOM_MIN: f32 = 0.5;
pub const ZOOM_MAX: f32 = 3.0;
pub const ZOOM_STEP: f32 = 0.1;

/// A status bar widget of View › Appearance › Status Bar Widgets: `(key, label, shown by
/// default, CodeCraft paints it)`. The key is the last part of the command id. Widgets
/// CodeCraft has no counterpart for (yet) are still toggles that persist, so a settings file
/// keeps working once they exist.
pub const WIDGETS: &[(&str, &str, bool, bool)] = &[
    ("statusText", "Status Text", true, true),
    ("fileSystemSync", "File System Sync", true, false),
    ("rustExternalLinter", "Rust External Linter", true, true),
    ("aggregator", "Aggregator", true, false),
    ("gridPosition", "Grid Position", true, false),
    ("lineColumnNumber", "Line:Column Number", true, true),
    ("languageServices", "Language Services", true, true),
    ("lineSeparator", "Line Separator", true, true),
    ("fileEncoding", "File Encoding", true, true),
    ("powerSaveMode", "Power Save Mode", true, true),
    ("editorSelectionMode", "Editor Selection Mode", true, true),
    ("indentation", "Indentation", true, true),
    ("jsonSchema", "JSON Schema", true, false),
    // The New UI shows the branch in the main toolbar, so the status bar widget starts hidden.
    ("gitBranch", "Git Branch", false, true),
    ("readOnlyAttribute", "Read-Only Attribute", true, true),
    ("rustResolveContext", "Rust Resolve Context", true, false),
    ("memoryIndicator", "Memory Indicator", false, true),
];

const WIDGET_PREFIX: &str = "view.appearance.statusBarWidgets.";

/// Whether a status bar widget is shown (the user's choice, else its default).
pub fn widget_visible(settings: &Settings, key: &str) -> bool {
    settings
        .status_bar_widgets
        .get(key)
        .copied()
        .unwrap_or_else(|| WIDGETS.iter().find(|w| w.0 == key).is_some_and(|w| w.2))
}

/// How often the Memory Indicator samples the process.
const MEMORY_EVERY: Duration = Duration::from_secs(5);

/// One window's appearance modes.
#[derive(Default)]
pub struct AppearanceState {
    pub presentation: bool,
    pub distraction_free: bool,
    pub zen: bool,
    /// The window is full screen (the shell reports the real state every frame).
    pub full_screen: bool,
    /// The Zoom IDE popup is open.
    pub zoom_popup: bool,
    /// Resident memory of this process (MiB), for the Memory Indicator.
    pub memory_mb: Option<u64>,
    /// Full screen before a mode turned it on (restored when the last such mode ends).
    full_screen_before: Option<bool>,
    /// Hidden tool windows before a mode hid them.
    tool_windows_before: Option<bool>,
    memory_rx: Option<Receiver<Option<u64>>>,
    memory_at: Option<Instant>,
    /// Structure extents of the active document for the navigation bar's members.
    nav_cache: Option<(DocId, u64, Vec<Extent>)>,
}

/// What the shell shows, from the settings and the modes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Chrome {
    pub toolbar: bool,
    pub status_bar: bool,
    /// The tool window bars (stripes).
    pub stripes: bool,
    pub editor_tabs: bool,
    /// The navigation bar above the editor.
    pub navigation_top: bool,
    /// The navigation path in the status bar.
    pub navigation_in_status_bar: bool,
    pub navigation_members: bool,
    pub editor_font_size: f32,
    pub compact: bool,
    pub zoom: f32,
}

/// The shell's chrome for this session.
pub fn chrome(s: &Session) -> Chrome {
    let set = &s.ws.settings;
    let a = &s.appearance;
    let bare = a.presentation || a.distraction_free;
    Chrome {
        toolbar: set.show_toolbar && !bare,
        status_bar: set.show_status_bar && !bare,
        stripes: set.show_tool_window_bars && !bare,
        editor_tabs: set.show_editor_tabs && !a.distraction_free,
        navigation_top: set.navigation_bar == NavigationBar::Top && !bare,
        navigation_in_status_bar: set.navigation_bar == NavigationBar::InStatusBar,
        navigation_members: set.navigation_bar_members,
        editor_font_size: if a.presentation { set.presentation_font_size } else { set.editor_font_size },
        compact: set.compact_mode,
        zoom: zoom_of(set),
    }
}

/// The UI zoom factor (sanitized: a hand-edited settings file may hold anything).
pub fn zoom_of(settings: &Settings) -> f32 {
    if settings.ide_zoom.is_finite() { settings.ide_zoom.clamp(ZOOM_MIN, ZOOM_MAX) } else { 1.0 }
}

/// Commands shown with a checkmark, and whether each is on. Radio items (Navigation Bar
/// placement) are checked when selected.
pub fn checked(s: &Session, id: &str) -> Option<bool> {
    // Soft-Wrap is per tab (View › Active Editor).
    if id == "view.toggleSoftWrap"
        && let Some(e) = s.ws.active_editor()
    {
        return Some(s.ws.soft_wrap_of(e));
    }
    checked_in(&s.ws.settings, &s.appearance, id)
}

fn checked_in(set: &Settings, a: &AppearanceState, id: &str) -> Option<bool> {
    if let Some(key) = id.strip_prefix(WIDGET_PREFIX) {
        return WIDGETS.iter().any(|w| w.0 == key).then(|| widget_visible(set, key));
    }
    Some(match id {
        "view.toggleSoftWrap" => set.soft_wrap,
        "view.toggleWhitespace" => set.show_whitespace,
        "view.toggleLineNumbers" => set.show_line_numbers,
        "file.powerSaveMode" => set.power_save_mode,
        "edit.find.scrollToResultsDuringTyping" => set.find_scroll_to_results,
        "window.processes.autoShow" => set.processes_auto_show,
        "view.appearance.enterPresentationMode" => a.presentation,
        "view.appearance.enterDistractionFreeMode" => a.distraction_free,
        "view.appearance.enterFullScreen" => a.full_screen,
        "view.appearance.enterZenMode" => a.zen,
        "view.appearance.compactMode" => set.compact_mode,
        "view.appearance.toolbar" => set.show_toolbar,
        "view.appearance.navigationBar.top" => set.navigation_bar == NavigationBar::Top,
        "view.appearance.navigationBar.inStatusBar" => set.navigation_bar == NavigationBar::InStatusBar,
        "view.appearance.navigationBar.dontShow" => set.navigation_bar == NavigationBar::Hidden,
        "view.appearance.navigationBar.showMembers" => set.navigation_bar_members,
        "view.appearance.toolWindowBars" => set.show_tool_window_bars,
        "view.appearance.statusBar" => set.show_status_bar,
        _ => return None,
    })
}

/// Whether a command is a checkmark toggle (independent of its current state).
pub fn is_toggle(id: &str) -> bool {
    checked_in(&Settings::default(), &AppearanceState::default(), id).is_some()
}

/// A label that follows the state (RustRover renames the mode items to "Exit …" while they are
/// on and shows the current zoom).
pub fn dynamic_label(s: &Session, id: &str) -> Option<String> {
    let a = &s.appearance;
    let exit = |on: bool, enter: &str, mode: &str| if on { format!("Exit {mode}") } else { enter.to_string() };
    Some(match id {
        "view.appearance.enterPresentationMode" => exit(a.presentation, "Enter Presentation Mode", "Presentation Mode"),
        "view.appearance.enterDistractionFreeMode" => {
            exit(a.distraction_free, "Enter Distraction Free Mode", "Distraction Free Mode")
        }
        "view.appearance.enterFullScreen" => exit(a.full_screen, "Enter Full Screen", "Full Screen"),
        "view.appearance.enterZenMode" => exit(a.zen, "Enter Zen Mode", "Zen Mode"),
        "view.appearance.zoomIde" => format!("Zoom IDE (Current: {}%)…", zoom_percent(&s.ws.settings)),
        _ => return None,
    })
}

/// The zoom as a whole percentage.
pub fn zoom_percent(settings: &Settings) -> u32 {
    (zoom_of(settings) * 100.0).round() as u32
}

// ---------------------------------------------------------------- navigation path

/// One element of the navigation path (the status bar's or the top bar's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavPathItem {
    pub label: String,
    /// The file or directory the element stands for.
    pub path: PathBuf,
    /// A member (symbol) element: the char offset of its name.
    pub offset: Option<usize>,
}

/// The navigation path of the active editor: project, directories, file and (with Show
/// Members) the items enclosing the caret.
pub fn navigation_path(s: &mut Session) -> Vec<NavPathItem> {
    let Some((e, d)) = s.ws.active() else { return Vec::new() };
    let Some(path) = d.path.clone() else { return Vec::new() };
    let (doc_id, revision, caret, language) = (d.id, d.doc.revision(), e.selections.primary().head, d.language);
    let mut out = Vec::new();
    match &s.ws.project {
        Some(p) if path.starts_with(&p.root) => {
            out.push(NavPathItem { label: p.name.clone(), path: p.root.clone(), offset: None });
            let mut dir = p.root.clone();
            if let Some(rel) = path.parent().and_then(|parent| parent.strip_prefix(&p.root).ok()) {
                for c in rel.components() {
                    dir = dir.join(c);
                    let label = c.as_os_str().to_string_lossy().into_owned();
                    out.push(NavPathItem { label, path: dir.clone(), offset: None });
                }
            }
        }
        _ => {}
    }
    out.push(NavPathItem { label: d.title(), path: path.clone(), offset: None });
    if !s.ws.settings.navigation_bar_members {
        return out;
    }
    let fresh = s.appearance.nav_cache.as_ref().is_some_and(|(id, rev, _)| *id == doc_id && *rev == revision);
    if !fresh {
        let text = s.ws.doc(doc_id).map(|d| d.doc.text()).unwrap_or_default();
        s.appearance.nav_cache = Some((doc_id, revision, structure::extents(language, &text)));
    }
    if let Some((_, _, extents)) = &s.appearance.nav_cache {
        for c in structure::crumbs_at(extents, caret) {
            out.push(NavPathItem { label: c.label(), path: path.clone(), offset: Some(c.offset) });
        }
    }
    out
}

// ---------------------------------------------------------------- modes

/// Hide the tool windows for a mode (remembering whether they were hidden).
fn hide_tool_windows(s: &mut Session) {
    if s.appearance.tool_windows_before.is_none() {
        s.appearance.tool_windows_before = Some(s.ws.layout.all_hidden);
    }
    s.ws.layout.all_hidden = true;
}

/// Bring the tool windows back once no mode hides them.
fn restore_tool_windows(s: &mut Session) {
    if s.appearance.presentation || s.appearance.distraction_free {
        return;
    }
    if let Some(before) = s.appearance.tool_windows_before.take() {
        s.ws.layout.all_hidden = before;
    }
}

fn set_full_screen(s: &mut Session, on: bool) {
    s.appearance.full_screen = on;
    s.effects.push(Effect::SetFullScreen(on));
}

/// Go full screen for a mode (remembering the window's state).
fn mode_full_screen(s: &mut Session) {
    if s.appearance.full_screen_before.is_none() {
        s.appearance.full_screen_before = Some(s.appearance.full_screen);
    }
    if !s.appearance.full_screen {
        set_full_screen(s, true);
    }
}

/// Restore the window once no mode needs full screen.
fn restore_full_screen(s: &mut Session) {
    if s.appearance.presentation || s.appearance.zen {
        return;
    }
    if let Some(before) = s.appearance.full_screen_before.take()
        && before != s.appearance.full_screen
    {
        set_full_screen(s, before);
    }
}

fn state(s: &Session) -> Value {
    let a = &s.appearance;
    json!({
        "presentation": a.presentation,
        "distractionFree": a.distraction_free,
        "zen": a.zen,
        "fullScreen": a.full_screen,
        "zoom": zoom_of(&s.ws.settings),
        "zoomPopup": a.zoom_popup,
        "chrome": chrome(s),
        "widgets": WIDGETS
            .iter()
            .map(|w| json!({ "key": w.0, "label": w.1, "visible": widget_visible(&s.ws.settings, w.0), "painted": w.3 }))
            .collect::<Vec<_>>(),
    })
}

fn presentation(s: &mut Session, p: &Value) -> CmdResult {
    let on = bool_or(p, "on", !s.appearance.presentation);
    if on != s.appearance.presentation {
        s.appearance.presentation = on;
        if on {
            hide_tool_windows(s);
            mode_full_screen(s);
        } else {
            restore_tool_windows(s);
            restore_full_screen(s);
        }
    }
    Ok(json!({ "on": on }))
}

fn distraction_free(s: &mut Session, p: &Value) -> CmdResult {
    let on = bool_or(p, "on", !s.appearance.distraction_free);
    if on != s.appearance.distraction_free {
        s.appearance.distraction_free = on;
        if on {
            hide_tool_windows(s);
        } else {
            // Leaving distraction-free mode also ends Zen mode.
            s.appearance.zen = false;
            restore_tool_windows(s);
            restore_full_screen(s);
        }
    }
    Ok(json!({ "on": on }))
}

fn zen(s: &mut Session, p: &Value) -> CmdResult {
    let on = bool_or(p, "on", !s.appearance.zen);
    if on != s.appearance.zen {
        s.appearance.zen = on;
        if on {
            s.appearance.distraction_free = true;
            hide_tool_windows(s);
            mode_full_screen(s);
        } else {
            s.appearance.distraction_free = false;
            restore_tool_windows(s);
            restore_full_screen(s);
        }
    }
    Ok(json!({ "on": on }))
}

fn full_screen(s: &mut Session, p: &Value) -> CmdResult {
    let on = bool_or(p, "on", !s.appearance.full_screen);
    // Choosing full screen by hand ends the modes' claim on it.
    s.appearance.full_screen_before = None;
    if on != s.appearance.full_screen {
        set_full_screen(s, on);
    }
    Ok(json!({ "on": on }))
}

// ---------------------------------------------------------------- zoom

fn set_zoom(s: &mut Session, zoom: f32) -> CmdResult {
    if !zoom.is_finite() {
        return Err(CommandError::BadParams("the zoom must be a number".into()));
    }
    // Whole percents, so repeated steps never drift.
    let zoom = ((zoom.clamp(ZOOM_MIN, ZOOM_MAX) * 100.0).round()) / 100.0;
    s.ws.settings.ide_zoom = zoom;
    s.status = format!("Zoom: {}%", zoom_percent(&s.ws.settings));
    Ok(json!({ "zoom": zoom, "percent": zoom_percent(&s.ws.settings) }))
}

fn zoom_ide(s: &mut Session, p: &Value) -> CmdResult {
    let number = |key: &str| p.get(key).and_then(Value::as_f64).map(|v| v as f32);
    if let Some(z) = number("zoom") {
        return set_zoom(s, z);
    }
    if let Some(pct) = number("percent") {
        return set_zoom(s, pct / 100.0);
    }
    if p.get("zoom").is_some_and(|v| !v.is_null()) || p.get("percent").is_some_and(|v| !v.is_null()) {
        return Err(CommandError::BadParams("`zoom`/`percent` must be numbers".into()));
    }
    s.appearance.zoom_popup = true;
    Ok(json!({ "zoom": zoom_of(&s.ws.settings), "percent": zoom_percent(&s.ws.settings) }))
}

// ---------------------------------------------------------------- toggles

/// Flip (or set, with `{on}`) a boolean setting.
fn toggle_setting(s: &mut Session, p: &Value, field: fn(&mut Settings) -> &mut bool) -> CmdResult {
    let current = *field(&mut s.ws.settings);
    let on = bool_or(p, "on", !current);
    *field(&mut s.ws.settings) = on;
    Ok(json!({ "on": on }))
}

fn toggle_widget(s: &mut Session, p: &Value, key: &str) -> CmdResult {
    let on = bool_or(p, "on", !widget_visible(&s.ws.settings, key));
    s.ws.settings.status_bar_widgets.insert(key.to_string(), on);
    if key == "memoryIndicator" && !on {
        s.appearance.memory_mb = None;
    }
    Ok(json!({ "on": on }))
}

fn navigation_bar(s: &mut Session, placement: NavigationBar) -> CmdResult {
    s.ws.settings.navigation_bar = placement;
    ok_on()
}

fn ok_on() -> CmdResult {
    Ok(json!({ "on": true }))
}

const TOGGLE: &str = "{on?: bool} — toggle, or set with `on`";

macro_rules! widget {
    ($key:literal, $label:literal) => {
        spec(
            concat!("view.appearance.statusBarWidgets.", $key),
            $label,
            None,
            TOGGLE,
            Context::Global,
            always,
            |s, p| toggle_widget(s, p, $key),
        )
    };
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "view.appearance.enterPresentationMode",
            "Enter Presentation Mode",
            None,
            "{on?: bool} — large editor font (`presentation_font_size`), no tool windows, bars or toolbar, full screen",
            Context::Global,
            always,
            presentation,
        ),
        spec(
            "view.appearance.enterDistractionFreeMode",
            "Enter Distraction Free Mode",
            None,
            "{on?: bool} — only the editor: no tool windows, bars, toolbar, tabs or navigation bar",
            Context::Global,
            always,
            distraction_free,
        ),
        spec(
            "view.appearance.enterFullScreen",
            "Enter Full Screen",
            None,
            TOGGLE,
            Context::Global,
            always,
            full_screen,
        ),
        spec(
            "view.appearance.enterZenMode",
            "Enter Zen Mode",
            None,
            "{on?: bool} — Distraction Free Mode in full screen",
            Context::Global,
            always,
            zen,
        ),
        spec("view.appearance.compactMode", "Compact Mode", None, TOGGLE, Context::Global, always, |s, p| {
            toggle_setting(s, p, |set| &mut set.compact_mode)
        }),
        spec(
            "view.appearance.zoomIde",
            "Zoom IDE…",
            None,
            "{zoom?: number (0.5–3.0) | percent?: number} — set the UI zoom; without one, open the Zoom IDE popup → {zoom, percent}",
            Context::Global,
            always,
            zoom_ide,
        ),
        spec("view.appearance.zoomIde.zoomIn", "Zoom In IDE", None, "", Context::Global, always, |s, _| {
            let z = zoom_of(&s.ws.settings) + ZOOM_STEP;
            set_zoom(s, z)
        }),
        spec("view.appearance.zoomIde.zoomOut", "Zoom Out IDE", None, "", Context::Global, always, |s, _| {
            let z = zoom_of(&s.ws.settings) - ZOOM_STEP;
            set_zoom(s, z)
        }),
        spec("view.appearance.zoomIde.reset", "Reset IDE Zoom", None, "", Context::Global, always, |s, _| {
            set_zoom(s, 1.0)
        }),
        spec("view.appearance.zoomIde.close", "Close Zoom IDE Popup", None, "", Context::Global, always, |s, _| {
            s.appearance.zoom_popup = false;
            Ok(Value::Null)
        }),
        spec("view.appearance.toolbar", "Toolbar", None, TOGGLE, Context::Global, always, |s, p| {
            toggle_setting(s, p, |set| &mut set.show_toolbar)
        }),
        spec("view.appearance.navigationBar.top", "Top", None, "", Context::Global, always, |s, _| {
            navigation_bar(s, NavigationBar::Top)
        }),
        spec(
            "view.appearance.navigationBar.inStatusBar",
            "In Status Bar",
            None,
            "",
            Context::Global,
            always,
            |s, _| navigation_bar(s, NavigationBar::InStatusBar),
        ),
        spec("view.appearance.navigationBar.dontShow", "Don't Show", None, "", Context::Global, always, |s, _| {
            navigation_bar(s, NavigationBar::Hidden)
        }),
        spec(
            "view.appearance.navigationBar.showMembers",
            "Show Members",
            None,
            TOGGLE,
            Context::Global,
            always,
            |s, p| toggle_setting(s, p, |set| &mut set.navigation_bar_members),
        ),
        spec("view.appearance.toolWindowBars", "Tool Window Bars", None, TOGGLE, Context::Global, always, |s, p| {
            toggle_setting(s, p, |set| &mut set.show_tool_window_bars)
        }),
        spec("view.appearance.statusBar", "Status Bar", None, TOGGLE, Context::Global, always, |s, p| {
            toggle_setting(s, p, |set| &mut set.show_status_bar)
        }),
        widget!("statusText", "Status Text"),
        widget!("fileSystemSync", "File System Sync"),
        widget!("rustExternalLinter", "Rust External Linter"),
        widget!("aggregator", "Aggregator"),
        widget!("gridPosition", "Grid Position"),
        widget!("lineColumnNumber", "Line:Column Number"),
        widget!("languageServices", "Language Services"),
        widget!("lineSeparator", "Line Separator"),
        widget!("fileEncoding", "File Encoding"),
        widget!("powerSaveMode", "Power Save Mode"),
        widget!("editorSelectionMode", "Editor Selection Mode"),
        widget!("indentation", "Indentation"),
        widget!("jsonSchema", "JSON Schema"),
        widget!("gitBranch", "Git Branch"),
        widget!("readOnlyAttribute", "Read-Only Attribute"),
        widget!("rustResolveContext", "Rust Resolve Context"),
        widget!("memoryIndicator", "Memory Indicator"),
        spec(
            "view.appearance.state",
            "Appearance State",
            None,
            "→ {presentation, distractionFree, zen, fullScreen, zoom, zoomPopup, chrome, widgets: [{key, label, visible, painted}]}",
            Context::Global,
            always,
            |s, _| Ok(state(s)),
        ),
    ]
}

// ---------------------------------------------------------------- memory indicator

/// Resident memory of this process in MiB (`ps`, so no unsafe code; `None` where it fails).
fn own_rss_mb() -> Option<u64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let kib: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kib / 1024)
}

/// Sample the Memory Indicator on a worker thread while it is shown. Returns true when the value
/// changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    if let Some(rx) = &s.appearance.memory_rx {
        match rx.try_recv() {
            Ok(mb) => {
                changed = mb != s.appearance.memory_mb;
                s.appearance.memory_mb = mb;
                s.appearance.memory_rx = None;
            }
            Err(TryRecvError::Disconnected) => s.appearance.memory_rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }
    let shown = s.ws.settings.show_status_bar && widget_visible(&s.ws.settings, "memoryIndicator");
    let due = s.appearance.memory_at.is_none_or(|t| t.elapsed() >= MEMORY_EVERY);
    if shown && due && s.appearance.memory_rx.is_none() {
        s.appearance.memory_at = Some(Instant::now());
        let (tx, rx) = crossbeam_channel::bounded(1);
        let spawned = std::thread::Builder::new().name("memory-indicator".into()).spawn(move || {
            let _ = tx.send(own_rss_mb());
        });
        if spawned.is_ok() {
            s.appearance.memory_rx = Some(rx);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut Session, id: &str, p: Value) -> Value {
        s.execute(id, &p).unwrap()
    }

    #[test]
    fn presentation_mode_hides_chrome_grows_the_font_and_restores_everything() {
        let mut s = Session::new();
        s.ws.settings.editor_font_size = 13.0;
        assert!(!s.ws.layout.all_hidden);
        let c = chrome(&s);
        assert!(c.toolbar && c.status_bar && c.stripes && c.editor_tabs);
        assert_eq!(run(&mut s, "view.appearance.enterPresentationMode", Value::Null)["on"], true);
        let c = chrome(&s);
        assert!(!c.toolbar && !c.status_bar && !c.stripes);
        assert_eq!(c.editor_font_size, 24.0);
        assert!(s.ws.layout.all_hidden);
        assert!(s.appearance.full_screen);
        assert!(s.effects.contains(&Effect::SetFullScreen(true)));
        assert_eq!(checked(&s, "view.appearance.enterPresentationMode"), Some(true));
        assert_eq!(
            dynamic_label(&s, "view.appearance.enterPresentationMode").as_deref(),
            Some("Exit Presentation Mode")
        );
        s.effects.clear();
        run(&mut s, "view.appearance.enterPresentationMode", Value::Null);
        assert!(!s.ws.layout.all_hidden && !s.appearance.full_screen);
        assert_eq!(s.effects, vec![Effect::SetFullScreen(false)]);
        assert_eq!(chrome(&s).editor_font_size, 13.0);
        // The settings never changed.
        assert_eq!(s.ws.settings.editor_font_size, 13.0);
    }

    #[test]
    fn zen_is_distraction_free_in_full_screen_and_full_screen_toggles() {
        let mut s = Session::new();
        run(&mut s, "view.appearance.enterZenMode", Value::Null);
        assert!(s.appearance.zen && s.appearance.distraction_free && s.appearance.full_screen);
        let c = chrome(&s);
        assert!(!c.editor_tabs && !c.toolbar && !c.navigation_top);
        // Leaving distraction-free mode ends Zen mode and full screen.
        run(&mut s, "view.appearance.enterDistractionFreeMode", Value::Null);
        assert!(!s.appearance.zen && !s.appearance.distraction_free && !s.appearance.full_screen);
        // A window that was already full screen stays so.
        run(&mut s, "view.appearance.enterFullScreen", Value::Null);
        assert!(s.appearance.full_screen);
        run(&mut s, "view.appearance.enterDistractionFreeMode", json!({ "on": true }));
        run(&mut s, "view.appearance.enterDistractionFreeMode", json!({ "on": false }));
        assert!(s.appearance.full_screen);
        run(&mut s, "view.appearance.enterFullScreen", json!({ "on": false }));
        assert!(!s.appearance.full_screen);
        assert_eq!(s.effects.last(), Some(&Effect::SetFullScreen(false)));
    }

    #[test]
    fn zoom_ide_sets_steps_clamps_and_opens_the_popup() {
        let mut s = Session::new();
        assert_eq!(dynamic_label(&s, "view.appearance.zoomIde").as_deref(), Some("Zoom IDE (Current: 100%)…"));
        let v = run(&mut s, "view.appearance.zoomIde", Value::Null);
        assert!(s.appearance.zoom_popup);
        assert_eq!(v["percent"], 100);
        run(&mut s, "view.appearance.zoomIde.zoomIn", Value::Null);
        run(&mut s, "view.appearance.zoomIde.zoomIn", Value::Null);
        assert_eq!(zoom_percent(&s.ws.settings), 120);
        assert_eq!(dynamic_label(&s, "view.appearance.zoomIde").as_deref(), Some("Zoom IDE (Current: 120%)…"));
        run(&mut s, "view.appearance.zoomIde.zoomOut", Value::Null);
        assert_eq!(zoom_percent(&s.ws.settings), 110);
        assert_eq!(run(&mut s, "view.appearance.zoomIde", json!({ "percent": 150 }))["zoom"], 1.5);
        run(&mut s, "view.appearance.zoomIde", json!({ "zoom": 99.0 }));
        assert_eq!(s.ws.settings.ide_zoom, ZOOM_MAX);
        run(&mut s, "view.appearance.zoomIde", json!({ "zoom": -3 }));
        assert_eq!(s.ws.settings.ide_zoom, ZOOM_MIN);
        assert!(s.execute("view.appearance.zoomIde", &json!({ "zoom": "big" })).is_err());
        run(&mut s, "view.appearance.zoomIde.reset", Value::Null);
        assert_eq!(chrome(&s).zoom, 1.0);
        run(&mut s, "view.appearance.zoomIde.close", Value::Null);
        assert!(!s.appearance.zoom_popup);
        // A hostile settings file cannot break the zoom.
        s.ws.settings.ide_zoom = f32::NAN;
        assert_eq!(chrome(&s).zoom, 1.0);
    }

    #[test]
    fn bars_navigation_bar_and_compact_mode_are_settings_toggles() {
        let mut s = Session::new();
        for (id, get) in [
            ("view.appearance.toolbar", (|s: &Settings| s.show_toolbar) as fn(&Settings) -> bool),
            ("view.appearance.toolWindowBars", |s| s.show_tool_window_bars),
            ("view.appearance.statusBar", |s| s.show_status_bar),
            ("view.appearance.compactMode", |s| s.compact_mode),
            ("view.appearance.navigationBar.showMembers", |s| s.navigation_bar_members),
        ] {
            let before = get(&s.ws.settings);
            assert_eq!(checked(&s, id), Some(before), "{id}");
            run(&mut s, id, Value::Null);
            assert_eq!(get(&s.ws.settings), !before, "{id}");
            assert_eq!(checked(&s, id), Some(!before), "{id}");
            run(&mut s, id, json!({ "on": before }));
            assert_eq!(get(&s.ws.settings), before, "{id}");
            assert!(is_toggle(id));
        }
        run(&mut s, "view.appearance.statusBar", json!({ "on": false }));
        assert!(!chrome(&s).status_bar);
        run(&mut s, "view.appearance.navigationBar.top", Value::Null);
        assert!(chrome(&s).navigation_top && !chrome(&s).navigation_in_status_bar);
        assert_eq!(checked(&s, "view.appearance.navigationBar.top"), Some(true));
        assert_eq!(checked(&s, "view.appearance.navigationBar.inStatusBar"), Some(false));
        run(&mut s, "view.appearance.navigationBar.dontShow", Value::Null);
        assert!(!chrome(&s).navigation_top && !chrome(&s).navigation_in_status_bar);
        run(&mut s, "view.appearance.navigationBar.inStatusBar", Value::Null);
        assert!(chrome(&s).navigation_in_status_bar);
        assert!(!is_toggle("edit.copy"));
        assert!(is_toggle("view.toggleSoftWrap"));
    }

    #[test]
    fn every_status_bar_widget_is_a_persistent_toggle() {
        let mut s = Session::new();
        for (key, _, default, _) in WIDGETS {
            let id = format!("{WIDGET_PREFIX}{key}");
            assert!(crate::registry().catalog_item(&id).is_some(), "{id} is not in the catalog");
            assert_eq!(widget_visible(&s.ws.settings, key), *default, "{key}");
            assert_eq!(checked(&s, &id), Some(*default));
            run(&mut s, &id, Value::Null);
            assert_eq!(widget_visible(&s.ws.settings, key), !default, "{key}");
            // The choice is in the settings (persisted with them).
            let saved: Settings = serde_json::from_value(serde_json::to_value(&s.ws.settings).unwrap()).unwrap();
            assert_eq!(widget_visible(&saved, key), !default);
        }
        assert_eq!(checked(&s, "view.appearance.statusBarWidgets.nope"), None);
        let st = run(&mut s, "view.appearance.state", Value::Null);
        assert_eq!(st["widgets"].as_array().map(Vec::len), Some(WIDGETS.len()));
    }

    #[test]
    fn navigation_path_lists_dirs_file_and_members() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        s.execute("file.open", &json!({ "path": root.join("src/main.rs") })).unwrap();
        s.execute("editor.setSelection", &json!({ "anchor": 20 })).unwrap();
        let labels: Vec<String> = navigation_path(&mut s).into_iter().map(|i| i.label).collect();
        assert_eq!(labels.get(1..3), Some(&["src".to_string(), "main.rs".to_string()][..]));
        assert!(labels.last().is_some_and(|l| l.contains("main")), "{labels:?}");
        assert_eq!(labels.len(), 4);
        s.execute("view.appearance.navigationBar.showMembers", &json!({ "on": false })).unwrap();
        assert_eq!(navigation_path(&mut s).len(), 3);
    }
}
