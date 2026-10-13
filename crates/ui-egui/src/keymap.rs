//! Shortcut parsing, matching and display (RustRover keymap notation: `Shift+Cmd+F`), and the
//! active keymap (Settings › Keymap) that dispatch and every displayed shortcut follow.

use std::cell::RefCell;
use std::collections::HashMap;

use egui::{Key, Modifiers};

use codecraft_engine::{Context, keymaps, registry};
use codecraft_workspace::Settings;

/// The active keymap's shortcut table (set by [`set_active`] when the settings change).
#[derive(Default)]
struct Active {
    /// Windows/Linux keymap: shortcuts display as `Ctrl+Shift+A` rather than macOS glyphs.
    text_labels: bool,
    table: Option<HashMap<&'static str, Vec<String>>>,
}

thread_local! {
    static ACTIVE: RefCell<Active> = RefCell::new(Active::default());
}

/// Make `settings`' keymap (and the user's overrides) the one menus and labels show.
pub fn set_active(settings: &Settings) {
    let text_labels = keymaps::active(settings) == keymaps::WINDOWS_LINUX;
    let table = keymaps::table(settings);
    ACTIVE.with(|a| *a.borrow_mut() = Active { text_labels, table: Some(table) });
}

/// The shortcuts of a command in the active keymap (RustRover notation).
pub fn active_shortcuts(id: &str) -> Vec<String> {
    ACTIVE.with(|a| match &a.borrow().table {
        Some(t) => t.get(id).cloned().unwrap_or_default(),
        None => registry().shortcuts(id).iter().map(|s| (*s).to_string()).collect(),
    })
}

/// The command bound to the double-Shift gesture (Search Everywhere unless remapped).
pub fn double_shift_command() -> Option<&'static str> {
    ACTIVE.with(|a| match &a.borrow().table {
        Some(t) => {
            let mut ids: Vec<&'static str> =
                t.iter().filter(|(_, v)| v.iter().any(|s| s == keymaps::DOUBLE_SHIFT)).map(|(id, _)| *id).collect();
            ids.sort_unstable();
            ids.first().copied()
        }
        None => Some("navigate.searchEverywhere"),
    })
}

/// The shortcut text (`Shift+Cmd+K`) for a key press, or `None` for keys a shortcut cannot use.
pub fn from_event(key: Key, m: &Modifiers) -> Option<String> {
    let name = match key {
        Key::ArrowUp => "Up".to_string(),
        Key::ArrowDown => "Down".into(),
        Key::ArrowLeft => "Left".into(),
        Key::ArrowRight => "Right".into(),
        Key::Enter => "Enter".into(),
        Key::Escape => "Escape".into(),
        Key::Backspace => "Backspace".into(),
        Key::Delete => "Delete".into(),
        Key::Tab => "Tab".into(),
        Key::Space => "Space".into(),
        Key::Home => "Home".into(),
        Key::End => "End".into(),
        Key::PageUp => "PageUp".into(),
        Key::PageDown => "PageDown".into(),
        Key::Insert => "Insert".into(),
        Key::Slash => "/".into(),
        Key::Backslash => "\\".into(),
        Key::OpenBracket => "[".into(),
        Key::CloseBracket => "]".into(),
        Key::Comma => ",".into(),
        Key::Period => ".".into(),
        Key::Equals => "=".into(),
        Key::Minus => "-".into(),
        Key::Plus => "+".into(),
        Key::Backtick => "`".into(),
        Key::Semicolon => ";".into(),
        Key::Quote => "'".into(),
        Key::Questionmark => "?".into(),
        other => other.symbol_or_name().to_string(),
    };
    let mut text = String::new();
    // egui reports Cmd as `mac_cmd` (macOS) and Ctrl as `ctrl` everywhere.
    for (on, part) in [(m.ctrl, "Ctrl+"), (m.alt, "Alt+"), (m.shift, "Shift+"), (m.mac_cmd, "Cmd+")] {
        if on {
            text.push_str(part);
        }
    }
    text.push_str(&name);
    keymaps::normalize(&text).ok().filter(|s| Shortcut::parse(s).is_some())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Shortcut {
    pub key: Key,
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Shortcut {
    /// Parse `Shift+Cmd+F`, `Ctrl+Alt+Left`, `F2`, `Alt+/`… Modifier order does not matter.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() || s.contains(' ') {
            return None; // "Shift Shift" (double press) is handled separately.
        }
        let (mods, key) = match s.strip_suffix("++") {
            Some(m) => (m, "+"),
            None => match s.rsplit_once('+') {
                Some((m, k)) => (m, k),
                None => ("", s),
            },
        };
        let mut sc = Self { key: key_from_name(key)?, cmd: false, ctrl: false, alt: false, shift: false };
        for m in mods.split('+').filter(|m| !m.is_empty()) {
            match m {
                "Cmd" | "Meta" => sc.cmd = true,
                "Ctrl" => sc.ctrl = true,
                "Alt" | "Option" => sc.alt = true,
                "Shift" => sc.shift = true,
                _ => return None,
            }
        }
        Some(sc)
    }

    pub fn matches(&self, key: Key, m: &Modifiers) -> bool {
        self.key == key && self.cmd == m.mac_cmd && self.ctrl == m.ctrl && self.alt == m.alt && self.shift == m.shift
    }

    pub fn modifiers(&self) -> Modifiers {
        Modifiers { alt: self.alt, ctrl: self.ctrl, shift: self.shift, mac_cmd: self.cmd, command: self.cmd }
    }

    pub fn has_command_modifier(&self) -> bool {
        self.cmd || self.ctrl || self.alt
    }
}

fn key_from_name(name: &str) -> Option<Key> {
    Some(match name {
        "Up" => Key::ArrowUp,
        "Down" => Key::ArrowDown,
        "Left" => Key::ArrowLeft,
        "Right" => Key::ArrowRight,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Return" | "Enter" => Key::Enter,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Space" | " " => Key::Space,
        "Insert" => Key::Insert,
        "/" => Key::Slash,
        "\\" => Key::Backslash,
        "[" => Key::OpenBracket,
        "]" => Key::CloseBracket,
        "," => Key::Comma,
        "." => Key::Period,
        "=" => Key::Equals,
        "-" => Key::Minus,
        "+" => Key::Plus,
        "`" => Key::Backtick,
        ";" => Key::Semicolon,
        "'" => Key::Quote,
        "?" => Key::Questionmark,
        other => {
            if let Some(n) = other.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()) {
                return f_key(n);
            }
            let mut chars = other.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else { return Key::from_name(other) };
            return Key::from_name(&c.to_ascii_uppercase().to_string());
        }
    })
}

fn f_key(n: u8) -> Option<Key> {
    Some(match n {
        1 => Key::F1,
        2 => Key::F2,
        3 => Key::F3,
        4 => Key::F4,
        5 => Key::F5,
        6 => Key::F6,
        7 => Key::F7,
        8 => Key::F8,
        9 => Key::F9,
        10 => Key::F10,
        11 => Key::F11,
        12 => Key::F12,
        _ => return None,
    })
}

/// macOS menu glyphs (`⇧⌘F`); with the Windows/Linux keymap active, text (`Ctrl+Shift+F`).
pub fn display(s: &str) -> String {
    if ACTIVE.with(|a| a.borrow().text_labels) {
        return if s == keymaps::DOUBLE_SHIFT {
            "Double Shift".into()
        } else {
            keymaps::normalize(s).unwrap_or_else(|_| s.to_string())
        };
    }
    if s == "Shift Shift" {
        return "⇧⇧".into();
    }
    let Some(sc) = Shortcut::parse(s) else { return s.to_string() };
    let mut out = String::new();
    if sc.ctrl {
        out.push('⌃');
    }
    if sc.alt {
        out.push('⌥');
    }
    if sc.shift {
        out.push('⇧');
    }
    if sc.cmd {
        out.push('⌘');
    }
    let key = match sc.key {
        Key::ArrowUp => "↑".to_string(),
        Key::ArrowDown => "↓".into(),
        Key::ArrowLeft => "←".into(),
        Key::ArrowRight => "→".into(),
        Key::Backspace => "⌫".into(),
        Key::Delete => "⌦".into(),
        Key::Enter => "↩".into(),
        Key::Tab => "⇥".into(),
        Key::Escape => "⎋".into(),
        Key::Space => "Space".into(),
        Key::Home => "↖".into(),
        Key::End => "↘".into(),
        Key::PageUp => "⇞".into(),
        Key::PageDown => "⇟".into(),
        other => other.symbol_or_name().to_string(),
    };
    out.push_str(&key);
    out
}

/// One bound shortcut.
#[derive(Clone, Debug)]
pub struct Binding {
    pub shortcut: Shortcut,
    pub id: &'static str,
    pub context: Context,
}

/// All bindings of the platform's default keymap (see [`bindings_for`]).
pub fn bindings() -> Vec<Binding> {
    bindings_for(&Settings::default())
}

/// All bindings of `settings`' keymap, in match order: the user's overrides, then commands' own
/// shortcuts, then catalog shortcuts, first wins for one key in one context (e.g. Backspace is
/// the editor's backspace, not Edit › Delete).
pub fn bindings_for(settings: &Settings) -> Vec<Binding> {
    keymaps::bindings(settings)
        .into_iter()
        .filter_map(|b| Some(Binding { shortcut: Shortcut::parse(&b.shortcut)?, id: b.id, context: b.context }))
        .collect()
}

/// The shortcut text shown next to a command in menus (active keymap).
pub fn shortcut_label(id: &str) -> Option<String> {
    active_shortcuts(id).first().map(|s| display(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shortcuts() {
        let s = Shortcut::parse("Shift+Cmd+F").unwrap();
        assert!(s.shift && s.cmd && !s.alt && s.key == Key::F);
        assert_eq!(Shortcut::parse("Alt+F12").unwrap().key, Key::F12);
        assert_eq!(Shortcut::parse("Ctrl+Shift+.").unwrap().key, Key::Period);
        assert_eq!(Shortcut::parse("Cmd+=").unwrap().key, Key::Equals);
        assert!(Shortcut::parse("Shift Shift").is_none());
        assert_eq!(display("Shift+Cmd+F"), "⇧⌘F");
    }

    #[test]
    fn every_catalog_shortcut_parses() {
        for item in codecraft_engine::catalog::CATALOG {
            if let Some(s) = item.shortcut {
                assert!(Shortcut::parse(s).is_some(), "{} `{s}`", item.id);
            }
        }
    }

    #[test]
    fn active_keymap_drives_labels_bindings_and_recording() {
        let win = Settings { keymap: "Windows/Linux".into(), ..Default::default() };
        set_active(&win);
        assert_eq!(shortcut_label("help.findAction").as_deref(), Some("Ctrl+Shift+A"));
        assert_eq!(display("Shift Shift"), "Double Shift");
        let b = bindings_for(&win);
        let sc = Shortcut::parse("Ctrl+Shift+A").unwrap();
        assert!(b.iter().any(|b| b.shortcut == sc && b.id == "help.findAction"));
        let mut mac = Settings { keymap: "macOS".into(), ..Default::default() };
        codecraft_engine::keymaps::set_shortcuts(&mut mac, "help.findAction", &["F12".into()]).unwrap();
        set_active(&mac);
        assert_eq!(shortcut_label("help.findAction").as_deref(), Some("F12"));
        assert_eq!(double_shift_command(), Some("navigate.searchEverywhere"));
        let cmd_shift = Modifiers { shift: true, mac_cmd: true, command: true, ..Default::default() };
        assert_eq!(from_event(Key::K, &cmd_shift).as_deref(), Some("Shift+Cmd+K"));
        let ctrl_alt = Modifiers { ctrl: true, alt: true, ..Default::default() };
        assert_eq!(from_event(Key::Slash, &ctrl_alt).as_deref(), Some("Ctrl+Alt+/"));
        assert_eq!(from_event(Key::F5, &Modifiers::NONE).as_deref(), Some("F5"));
        ACTIVE.with(|a| *a.borrow_mut() = Active::default());
    }

    #[test]
    fn backspace_is_the_editor_backspace() {
        let b = bindings();
        let sc = Shortcut::parse("Backspace").unwrap();
        let hits: Vec<_> = b.iter().filter(|b| b.shortcut == sc && b.context == Context::Editor).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "editor.backspace");
    }
}
