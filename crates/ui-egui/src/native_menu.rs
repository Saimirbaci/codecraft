//! The macOS global menu bar (an `NSMenu` through `muda`), built from the same data as the
//! in-window menu bar: the observed catalog plus `menus::EXTRA`, the same labels, enabled state and
//! shortcuts. Activations go through the normal command path.
//!
//! Key equivalents: `NSMenu` sees a key combination before the window does, so an item with an
//! accelerator swallows its key. To keep the shell's routing (editor vs text field vs terminal vs
//! popups, see `App::handle_input`) exactly as it is without the native bar, an item fired while
//! its shortcut's modifiers are held is turned back into the egui key event it swallowed; a click
//! runs the command. Only chords that cannot be typing (Cmd or Ctrl chords, function keys, Alt
//! with a non-character key) become native key equivalents; the others stay with egui. A disabled
//! native item does not swallow its key, so a stale enabled state only changes which path a key
//! takes, never whether it works.

// Only macOS builds the native menus; elsewhere the model is compiled for its tests.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::time::{Duration, Instant};

use egui::{Event, Key, Modifiers};
use serde_json::{Value, json};

use codecraft_engine::catalog::CATALOG;
use codecraft_engine::{CatalogItem, Session, registry};

use crate::keymap::Shortcut;
use crate::{UiState, editor_id, menus, run_command};

/// How often enabled states and checkmarks are re-evaluated (only changed items are touched).
const REFRESH: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------- model

/// One entry of the native menu tree.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Node {
    Item(Item),
    /// An item macOS provides (Services, Hide, Minimize…).
    Role(Role),
    Separator,
    Submenu {
        title: String,
        children: Vec<Node>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    pub id: &'static str,
    pub label: String,
    /// The effective shortcut (`menus::raw_shortcut`), parsed.
    pub shortcut: Option<Shortcut>,
    /// The native key equivalent in `muda` accelerator syntax, when the shortcut can be one.
    pub accelerator: Option<String>,
    /// Shown with a checkmark (a settings toggle).
    pub checkable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    Services,
    Hide,
    HideOthers,
    ShowAll,
    Minimize,
    Zoom,
}

/// Observed items that macOS itself provides; the native bar shows the system's own item.
fn role_for(id: &str) -> Option<Role> {
    Some(match id {
        "app.services" => Role::Services,
        "app.hideRustRover" => Role::Hide,
        "app.hideOthers" => Role::HideOthers,
        "app.showAll" => Role::ShowAll,
        "window.minimize" => Role::Minimize,
        "window.zoom" => Role::Zoom,
        _ => return None,
    })
}

/// Toggles shown with a checkmark, and their current state.
fn checked(s: &Session, id: &str) -> Option<bool> {
    let set = &s.ws.settings;
    Some(match id {
        "view.toggleSoftWrap" => set.soft_wrap,
        "view.toggleWhitespace" => set.show_whitespace,
        "view.toggleLineNumbers" => set.show_line_numbers,
        _ => return None,
    })
}

fn checkable(id: &str) -> bool {
    matches!(id, "view.toggleSoftWrap" | "view.toggleWhitespace" | "view.toggleLineNumbers")
}

fn shown(item: &CatalogItem) -> bool {
    menus::visible(item) || role_for(item.id).is_some()
}

fn item(id: &'static str, label: String) -> Item {
    let shortcut = menus::raw_shortcut(id).and_then(|s| Shortcut::parse(&s));
    Item { id, label, accelerator: shortcut.as_ref().and_then(accelerator), shortcut, checkable: checkable(id) }
}

/// The entries under `path`, with the in-window bar's separator rules (no leading, trailing or
/// doubled separators) and the `EXTRA` commands after a separator at the end.
fn entries(path: &[&str]) -> Vec<Node> {
    let mut out = Vec::new();
    let mut pending_sep = false;
    for c in CATALOG.iter().filter(|c| c.path == path && shown(c)) {
        if c.id == "---" {
            pending_sep = !out.is_empty();
            continue;
        }
        if pending_sep {
            out.push(Node::Separator);
            pending_sep = false;
        }
        if let Some(role) = role_for(c.id) {
            out.push(Node::Role(role));
        } else if c.submenu {
            let mut sub = path.to_vec();
            sub.push(c.label);
            out.push(Node::Submenu { title: c.label.to_string(), children: entries(&sub) });
        } else {
            out.push(Node::Item(item(c.id, menus::label_for(c))));
        }
    }
    let key = path.join(" › ");
    let shown_here = |id: &str| out.iter().any(|n| matches!(n, Node::Item(i) if i.id == id));
    let extras: Vec<&'static str> =
        menus::EXTRA.iter().filter(|(m, id)| *m == key && !shown_here(id)).map(|(_, id)| *id).collect();
    let mut extras = extras.into_iter().peekable();
    if extras.peek().is_some() && !out.is_empty() {
        out.push(Node::Separator);
    }
    for id in extras {
        let label = registry().get(id).map(|s| s.label).unwrap_or(id);
        out.push(Node::Item(item(id, label.to_string())));
    }
    out
}

/// The whole menu bar: one submenu per top-level catalog menu, in catalog order. The first one
/// (RustRover's application menu) is titled CodeCraft.
pub(crate) fn model() -> Vec<Node> {
    let mut tops: Vec<&'static str> = Vec::new();
    for c in CATALOG {
        if let Some(t) = c.path.first()
            && !tops.contains(t)
        {
            tops.push(t);
        }
    }
    tops.into_iter()
        .map(|top| Node::Submenu {
            title: if top == "RustRover" { "CodeCraft".into() } else { top.to_string() },
            children: entries(&[top]),
        })
        .collect()
}

// ---------------------------------------------------------------- accelerators

fn is_function_key(key: Key) -> bool {
    matches!(
        key,
        Key::F1
            | Key::F2
            | Key::F3
            | Key::F4
            | Key::F5
            | Key::F6
            | Key::F7
            | Key::F8
            | Key::F9
            | Key::F10
            | Key::F11
            | Key::F12
            | Key::F13
            | Key::F14
            | Key::F15
            | Key::F16
            | Key::F17
            | Key::F18
            | Key::F19
            | Key::F20
    )
}

/// The `muda`/`keyboard-types` code name of a key (US layout positions, as RustRover's keymap).
fn code_name(key: Key) -> Option<&'static str> {
    Some(match key {
        Key::A => "KeyA",
        Key::B => "KeyB",
        Key::C => "KeyC",
        Key::D => "KeyD",
        Key::E => "KeyE",
        Key::F => "KeyF",
        Key::G => "KeyG",
        Key::H => "KeyH",
        Key::I => "KeyI",
        Key::J => "KeyJ",
        Key::K => "KeyK",
        Key::L => "KeyL",
        Key::M => "KeyM",
        Key::N => "KeyN",
        Key::O => "KeyO",
        Key::P => "KeyP",
        Key::Q => "KeyQ",
        Key::R => "KeyR",
        Key::S => "KeyS",
        Key::T => "KeyT",
        Key::U => "KeyU",
        Key::V => "KeyV",
        Key::W => "KeyW",
        Key::X => "KeyX",
        Key::Y => "KeyY",
        Key::Z => "KeyZ",
        Key::Num0 => "Digit0",
        Key::Num1 => "Digit1",
        Key::Num2 => "Digit2",
        Key::Num3 => "Digit3",
        Key::Num4 => "Digit4",
        Key::Num5 => "Digit5",
        Key::Num6 => "Digit6",
        Key::Num7 => "Digit7",
        Key::Num8 => "Digit8",
        Key::Num9 => "Digit9",
        Key::F1 => "F1",
        Key::F2 => "F2",
        Key::F3 => "F3",
        Key::F4 => "F4",
        Key::F5 => "F5",
        Key::F6 => "F6",
        Key::F7 => "F7",
        Key::F8 => "F8",
        Key::F9 => "F9",
        Key::F10 => "F10",
        Key::F11 => "F11",
        Key::F12 => "F12",
        Key::F13 => "F13",
        Key::F14 => "F14",
        Key::F15 => "F15",
        Key::F16 => "F16",
        Key::F17 => "F17",
        Key::F18 => "F18",
        Key::F19 => "F19",
        Key::F20 => "F20",
        Key::ArrowUp => "ArrowUp",
        Key::ArrowDown => "ArrowDown",
        Key::ArrowLeft => "ArrowLeft",
        Key::ArrowRight => "ArrowRight",
        Key::Home => "Home",
        Key::End => "End",
        Key::PageUp => "PageUp",
        Key::PageDown => "PageDown",
        Key::Backspace => "Backspace",
        Key::Delete => "Delete",
        Key::Enter => "Enter",
        Key::Tab => "Tab",
        Key::Escape => "Escape",
        Key::Space => "Space",
        Key::Insert => "Insert",
        Key::Slash => "Slash",
        Key::Backslash => "Backslash",
        Key::OpenBracket => "BracketLeft",
        Key::CloseBracket => "BracketRight",
        Key::Comma => "Comma",
        Key::Period => "Period",
        Key::Equals => "Equal",
        Key::Minus => "Minus",
        Key::Backtick => "Backquote",
        Key::Semicolon => "Semicolon",
        Key::Quote => "Quote",
        // `+` and `?` are shifted keys on a US layout; a key equivalent cannot say "the key that
        // types +", so these stay egui shortcuts.
        _ => return None,
    })
}

/// Keys that type a character (with or without Shift/Alt): a key equivalent on them would eat
/// typing (Alt+letter types accented letters on macOS).
fn is_character_key(key: Key) -> bool {
    code_name(key).is_some_and(|c| {
        c.starts_with("Key") || c.starts_with("Digit") || c == "Space" || {
            matches!(
                key,
                Key::Slash
                    | Key::Backslash
                    | Key::OpenBracket
                    | Key::CloseBracket
                    | Key::Comma
                    | Key::Period
                    | Key::Equals
                    | Key::Minus
                    | Key::Backtick
                    | Key::Semicolon
                    | Key::Quote
            )
        }
    })
}

/// The native key equivalent for a shortcut in `muda` syntax (`shift+super+KeyF`), or `None`
/// when the shortcut must stay with egui (plain or Shift-only keys, Alt+character).
pub(crate) fn accelerator(sc: &Shortcut) -> Option<String> {
    let chord = sc.cmd || sc.ctrl || is_function_key(sc.key) || (sc.alt && !is_character_key(sc.key));
    if !chord {
        return None;
    }
    let code = code_name(sc.key)?;
    let mut out = String::new();
    for (on, name) in [(sc.shift, "shift+"), (sc.ctrl, "control+"), (sc.alt, "alt+"), (sc.cmd, "super+")] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(code);
    Some(out)
}

// ---------------------------------------------------------------- dispatch

/// What one native menu activation turns into.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Dispatch {
    /// Feed these events to egui (a swallowed key equivalent, or an edit a focused text field
    /// should do itself).
    Inject(Vec<Event>),
    /// Run the command (a click on the item).
    Run(String, Value),
}

fn key_pair(key: Key, modifiers: Modifiers) -> Vec<Event> {
    [true, false]
        .into_iter()
        .map(|pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers })
        .collect()
}

fn paste_event(clipboard: &dyn Fn() -> Option<String>) -> Vec<Event> {
    clipboard().map(|t| t.replace("\r\n", "\n")).filter(|t| !t.is_empty()).map(Event::Paste).into_iter().collect()
}

/// The events egui-winit produces for a key press: Cmd+C/X/V become Copy/Cut/Paste, the rest a
/// key press and release.
pub(crate) fn key_events(sc: &Shortcut, clipboard: &dyn Fn() -> Option<String>) -> Vec<Event> {
    if sc.cmd && !sc.ctrl && !sc.alt && !sc.shift {
        match sc.key {
            Key::C => return vec![Event::Copy],
            Key::X => return vec![Event::Cut],
            Key::V => return paste_event(clipboard),
            _ => {}
        }
    }
    key_pair(sc.key, sc.modifiers())
}

/// Edit commands that a focused text field (or the terminal) performs itself, as the events
/// egui's text widgets understand.
fn text_field_events(id: &str, clipboard: &dyn Fn() -> Option<String>) -> Option<Vec<Event>> {
    let cmd = Modifiers::MAC_CMD | Modifiers::COMMAND;
    Some(match id {
        "edit.copy" => vec![Event::Copy],
        "edit.cut" => vec![Event::Cut],
        "edit.paste" => paste_event(clipboard),
        "edit.selectAll" => key_pair(Key::A, cmd),
        "edit.undo" => key_pair(Key::Z, cmd),
        "edit.redo" => key_pair(Key::Z, cmd | Modifiers::SHIFT),
        _ => return None,
    })
}

fn modifiers_held(sc: &Shortcut, m: &Modifiers) -> bool {
    sc.cmd == m.mac_cmd && sc.ctrl == m.ctrl && sc.alt == m.alt && sc.shift == m.shift
}

/// Decide what an activation of item `id` does. `held`: the modifier states seen since the last
/// frame (one matches the item's key equivalent when the keyboard fired it); `widget_focus`: a
/// widget other than the editor has keyboard focus.
pub(crate) fn dispatch(
    id: &str,
    shortcut: Option<&Shortcut>,
    held: &[Modifiers],
    widget_focus: bool,
    clipboard: &dyn Fn() -> Option<String>,
) -> Dispatch {
    if let Some(sc) = shortcut
        && accelerator(sc).is_some()
        && held.iter().any(|m| modifiers_held(sc, m))
    {
        return Dispatch::Inject(key_events(sc, clipboard));
    }
    if widget_focus && let Some(events) = text_field_events(id, clipboard) {
        return Dispatch::Inject(events);
    }
    if id == "edit.paste"
        && let Some(text) = clipboard()
    {
        return Dispatch::Run(id.to_string(), json!({ "text": text }));
    }
    Dispatch::Run(id.to_string(), Value::Null)
}

fn find_item<'a>(nodes: &'a [Node], id: &str) -> Option<&'a Item> {
    nodes.iter().find_map(|n| match n {
        Node::Item(it) if it.id == id => Some(it),
        Node::Submenu { children, .. } => find_item(children, id),
        _ => None,
    })
}

// ---------------------------------------------------------------- state

/// The native menu bar's lifecycle, owned by the app.
#[derive(Default)]
pub struct NativeMenu {
    /// Only the real app may touch `NSApp` (tests and offscreen snapshots have none); set by `run`.
    pub(crate) allowed: bool,
    #[cfg(target_os = "macos")]
    bar: Option<mac::Bar>,
    last_refresh: Option<Instant>,
}

impl NativeMenu {
    /// The native bar is installed (the in-window bar is hidden).
    pub fn active(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.bar.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// Per frame (UI thread, after the event loop started): install or remove the bar following
    /// the `native_menu_bar` setting, rebuild it when the menus changed (keymap switch), and
    /// refresh enabled states and checkmarks.
    pub(crate) fn update(&mut self, ctx: &egui::Context, s: &Session) {
        if !self.allowed {
            return;
        }
        let now = Instant::now();
        if self.last_refresh.is_some_and(|t| now.duration_since(t) < REFRESH) {
            return;
        }
        self.last_refresh = Some(now);
        #[cfg(target_os = "macos")]
        self.update_mac(ctx, s);
        #[cfg(not(target_os = "macos"))]
        let _ = (ctx, s);
    }

    #[cfg(target_os = "macos")]
    fn update_mac(&mut self, ctx: &egui::Context, s: &Session) {
        if !s.ws.settings.native_menu_bar {
            if let Some(bar) = self.bar.take() {
                bar.remove();
            }
            return;
        }
        let model = model();
        if self.bar.as_ref().is_some_and(|b| b.model != model)
            && let Some(bar) = self.bar.take()
        {
            bar.remove();
        }
        if self.bar.is_none() {
            mac::set_waker(ctx.clone());
            match mac::Bar::install(model) {
                Ok(bar) => self.bar = Some(bar),
                Err(e) => {
                    log::warn!("native menu bar: {e}");
                    self.allowed = false;
                    return;
                }
            }
        }
        if let Some(bar) = self.bar.as_mut() {
            bar.refresh(s);
        }
    }

    /// Native items activated since the last call (none when the bar is inactive).
    pub(crate) fn take_fired(&mut self) -> Vec<String> {
        #[cfg(target_os = "macos")]
        if self.bar.is_some() {
            return mac::take_events();
        }
        Vec::new()
    }

    /// Turn activations into commands on the window's session `s`, or into input events appended
    /// to `events` (that window's input for this frame). `held`: see [`held_modifiers`].
    pub(crate) fn deliver(
        &self,
        fired: Vec<String>,
        ctx: &egui::Context,
        s: &mut Session,
        st: &mut UiState,
        held: &[Modifiers],
        events: &mut Vec<Event>,
    ) {
        if fired.is_empty() {
            return;
        }
        let model = self.model();
        let widget_focus =
            ctx.memory(|m| m.focused()).is_some_and(|f| s.ws.active_editor().is_none_or(|e| editor_id(e) != f));
        let clipboard = system_clipboard;
        for id in fired {
            let shortcut = model.and_then(|m| find_item(m, &id)).and_then(|it| it.shortcut);
            let d = dispatch(&id, shortcut.as_ref(), held, widget_focus, &clipboard);
            log::debug!("native menu {id}: {d:?}");
            match d {
                Dispatch::Inject(evs) => events.extend(evs),
                Dispatch::Run(id, params) => run_command(s, st, &id, &params),
            }
        }
    }

    fn model(&self) -> Option<&[Node]> {
        #[cfg(target_os = "macos")]
        if let Some(bar) = &self.bar {
            return Some(&bar.model);
        }
        None
    }
}

/// Every modifier state since the last frame: the last frame's, then each change in this frame's
/// input. A quick chord may already be released when its menu activation is delivered.
pub(crate) fn held_modifiers(ctx: &egui::Context, events: &[Event]) -> Vec<Modifiers> {
    std::iter::once(ctx.input(|i| i.modifiers))
        .chain(events.iter().filter_map(|e| if let Event::ModifiersChanged(m) = e { Some(*m) } else { None }))
        .collect()
}

fn system_clipboard() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok())
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::sync::{Mutex, PoisonError};

    use muda::accelerator::Accelerator;
    use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

    use codecraft_engine::{Session, registry};

    use super::{Node, Role, checked};

    /// Activations not yet seen by a frame, and the context to wake.
    static FIRED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    static WAKER: Mutex<Option<egui::Context>> = Mutex::new(None);

    pub(super) fn set_waker(ctx: egui::Context) {
        *WAKER.lock().unwrap_or_else(PoisonError::into_inner) = Some(ctx);
    }

    pub(super) fn take_events() -> Vec<String> {
        std::mem::take(&mut *FIRED.lock().unwrap_or_else(PoisonError::into_inner))
    }

    enum Handle {
        Plain(MenuItem),
        Check(CheckMenuItem),
    }

    struct Entry {
        id: &'static str,
        handle: Handle,
        enabled: bool,
        checked: bool,
    }

    pub(super) struct Bar {
        menu: Menu,
        pub(super) model: Vec<Node>,
        entries: Vec<Entry>,
    }

    /// `muda` treats `&` as a mnemonic marker on every platform.
    fn escape(label: &str) -> String {
        label.replace('&', "&&")
    }

    fn predefined(role: Role) -> PredefinedMenuItem {
        match role {
            Role::Services => PredefinedMenuItem::services(None),
            Role::Hide => PredefinedMenuItem::hide(Some("Hide CodeCraft")),
            Role::HideOthers => PredefinedMenuItem::hide_others(None),
            Role::ShowAll => PredefinedMenuItem::show_all(None),
            Role::Minimize => PredefinedMenuItem::minimize(Some("Minimize")),
            Role::Zoom => PredefinedMenuItem::maximize(Some("Zoom")),
        }
    }

    fn fill(parent: &Submenu, nodes: &[Node], entries: &mut Vec<Entry>) -> muda::Result<()> {
        for node in nodes {
            match node {
                Node::Separator => parent.append(&PredefinedMenuItem::separator())?,
                Node::Role(role) => parent.append(&predefined(*role))?,
                Node::Submenu { title, children } => {
                    let sub = Submenu::new(escape(title), !children.is_empty());
                    fill(&sub, children, entries)?;
                    parent.append(&sub)?;
                }
                Node::Item(it) => {
                    let accel = it.accelerator.as_deref().and_then(|a| a.parse::<Accelerator>().ok());
                    let label = escape(&it.label);
                    let handle = if it.checkable {
                        let item = CheckMenuItem::with_id(it.id, label, false, false, accel);
                        parent.append(&item)?;
                        Handle::Check(item)
                    } else {
                        let item = MenuItem::with_id(it.id, label, false, accel);
                        parent.append(&item)?;
                        Handle::Plain(item)
                    };
                    entries.push(Entry { id: it.id, handle, enabled: false, checked: false });
                }
            }
        }
        Ok(())
    }

    impl Bar {
        /// Build the `NSMenu` tree and make it the application's main menu. Main thread only,
        /// after `NSApp` finished launching (eframe's first frame).
        pub(super) fn install(model: Vec<Node>) -> Result<Self, String> {
            let menu = Menu::new();
            let mut entries = Vec::new();
            let mut window_menu = None;
            let mut help_menu = None;
            for node in &model {
                let Node::Submenu { title, children } = node else { continue };
                let sub = Submenu::new(escape(title), true);
                fill(&sub, children, &mut entries).map_err(|e| e.to_string())?;
                menu.append(&sub).map_err(|e| e.to_string())?;
                match title.as_str() {
                    "Window" => window_menu = Some(sub),
                    "Help" => help_menu = Some(sub),
                    _ => {}
                }
            }
            MenuEvent::set_event_handler(Some(|e: MenuEvent| {
                FIRED.lock().unwrap_or_else(PoisonError::into_inner).push(e.id.0);
                if let Some(ctx) = WAKER.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                    ctx.request_repaint();
                }
            }));
            menu.init_for_nsapp();
            if let Some(w) = window_menu {
                w.set_as_windows_menu_for_nsapp();
            }
            if let Some(h) = help_menu {
                h.set_as_help_menu_for_nsapp();
            }
            Ok(Self { menu, model, entries })
        }

        pub(super) fn remove(self) {
            self.menu.remove_for_nsapp();
            MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
        }

        /// Push changed enabled states and checkmarks to AppKit.
        pub(super) fn refresh(&mut self, s: &Session) {
            let reg = registry();
            for e in &mut self.entries {
                let enabled = reg.contains(e.id) && s.enabled(e.id).is_ok();
                if enabled != e.enabled {
                    e.enabled = enabled;
                    match &e.handle {
                        Handle::Plain(i) => i.set_enabled(enabled),
                        Handle::Check(i) => i.set_enabled(enabled),
                    }
                }
                if let Handle::Check(i) = &e.handle {
                    let on = checked(s, e.id).unwrap_or(false);
                    if on != e.checked || on != i.is_checked() {
                        e.checked = on;
                        i.set_checked(on);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(s: &str) -> Shortcut {
        Shortcut::parse(s).unwrap()
    }

    fn no_clip() -> Option<String> {
        None
    }

    fn all_items(nodes: &[Node], out: &mut Vec<Item>) {
        for n in nodes {
            match n {
                Node::Item(i) => out.push(i.clone()),
                Node::Submenu { children, .. } => all_items(children, out),
                _ => {}
            }
        }
    }

    fn submenu<'a>(nodes: &'a [Node], title: &str) -> &'a [Node] {
        nodes
            .iter()
            .find_map(|n| match n {
                Node::Submenu { title: t, children } if t == title => Some(children.as_slice()),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn accelerator_strings() {
        assert_eq!(accelerator(&sc("Shift+Cmd+F")).as_deref(), Some("shift+super+KeyF"));
        assert_eq!(accelerator(&sc("Cmd+,")).as_deref(), Some("super+Comma"));
        assert_eq!(accelerator(&sc("Ctrl+Alt+Left")).as_deref(), Some("control+alt+ArrowLeft"));
        assert_eq!(accelerator(&sc("Alt+Shift+Cmd+`")).as_deref(), Some("shift+alt+super+Backquote"));
        assert_eq!(accelerator(&sc("Cmd+1")).as_deref(), Some("super+Digit1"));
        assert_eq!(accelerator(&sc("Ctrl+Cmd+=")).as_deref(), Some("control+super+Equal"));
        // Function keys with or without modifiers.
        assert_eq!(accelerator(&sc("F2")).as_deref(), Some("F2"));
        assert_eq!(accelerator(&sc("Shift+F6")).as_deref(), Some("shift+F6"));
        assert_eq!(accelerator(&sc("Alt+F12")).as_deref(), Some("alt+F12"));
        // Alt with a non-character key is a chord; Alt+character is typing on macOS.
        assert_eq!(accelerator(&sc("Alt+Return")).as_deref(), Some("alt+Enter"));
        assert_eq!(accelerator(&sc("Alt+Up")).as_deref(), Some("alt+ArrowUp"));
        assert_eq!(accelerator(&sc("Alt+/")), None);
        assert_eq!(accelerator(&sc("Alt+J")), None);
        // Plain and Shift-only keys stay with egui.
        assert_eq!(accelerator(&sc("Backspace")), None);
        assert_eq!(accelerator(&sc("Tab")), None);
        assert_eq!(accelerator(&sc("Shift+Return")), None);
        // Shifted characters have no key-position code.
        assert_eq!(accelerator(&sc("Cmd++")), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn accelerators_parse_in_muda() {
        use muda::accelerator::{Accelerator, Code, Modifiers as M};
        let a: Accelerator = accelerator(&sc("Shift+Cmd+F")).unwrap().parse().unwrap();
        assert_eq!(a, Accelerator::new(Some(M::SHIFT | M::SUPER), Code::KeyF));
        let a: Accelerator = accelerator(&sc("Ctrl+Alt+Left")).unwrap().parse().unwrap();
        assert_eq!(a, Accelerator::new(Some(M::CONTROL | M::ALT), Code::ArrowLeft));
        // Every accelerator the menu model produces parses.
        let mut items = Vec::new();
        all_items(&model(), &mut items);
        for it in items {
            if let Some(a) = &it.accelerator {
                assert!(a.parse::<Accelerator>().is_ok(), "{}: {a}", it.id);
            }
        }
    }

    #[test]
    fn model_mirrors_the_menu_bar() {
        let m = model();
        let titles: Vec<&str> = m
            .iter()
            .map(|n| match n {
                Node::Submenu { title, .. } => title.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(titles.first(), Some(&"CodeCraft"));
        for t in ["File", "Edit", "View", "Navigate", "Code", "Refactor", "Build", "Run", "Window", "Help"] {
            assert!(titles.contains(&t), "missing top menu {t}: {titles:?}");
        }
        // App menu: About, Settings with its shortcut, macOS roles, Quit; no Check for Updates.
        let app = submenu(&m, "CodeCraft");
        let ids: Vec<&str> = app
            .iter()
            .filter_map(|n| match n {
                Node::Item(i) => Some(i.id),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&"app.aboutRustRover"));
        assert!(ids.contains(&"app.quitRustRover"));
        assert!(!ids.contains(&"app.checkForUpdates"));
        assert!(app.contains(&Node::Role(Role::Services)));
        assert!(app.contains(&Node::Role(Role::Hide)));
        let about = app
            .iter()
            .find_map(|n| match n {
                Node::Item(i) if i.id == "app.aboutRustRover" => Some(i),
                _ => None,
            })
            .unwrap();
        assert_eq!(about.label, "About CodeCraft");
        let settings = find_item(app, "app.settings").unwrap();
        assert_eq!(settings.accelerator.as_deref(), Some("super+Comma"));
        // Window menu has the system's Minimize/Zoom.
        let window = submenu(&m, "Window");
        assert!(window.contains(&Node::Role(Role::Minimize)));
        assert!(window.contains(&Node::Role(Role::Zoom)));
        // EXTRA commands appear where the in-window bar puts them.
        assert!(find_item(submenu(&m, "Build"), "cargo.check").is_some());
        assert!(find_item(submenu(&m, "Run"), "run.debuggingActions.stepOver").is_some());
        // Checkable toggles.
        let view = submenu(&m, "View");
        assert!(find_item(view, "view.toggleSoftWrap").unwrap().checkable);
    }

    #[test]
    fn no_item_appears_twice_in_one_menu() {
        fn check(nodes: &[Node], path: &str) {
            let ids: Vec<&str> = nodes
                .iter()
                .filter_map(|n| match n {
                    Node::Item(i) => Some(i.id),
                    _ => None,
                })
                .collect();
            for (k, id) in ids.iter().enumerate() {
                assert!(!ids[..k].contains(id), "{id} twice in {path}");
            }
            for n in nodes {
                if let Node::Submenu { title, children } = n {
                    check(children, &format!("{path} › {title}"));
                }
            }
        }
        check(&model(), "");
    }

    #[test]
    fn separators_are_never_leading_trailing_or_doubled() {
        fn check(nodes: &[Node], path: &str) {
            assert!(!matches!(nodes.first(), Some(Node::Separator)), "leading separator in {path}");
            assert!(!matches!(nodes.last(), Some(Node::Separator)), "trailing separator in {path}");
            for w in nodes.windows(2) {
                assert!(
                    !(matches!(w[0], Node::Separator) && matches!(w[1], Node::Separator)),
                    "double separator in {path}"
                );
            }
            for n in nodes {
                if let Node::Submenu { title, children } = n {
                    check(children, &format!("{path} › {title}"));
                }
            }
        }
        for n in model() {
            if let Node::Submenu { title, children } = &n {
                check(children, title);
            }
        }
    }

    #[test]
    fn items_are_known_ids_without_out_of_scope_catalog_entries() {
        let mut items = Vec::new();
        all_items(&model(), &mut items);
        assert!(items.len() > 200, "{}", items.len());
        for it in &items {
            let known = registry().catalog_item(it.id).is_some() || registry().contains(it.id);
            assert!(known, "unknown id {}", it.id);
            let extra = menus::EXTRA.iter().any(|(_, id)| *id == it.id);
            if let Some(c) = registry().catalog_item(it.id) {
                assert!(menus::visible(c) || extra, "{} is out of scope", it.id);
            }
            assert_ne!(it.id, "---");
        }
    }

    #[test]
    fn keyboard_activation_reinjects_the_key() {
        let s = sc("Shift+Cmd+F");
        let held = s.modifiers();
        let d = dispatch("edit.find.findInFiles", Some(&s), &[held], false, &no_clip);
        let Dispatch::Inject(events) = d else { panic!("{d:?}") };
        assert_eq!(events.len(), 2);
        assert!(
            matches!(events[0], Event::Key { key: Key::F, pressed: true, modifiers, .. } if modifiers.mac_cmd && modifiers.shift)
        );
        assert!(matches!(events[1], Event::Key { key: Key::F, pressed: false, .. }));
    }

    #[test]
    fn a_chord_released_before_the_frame_still_counts_as_keyboard() {
        let s = sc("Cmd+S");
        let d = dispatch("file.saveAll", Some(&s), &[Modifiers::NONE, s.modifiers(), Modifiers::NONE], false, &no_clip);
        assert!(matches!(d, Dispatch::Inject(_)), "{d:?}");
    }

    #[test]
    fn keyboard_copy_paste_become_clipboard_events() {
        let c = sc("Cmd+C");
        assert_eq!(
            dispatch("edit.copy", Some(&c), &[c.modifiers()], false, &no_clip),
            Dispatch::Inject(vec![Event::Copy])
        );
        let v = sc("Cmd+V");
        let clip = || Some("a\r\nb".to_string());
        assert_eq!(
            dispatch("edit.paste", Some(&v), &[v.modifiers()], true, &clip),
            Dispatch::Inject(vec![Event::Paste("a\nb".into())])
        );
        // An empty clipboard pastes nothing (like egui-winit).
        assert_eq!(dispatch("edit.paste", Some(&v), &[v.modifiers()], true, &no_clip), Dispatch::Inject(vec![]));
        // Copy Path (Shift+Alt+Cmd+C) is a key, not Copy.
        let p = sc("Shift+Alt+Cmd+C");
        let Dispatch::Inject(e) = dispatch("edit.copyPathReference", Some(&p), &[p.modifiers()], false, &no_clip)
        else {
            panic!()
        };
        assert!(matches!(e[0], Event::Key { key: Key::C, .. }));
    }

    #[test]
    fn click_runs_the_command() {
        let s = sc("Shift+Cmd+F");
        let d = dispatch("edit.find.findInFiles", Some(&s), &[Modifiers::NONE], false, &no_clip);
        assert_eq!(d, Dispatch::Run("edit.find.findInFiles".into(), Value::Null));
        // Items whose shortcut stays with egui always run.
        let b = sc("Backspace");
        assert_eq!(
            dispatch("edit.delete", Some(&b), &[Modifiers::NONE], false, &no_clip),
            Dispatch::Run("edit.delete".into(), Value::Null)
        );
        // Paste into the editor carries the system clipboard.
        let clip = || Some("x".to_string());
        assert_eq!(
            dispatch("edit.paste", Some(&sc("Cmd+V")), &[Modifiers::NONE], false, &clip),
            Dispatch::Run("edit.paste".into(), json!({ "text": "x" }))
        );
    }

    #[test]
    fn click_edit_in_a_text_field_goes_to_the_field() {
        let d = dispatch("edit.copy", Some(&sc("Cmd+C")), &[Modifiers::NONE], true, &no_clip);
        assert_eq!(d, Dispatch::Inject(vec![Event::Copy]));
        let Dispatch::Inject(e) = dispatch("edit.undo", Some(&sc("Cmd+Z")), &[Modifiers::NONE], true, &no_clip) else {
            panic!()
        };
        assert!(
            matches!(e[0], Event::Key { key: Key::Z, pressed: true, modifiers, .. } if modifiers.mac_cmd && !modifiers.shift)
        );
        // Non-edit commands still run.
        assert_eq!(
            dispatch("file.saveAll", None, &[Modifiers::NONE], true, &no_clip),
            Dispatch::Run("file.saveAll".into(), Value::Null)
        );
    }

    #[test]
    fn headless_state_never_installs() {
        let mut n = NativeMenu::default();
        let s = Session::new();
        n.update(&egui::Context::default(), &s);
        assert!(!n.active());
    }
}
