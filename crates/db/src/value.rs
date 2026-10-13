//! The value viewer and the grid's aggregate footer: what a cell holds (text, JSON, binary or an
//! image) in a form fit for display, and count/sum/avg/min/max over selected cells.

use serde::{Deserialize, Serialize};

use crate::Cell;

/// Bytes shown in a hex dump (larger values are cut, with a note).
pub const MAX_HEX_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ValueKind {
    Null,
    Text,
    Number,
    Json,
    Binary,
    Image,
}

/// An image recognised from its header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// `png`, `jpeg` or `gif`.
    pub format: String,
    pub width: u32,
    pub height: u32,
}

/// A cell prepared for the value viewer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValueView {
    pub kind: ValueKind,
    /// Text, pretty-printed JSON, or a hex dump of binary values.
    pub text: String,
    /// Size in bytes (UTF-8 for text).
    pub len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageInfo>,
    /// The hex dump stops before the end of the value.
    #[serde(default)]
    pub truncated: bool,
}

/// How the value viewer shows `cell`.
pub fn view(cell: &Cell) -> ValueView {
    let plain =
        |kind: ValueKind, text: String| ValueView { kind, len: text.len(), text, image: None, truncated: false };
    match cell {
        Cell::Null => plain(ValueKind::Null, String::new()),
        Cell::Bytes(b) => {
            let image = image_info(b);
            let truncated = b.len() > MAX_HEX_BYTES;
            ValueView {
                kind: if image.is_some() { ValueKind::Image } else { ValueKind::Binary },
                text: hex_dump(b, MAX_HEX_BYTES),
                len: b.len(),
                image,
                truncated,
            }
        }
        Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) => plain(ValueKind::Number, cell.to_plain()),
        Cell::Bool(_) => plain(ValueKind::Text, cell.to_plain()),
        Cell::Text(s) | Cell::Other(s) => match pretty_json(s) {
            Some(pretty) => {
                ValueView { kind: ValueKind::Json, text: pretty, len: s.len(), image: None, truncated: false }
            }
            None => plain(ValueKind::Text, s.clone()),
        },
    }
}

/// Objects and arrays pretty-printed; anything else (including bare numbers and strings) is not
/// treated as JSON.
pub fn pretty_json(s: &str) -> Option<String> {
    let t = s.trim_start();
    if !(t.starts_with('{') || t.starts_with('[')) || s.len() > 16 * 1024 * 1024 {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    serde_json::to_string_pretty(&v).ok()
}

/// `00000000  89 50 4E 47 0D 0A 1A 0A  00 00 00 0D 49 48 44 52  |.PNG........IHDR|`
pub fn hex_dump(bytes: &[u8], max: usize) -> String {
    let shown = bytes.get(..bytes.len().min(max)).unwrap_or(bytes);
    let mut out = String::with_capacity(shown.len().saturating_mul(4).saturating_add(16));
    for (i, chunk) in shown.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", i.saturating_mul(16)));
        for j in 0..16 {
            match chunk.get(j) {
                Some(b) => out.push_str(&format!("{b:02x} ")),
                None => out.push_str("   "),
            }
            if j == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        out.extend(chunk.iter().map(|b| if b.is_ascii_graphic() || *b == b' ' { char::from(*b) } else { '.' }));
        out.push_str("|\n");
    }
    if bytes.len() > shown.len() {
        out.push_str(&format!("… {} more bytes\n", bytes.len() - shown.len()));
    }
    out
}

/// PNG, JPEG or GIF dimensions from the header.
pub fn image_info(b: &[u8]) -> Option<ImageInfo> {
    let be32 = |at: usize| b.get(at..at + 4).and_then(|s| s.try_into().ok()).map(u32::from_be_bytes);
    let be16 = |at: usize| b.get(at..at + 2).and_then(|s| s.try_into().ok()).map(u16::from_be_bytes);
    let le16 = |at: usize| b.get(at..at + 2).and_then(|s| s.try_into().ok()).map(u16::from_le_bytes);
    let info = |format: &str, width: u32, height: u32| Some(ImageInfo { format: format.into(), width, height });
    if b.starts_with(b"\x89PNG\r\n\x1a\n") && b.get(12..16) == Some(b"IHDR") {
        return info("png", be32(16)?, be32(20)?);
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return info("gif", u32::from(le16(6)?), u32::from(le16(8)?));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        // Walk the segments to the first start-of-frame marker.
        let mut i = 2usize;
        for _ in 0..10_000 {
            if *b.get(i)? != 0xFF {
                return None;
            }
            let marker = *b.get(i + 1)?;
            if marker == 0xFF {
                i += 1;
                continue;
            }
            let len = usize::from(be16(i + 2)?);
            let sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
            if sof {
                return info("jpeg", u32::from(be16(i + 7)?), u32::from(be16(i + 5)?));
            }
            i = i.checked_add(2)?.checked_add(len)?;
        }
    }
    None
}

/// The aggregate footer of a selection.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Aggregates {
    /// Selected cells.
    pub cells: usize,
    /// Non-NULL values.
    pub count: usize,
    /// Values that are numbers (numeric text counts too).
    pub numeric: usize,
    pub sum: Option<f64>,
    pub avg: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

impl Aggregates {
    /// `Count: 3  Sum: 6  Avg: 2  Min: 1  Max: 3` (numbers only when some are numeric).
    pub fn summary(&self) -> String {
        let mut s = format!("Count: {}", self.count);
        for (label, v) in [("Sum", self.sum), ("Avg", self.avg), ("Min", self.min), ("Max", self.max)] {
            if let Some(v) = v {
                s.push_str(&format!("  {label}: {}", format_number(v)));
            }
        }
        s
    }
}

/// A number without float noise: integers plainly, others with up to 6 significant decimals.
pub fn format_number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{v:.0}");
    }
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

fn number(c: &Cell) -> Option<f64> {
    match c {
        Cell::Int(i) => Some(*i as f64),
        Cell::UInt(u) => Some(*u as f64),
        Cell::Float(f) => Some(*f),
        Cell::Text(s) | Cell::Other(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|f| f.is_finite())
}

/// Count, sum, average, minimum and maximum of the selected cells.
pub fn aggregate<'a>(cells: impl IntoIterator<Item = &'a Cell>) -> Aggregates {
    let mut a = Aggregates::default();
    let mut sum = 0.0f64;
    for c in cells {
        a.cells += 1;
        if c.is_null() {
            continue;
        }
        a.count += 1;
        if let Some(n) = number(c) {
            a.numeric += 1;
            sum += n;
            a.min = Some(a.min.map_or(n, |m| m.min(n)));
            a.max = Some(a.max.map_or(n, |m| m.max(n)));
        }
    }
    if a.numeric > 0 {
        a.sum = Some(sum);
        a.avg = Some(sum / a.numeric as f64);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn views_text_json_and_binary() {
        assert_eq!(view(&Cell::Null).kind, ValueKind::Null);
        let j = view(&Cell::Text("{\"a\":[1,2]}".into()));
        assert_eq!(j.kind, ValueKind::Json);
        assert_eq!(j.text, "{\n  \"a\": [\n    1,\n    2\n  ]\n}");
        assert_eq!(view(&Cell::Text("42".into())).kind, ValueKind::Text);
        assert_eq!(view(&Cell::Text("{broken".into())).kind, ValueKind::Text);
        assert_eq!(view(&Cell::Int(7)).kind, ValueKind::Number);
        let b = view(&Cell::Bytes(b"hello, world!\x00\x01\xff more".to_vec()));
        assert_eq!(b.kind, ValueKind::Binary);
        assert!(
            b.text.starts_with("00000000  68 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 00 01 ff  |hello, world!...|\n"),
            "{}",
            b.text
        );
        assert!(b.text.contains("00000010  20 6d 6f 72 65"));
        let big = hex_dump(&[0u8; 40], 32);
        assert!(big.ends_with("… 8 more bytes\n"));
    }

    #[test]
    fn recognises_images() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        png.extend(3u32.to_be_bytes());
        png.extend(2u32.to_be_bytes());
        assert_eq!(image_info(&png), Some(ImageInfo { format: "png".into(), width: 3, height: 2 }));
        assert_eq!(view(&Cell::Bytes(png)).kind, ValueKind::Image);
        let gif = b"GIF89a\x05\x00\x04\x00".to_vec();
        assert_eq!(image_info(&gif).map(|i| (i.width, i.height)), Some((5, 4)));
        // SOI, an APP0 segment, then SOF0 with height 0x0010 and width 0x0020.
        let jpeg =
            [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00, 0x10, 0x00, 0x20];
        assert_eq!(image_info(&jpeg).map(|i| (i.format, i.width, i.height)), Some(("jpeg".into(), 32, 16)));
        assert_eq!(image_info(&[0xFF, 0xD8, 0x00]), None);
        assert_eq!(image_info(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(image_info(&[]), None);
    }

    #[test]
    fn aggregates_numbers_and_skips_nulls() {
        let cells = [Cell::Int(1), Cell::Null, Cell::Float(2.5), Cell::Other("3.5".into()), Cell::Text("x".into())];
        let a = aggregate(&cells);
        assert_eq!((a.cells, a.count, a.numeric), (5, 4, 3));
        assert_eq!((a.sum, a.min, a.max), (Some(7.0), Some(1.0), Some(3.5)));
        assert_eq!(a.summary(), "Count: 4  Sum: 7  Avg: 2.333333  Min: 1  Max: 3.5");
        let none = aggregate(&[Cell::Text("a".into())]);
        assert_eq!(none.summary(), "Count: 1");
        assert_eq!(format_number(0.1 + 0.2), "0.3");
    }
}
