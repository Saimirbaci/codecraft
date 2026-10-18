//! Keyboard and mouse input → the byte sequences an xterm-compatible terminal sends.

use serde::{Deserialize, Serialize};

/// A key that is not plain text (text goes to the PTY as UTF-8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Key {
    /// A character key, already shifted (`'a'`, `'A'`, `'['`).
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1..F12.
    F(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mods {
    pub ctrl: bool,
    /// Alt/Option as Meta (an ESC prefix).
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Self = Self { ctrl: false, alt: false, shift: false };

    pub fn is_none(self) -> bool {
        !self.ctrl && !self.alt && !self.shift
    }

    /// xterm's modifier parameter (`CSI 1;<m>A`): 1 + shift + 2·alt + 4·ctrl.
    fn param(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }
}

/// Parse `Ctrl+C`, `Shift+Tab`, `Alt+b`, `Up`, `F5`, `Enter`, `Ctrl+Shift+Left`, `Space`.
/// Names are case-insensitive; a single character is taken literally.
pub fn parse_key(s: &str) -> Option<(Key, Mods)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (mods_part, key_part) = match s.strip_suffix("++") {
        Some(m) => (m, "+"),
        None if s == "+" => ("", "+"),
        None => match s.rsplit_once('+') {
            Some((m, k)) => (m, k),
            None => ("", s),
        },
    };
    let mut mods = Mods::NONE;
    for m in mods_part.split('+').filter(|m| !m.is_empty()) {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "option" | "meta" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
    }
    let mut chars = key_part.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        let c = if mods.shift && c.is_ascii_lowercase() { c.to_ascii_uppercase() } else { c };
        return Some((Key::Char(c), mods));
    }
    let lower = key_part.to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "escape" | "esc" => Key::Escape,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "insert" => Key::Insert,
        "delete" | "del" => Key::Delete,
        "space" => Key::Char(' '),
        other => {
            let n: u8 = other.strip_prefix('f')?.parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            Key::F(n)
        }
    };
    Some((key, mods))
}

/// The bytes for a key press. `app_cursor` is DECCKM (application cursor keys).
pub fn encode_key(key: Key, mods: Mods, app_cursor: bool) -> Option<Vec<u8>> {
    const ESC: u8 = 0x1b;
    let with_meta = |mut bytes: Vec<u8>| {
        if mods.alt {
            bytes.insert(0, ESC);
        }
        bytes
    };
    let csi_letter = |letter: u8| -> Vec<u8> {
        if mods.is_none() {
            vec![ESC, if app_cursor { b'O' } else { b'[' }, letter]
        } else {
            format!("\x1b[1;{}{}", mods.param(), letter as char).into_bytes()
        }
    };
    let csi_tilde = |n: u8| -> Vec<u8> {
        if mods.is_none() {
            format!("\x1b[{n}~").into_bytes()
        } else {
            format!("\x1b[{n};{}~", mods.param()).into_bytes()
        }
    };
    Some(match key {
        Key::Char(c) => {
            if mods.ctrl {
                with_meta(vec![ctrl_byte(c)?])
            } else {
                let mut buf = [0u8; 4];
                with_meta(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        // Shift+Enter is a line feed (Ctrl+J), as in Zed and other terminals: Claude Code and
        // other prompt-style programs insert a new line for it; shells run the line as for Enter.
        Key::Enter if mods.shift && !mods.ctrl => with_meta(vec![b'\n']),
        Key::Enter => with_meta(vec![b'\r']),
        Key::Tab if mods.shift => b"\x1b[Z".to_vec(),
        Key::Tab => with_meta(vec![b'\t']),
        Key::Backspace if mods.ctrl => with_meta(vec![0x08]),
        Key::Backspace => with_meta(vec![0x7f]),
        Key::Escape => with_meta(vec![ESC]),
        Key::Up => csi_letter(b'A'),
        Key::Down => csi_letter(b'B'),
        Key::Right => csi_letter(b'C'),
        Key::Left => csi_letter(b'D'),
        Key::Home => csi_letter(b'H'),
        Key::End => csi_letter(b'F'),
        Key::Insert => csi_tilde(2),
        Key::Delete => csi_tilde(3),
        Key::PageUp => csi_tilde(5),
        Key::PageDown => csi_tilde(6),
        Key::F(n @ 1..=4) => {
            let letter = b"PQRS".get(usize::from(n - 1)).copied()?;
            if mods.is_none() {
                vec![ESC, b'O', letter]
            } else {
                format!("\x1b[1;{}{}", mods.param(), letter as char).into_bytes()
            }
        }
        Key::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return None,
            };
            csi_tilde(code)
        }
    })
}

/// The control character for Ctrl+`c` (Ctrl+A = 0x01 … Ctrl+Z = 0x1a, Ctrl+[ = ESC, …).
fn ctrl_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => (c as u8) - b'a' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// Motion with no button held (any-event tracking).
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseAction {
    Press,
    Release,
    /// Pointer motion (with `button` held, or `MouseButton::None`).
    Motion,
}

/// A mouse report for programs that enabled mouse tracking. `col`/`row` are zero-based cells;
/// `sgr` selects the SGR (1006) encoding. `None` when the position cannot be encoded.
pub fn encode_mouse(
    button: MouseButton,
    action: MouseAction,
    col: usize,
    row: usize,
    mods: Mods,
    sgr: bool,
) -> Option<Vec<u8>> {
    let base: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    let mut code = base;
    if action == MouseAction::Motion {
        code += 32;
    }
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.ctrl {
        code += 16;
    }
    let x = col.checked_add(1)?;
    let y = row.checked_add(1)?;
    if sgr {
        let end = if action == MouseAction::Release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }
    // X10/normal encoding: release is button 3; coordinates must fit one byte.
    if action == MouseAction::Release {
        code = 3 + (code & !3);
    }
    let enc = |v: usize| -> Option<u8> { u8::try_from(v.checked_add(32)?).ok() };
    Some(vec![0x1b, b'[', b'M', u8::try_from(code + 32).ok()?, enc(x)?, enc(y)?])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(s: &str) -> Vec<u8> {
        let (k, m) = parse_key(s).unwrap();
        encode_key(k, m, false).unwrap()
    }

    #[test]
    fn control_keys() {
        assert_eq!(key("Ctrl+C"), vec![3]);
        assert_eq!(key("Ctrl+d"), vec![4]);
        assert_eq!(key("Ctrl+Z"), vec![0x1a]);
        assert_eq!(key("Ctrl+["), vec![0x1b]);
        assert_eq!(key("Ctrl+Space"), vec![0]);
        assert_eq!(key("Enter"), b"\r");
        assert_eq!(key("Shift+Enter"), b"\n", "a new line in Claude Code, not a submit");
        assert_eq!(key("Alt+Enter"), b"\x1b\r");
        assert_eq!(key("Ctrl+Shift+Enter"), b"\r");
        assert_eq!(key("Tab"), b"\t");
        assert_eq!(key("Shift+Tab"), b"\x1b[Z");
        assert_eq!(key("Backspace"), vec![0x7f]);
        assert_eq!(key("Escape"), vec![0x1b]);
        assert_eq!(key("Alt+b"), b"\x1bb");
        assert_eq!(key("Shift+a"), b"A");
        assert_eq!(key("+"), b"+");
        let (k, m) = parse_key("Ctrl++").unwrap();
        assert_eq!((k, m.ctrl), (Key::Char('+'), true));
        assert!(encode_key(k, m, false).is_none(), "Ctrl++ has no control character");
    }

    #[test]
    fn cursor_and_function_keys() {
        assert_eq!(key("Up"), b"\x1b[A");
        assert_eq!(encode_key(Key::Up, Mods::NONE, true).unwrap(), b"\x1bOA");
        assert_eq!(key("Ctrl+Left"), b"\x1b[1;5D");
        assert_eq!(key("Shift+Alt+Right"), b"\x1b[1;4C");
        assert_eq!(key("Home"), b"\x1b[H");
        assert_eq!(key("Delete"), b"\x1b[3~");
        assert_eq!(key("Shift+PageUp"), b"\x1b[5;2~");
        assert_eq!(key("F1"), b"\x1bOP");
        assert_eq!(key("Ctrl+F1"), b"\x1b[1;5P");
        assert_eq!(key("F5"), b"\x1b[15~");
        assert_eq!(key("F12"), b"\x1b[24~");
        assert!(parse_key("F13").is_none());
        assert!(parse_key("Hyper+X").is_none());
        assert!(parse_key("").is_none());
    }

    #[test]
    fn mouse_reports() {
        let sgr = encode_mouse(MouseButton::Left, MouseAction::Press, 0, 0, Mods::NONE, true).unwrap();
        assert_eq!(sgr, b"\x1b[<0;1;1M");
        let rel = encode_mouse(MouseButton::Left, MouseAction::Release, 9, 4, Mods::NONE, true).unwrap();
        assert_eq!(rel, b"\x1b[<0;10;5m");
        let wheel = encode_mouse(MouseButton::WheelDown, MouseAction::Press, 2, 3, Mods::NONE, false).unwrap();
        assert_eq!(wheel, vec![0x1b, b'[', b'M', 65 + 32, 3 + 32, 4 + 32]);
        assert!(encode_mouse(MouseButton::Left, MouseAction::Press, 500, 0, Mods::NONE, false).is_none());
        assert!(encode_mouse(MouseButton::Left, MouseAction::Press, usize::MAX, 0, Mods::NONE, true).is_none());
    }
}
