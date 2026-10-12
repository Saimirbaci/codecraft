//! Shortcut parsing, matching and display (RustRover macOS keymap notation: `Shift+Cmd+F`).

use egui::{Key, Modifiers};

use codecraft_engine::{Context, registry};

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

/// macOS menu glyphs: `⇧⌘F`.
pub fn display(s: &str) -> String {
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

/// All bindings: commands' own shortcuts take precedence over catalog shortcuts for the same
/// key in the same context (e.g. Backspace is the editor's backspace, not Edit › Delete).
pub fn bindings() -> Vec<Binding> {
    let reg = registry();
    let mut out: Vec<Binding> = Vec::new();
    let add = |sc: Shortcut, id: &'static str, context: Context, out: &mut Vec<Binding>| {
        if !out.iter().any(|b| b.shortcut == sc && b.context == context) {
            out.push(Binding { shortcut: sc, id, context });
        }
    };
    // Pass 1: explicit spec shortcuts.
    for spec in reg.all() {
        for s in spec.shortcut.iter().chain(spec.alt_shortcuts.iter()) {
            if let Some(sc) = Shortcut::parse(s) {
                add(sc, spec.id, spec.context, &mut out);
            }
        }
    }
    // Pass 2: catalog shortcuts of registered commands.
    for spec in reg.all().iter().filter(|s| s.shortcut.is_none()) {
        if let Some(sc) = reg.catalog_item(spec.id).and_then(|c| c.shortcut).and_then(Shortcut::parse) {
            add(sc, spec.id, spec.context, &mut out);
        }
    }
    out
}

/// The shortcut text shown next to a command in menus.
pub fn shortcut_label(id: &str) -> Option<String> {
    registry().shortcuts(id).first().map(|s| display(s))
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
    fn backspace_is_the_editor_backspace() {
        let b = bindings();
        let sc = Shortcut::parse("Backspace").unwrap();
        let hits: Vec<_> = b.iter().filter(|b| b.shortcut == sc && b.context == Context::Editor).collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "editor.backspace");
    }
}
