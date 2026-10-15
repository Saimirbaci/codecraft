//! Colour scheme files (Settings › Editor › Color Scheme › Export / Import): CodeCraft's own
//! JSON format, and import of an IntelliJ-format `.icls` file the user has (for example one they
//! exported themselves). Only the file the user picks is parsed; no scheme files are bundled.
//!
//! An `.icls` file is XML: `<scheme name=… parent_scheme=…>` with `<colors><option name=…
//! value=…/>` and `<attributes><option name=…><value><option name="FOREGROUND" value=…/>…`.
//! Attribute keys that correspond to one of our colour tokens are mapped (the key names are
//! the public ones of the IntelliJ Platform's open-source `TextAttributesKey`s and colour keys);
//! everything else is ignored, and the scheme's base is its parent scheme (Darcula → Dark,
//! Default → Light) or, without one, whichever built-in its editor background is closest to.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use codecraft_workspace::ColorScheme;

use crate::settings::{self, COLOR_TOKENS};

/// The JSON format's identifier.
pub const FORMAT: &str = "codecraft-color-scheme/1";
/// Largest scheme file read.
pub const MAX_FILE: u64 = 4 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct SchemeFile {
    #[serde(default)]
    format: String,
    name: String,
    #[serde(default)]
    base: String,
    #[serde(default)]
    colors: BTreeMap<String, String>,
}

/// The JSON text of a scheme.
pub fn to_json(s: &ColorScheme) -> Result<String, String> {
    let f = SchemeFile { format: FORMAT.into(), name: s.name.clone(), base: s.base.clone(), colors: s.colors.clone() };
    serde_json::to_string_pretty(&f).map_err(|e| e.to_string())
}

/// What an import found: the scheme plus what was left out.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Imported {
    pub scheme: ColorScheme,
    /// Colours of tokens we have.
    pub mapped: usize,
    /// Attribute and colour keys with no corresponding token (or unreadable values).
    pub skipped: usize,
}

/// Parse a scheme file by its extension (`.icls`/`.xml` as IntelliJ XML, otherwise our JSON).
pub fn import_file(path: &Path) -> Result<Imported, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.len() > MAX_FILE {
        return Err(format!("{} is too large for a colour scheme", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if ext == "icls" || ext == "xml" || text.trim_start().starts_with('<') {
        from_icls(&text)
    } else {
        from_json(&text)
    }
}

/// Parse our JSON format (unknown tokens and bad colours are skipped and counted).
pub fn from_json(text: &str) -> Result<Imported, String> {
    let f: SchemeFile = serde_json::from_str(text).map_err(|e| format!("not a CodeCraft colour scheme: {e}"))?;
    if !f.format.is_empty() && f.format != FORMAT {
        return Err(format!("unsupported colour scheme format `{}`", f.format));
    }
    let base = settings::canonical_theme(&f.base).unwrap_or("Dark").to_string();
    let mut colors = BTreeMap::new();
    let mut skipped = 0;
    for (k, v) in f.colors {
        match settings::parse_color(&v) {
            Ok(c) if COLOR_TOKENS.contains(&k.as_str()) => {
                colors.insert(k, c);
            }
            _ => skipped += 1,
        }
    }
    let mapped = colors.len();
    Ok(Imported { scheme: ColorScheme { name: f.name.trim().to_string(), base, colors }, mapped, skipped })
}

/// `.icls` colour keys → our tokens.
const ICLS_COLORS: &[(&str, &str)] = &[
    ("CARET_COLOR", "caret"),
    ("CARET_ROW_COLOR", "caret_row"),
    ("SELECTION_BACKGROUND", "selection"),
    ("LINE_NUMBERS_COLOR", "line_number"),
    ("LINE_NUMBER_ON_CARET_ROW_COLOR", "line_number_active"),
    ("INDENT_GUIDE", "indent_guide"),
    ("WHITESPACES", "whitespace"),
    ("RIGHT_MARGIN_COLOR", "right_margin"),
    ("ADDED_LINES_COLOR", "vcs_gutter_added"),
    ("MODIFIED_LINES_COLOR", "vcs_gutter_modified"),
    ("DELETED_LINES_COLOR", "vcs_gutter_deleted"),
    ("ANNOTATIONS_COLOR", "annotation"),
    ("FILESTATUS_MODIFIED", "vcs_modified"),
    ("FILESTATUS_ADDED", "vcs_added"),
    ("FILESTATUS_DELETED", "vcs_deleted"),
    ("FILESTATUS_UNKNOWN", "vcs_untracked"),
    ("FILESTATUS_IDEA_FILESTATUS_IGNORED", "vcs_ignored"),
    ("FILESTATUS_MERGED", "vcs_conflict"),
];

/// Which part of a text attribute a token takes.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    Fg,
    Bg,
    /// The effect (underline/wave) colour, else the error stripe, else the foreground.
    Effect,
}

/// `.icls` text attribute keys → our tokens.
const ICLS_ATTRIBUTES: &[(&str, Part, &str)] = &[
    ("TEXT", Part::Bg, "editor_bg"),
    ("TEXT", Part::Fg, "text_editor"),
    ("DEFAULT_KEYWORD", Part::Fg, "keyword"),
    ("DEFAULT_STRING", Part::Fg, "string"),
    ("DEFAULT_NUMBER", Part::Fg, "number"),
    ("DEFAULT_LINE_COMMENT", Part::Fg, "comment"),
    ("DEFAULT_BLOCK_COMMENT", Part::Fg, "comment"),
    ("DEFAULT_DOC_COMMENT", Part::Fg, "doc_comment"),
    ("DEFAULT_FUNCTION_DECLARATION", Part::Fg, "function"),
    ("DEFAULT_METADATA", Part::Fg, "attribute"),
    ("DEFAULT_CONSTANT", Part::Fg, "constant"),
    ("DEFAULT_INSTANCE_FIELD", Part::Fg, "field"),
    ("DEFAULT_VALID_STRING_ESCAPE", Part::Fg, "escape"),
    ("DEFAULT_CLASS_NAME", Part::Fg, "type"),
    ("org.rust.MACRO", Part::Fg, "macro"),
    ("org.rust.LIFETIME", Part::Fg, "lifetime"),
    ("org.rust.ATTRIBUTE", Part::Fg, "attribute"),
    ("org.rust.FUNCTION", Part::Fg, "function"),
    ("org.rust.STRUCT", Part::Fg, "type"),
    ("ERRORS_ATTRIBUTES", Part::Effect, "error"),
    ("WARNING_ATTRIBUTES", Part::Effect, "warning"),
    ("INFO_ATTRIBUTES", Part::Effect, "weak"),
    ("TEXT_SEARCH_RESULT_ATTRIBUTES", Part::Bg, "find_match"),
    ("SEARCH_RESULT_ATTRIBUTES", Part::Bg, "find_match"),
    ("MATCHED_BRACE_ATTRIBUTES", Part::Bg, "bracket_match"),
    ("IDENTIFIER_UNDER_CARET_ATTRIBUTES", Part::Bg, "occurrence"),
    ("HYPERLINK_ATTRIBUTES", Part::Fg, "link"),
    ("BREAKPOINT_ATTRIBUTES", Part::Bg, "breakpoint"),
    ("EXECUTIONPOINT_ATTRIBUTES", Part::Bg, "execution_line"),
    ("INLAY_DEFAULT", Part::Fg, "inlay_text"),
    ("INLAY_DEFAULT", Part::Bg, "inlay_bg"),
    ("CONSOLE_ERROROUTPUT", Part::Fg, "console_stderr"),
    ("CONSOLE_SYSTEM_OUTPUT", Part::Fg, "console_system"),
    ("DEBUGGER_INLINED_VALUES", Part::Fg, "inline_value"),
    ("DIFF_INSERTED", Part::Bg, "diff_added_bg"),
    ("DIFF_DELETED", Part::Bg, "diff_removed_bg"),
    ("DIFF_MODIFIED", Part::Bg, "diff_modified_bg"),
];

/// An `.icls` colour value (`a9b7c6`, `0`, `8000` with leading zeros dropped, or 8 digits
/// with alpha) → `#RRGGBB`.
fn icls_color(v: &str) -> Option<String> {
    let v = v.trim().trim_start_matches('#');
    if v.is_empty() || v.len() > 8 || !v.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let hex = if v.len() > 6 { v.get(..6)?.to_string() } else { format!("{v:0>6}") };
    settings::parse_color(&hex).ok()
}

/// One XML start (or empty) tag: name and attributes. Text, comments and end tags are skipped
/// by [`tags`]; `depth` is the element depth of the tag.
#[derive(Debug)]
struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
    depth: usize,
}

impl Tag {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// The start tags of an XML text in order (a tolerant tokenizer: malformed parts are skipped).
fn tags(text: &str) -> Vec<Tag> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        rest = rest.get(i + 1..).unwrap_or("");
        if let Some(r) = rest.strip_prefix("!--") {
            rest = r.find("-->").and_then(|e| r.get(e + 3..)).unwrap_or("");
            continue;
        }
        if rest.starts_with('?') || rest.starts_with('!') {
            rest = rest.find('>').and_then(|e| rest.get(e + 1..)).unwrap_or("");
            continue;
        }
        if let Some(r) = rest.strip_prefix('/') {
            depth = depth.saturating_sub(1);
            rest = r.find('>').and_then(|e| r.get(e + 1..)).unwrap_or("");
            continue;
        }
        // The tag's end: the first `>` outside a quoted value.
        let mut quote: Option<char> = None;
        let mut end = None;
        for (j, c) in rest.char_indices() {
            match (quote, c) {
                (None, '"' | '\'') => quote = Some(c),
                (Some(q), c) if c == q => quote = None,
                (None, '>') => {
                    end = Some(j);
                    break;
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        let body = rest.get(..end).unwrap_or("");
        rest = rest.get(end + 1..).unwrap_or("");
        let empty = body.trim_end().ends_with('/');
        let body = body.trim_end().trim_end_matches('/');
        let name: String = body.chars().take_while(|c| !c.is_whitespace()).collect();
        let mut attrs = Vec::new();
        let mut a = body.get(name.len()..).unwrap_or("");
        while let Some(eq) = a.find('=') {
            let key = a.get(..eq).unwrap_or("").trim().to_string();
            let after = a.get(eq + 1..).unwrap_or("").trim_start();
            let Some(q) = after.chars().next().filter(|c| *c == '"' || *c == '\'') else { break };
            let inner = after.get(1..).unwrap_or("");
            let Some(close) = inner.find(q) else { break };
            attrs.push((key, unescape(inner.get(..close).unwrap_or(""))));
            a = inner.get(close + 1..).unwrap_or("");
            if attrs.len() > 64 {
                break;
            }
        }
        out.push(Tag { name, attrs, depth });
        if !empty {
            depth += 1;
        }
        if out.len() > 200_000 {
            break;
        }
    }
    out
}

fn luminance(hex: &str) -> f32 {
    let n = u32::from_str_radix(hex.trim_start_matches('#'), 16).unwrap_or(0);
    let (r, g, b) = ((n >> 16) & 0xff, (n >> 8) & 0xff, n & 0xff);
    (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0
}

/// Parse an `.icls` scheme.
pub fn from_icls(text: &str) -> Result<Imported, String> {
    let all = tags(text);
    let scheme = all.iter().find(|t| t.name == "scheme").ok_or("not an .icls colour scheme (no <scheme> element)")?;
    let name = scheme.attr("name").unwrap_or("Imported").trim().to_string();
    let parent = scheme.attr("parent_scheme").unwrap_or_default().to_ascii_lowercase();
    let mut colors: BTreeMap<String, String> = BTreeMap::new();
    let mut skipped = 0usize;
    // Section and attribute currently inside (`colors` / `attributes`, then the attribute key).
    let mut section = "";
    let mut attr: Option<(String, usize)> = None;
    let mut parts: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for t in &all {
        if let Some((_, d)) = &attr
            && t.depth <= *d
        {
            attr = None;
        }
        match t.name.as_str() {
            "colors" => section = "colors",
            "attributes" => section = "attributes",
            "option" if section == "colors" && attr.is_none() => {
                let (Some(k), Some(v)) = (t.attr("name"), t.attr("value")) else { continue };
                match (ICLS_COLORS.iter().find(|(key, _)| *key == k), icls_color(v)) {
                    (Some((_, token)), Some(c)) => {
                        colors.insert((*token).to_string(), c);
                    }
                    _ => skipped += 1,
                }
            }
            "option" if section == "attributes" => match &attr {
                None => {
                    if let Some(k) = t.attr("name") {
                        attr = Some((k.to_string(), t.depth));
                        parts.entry(k.to_string()).or_default();
                    }
                }
                Some((key, _)) => {
                    if let (Some(part), Some(v)) = (t.attr("name"), t.attr("value"))
                        && let Some(c) = icls_color(v)
                    {
                        parts.entry(key.clone()).or_default().insert(part.to_string(), c);
                    }
                }
            },
            _ => {}
        }
    }
    skipped += parts.keys().filter(|k| !ICLS_ATTRIBUTES.iter().any(|(a, _, _)| a == k)).count();
    // In table order, so the first key that gives a token a colour wins (line comments before
    // block comments).
    for (key, part, token) in ICLS_ATTRIBUTES {
        let Some(values) = parts.get(*key) else { continue };
        {
            let v = match part {
                Part::Fg => values.get("FOREGROUND"),
                Part::Bg => values.get("BACKGROUND"),
                Part::Effect => values
                    .get("EFFECT_COLOR")
                    .or_else(|| values.get("ERROR_STRIPE_COLOR"))
                    .or_else(|| values.get("FOREGROUND")),
            };
            if let Some(v) = v
                && !colors.contains_key(*token)
            {
                colors.insert((*token).to_string(), v.clone());
            }
        }
    }
    let base = if parent.contains("darcula") || parent.contains("dark") {
        "Dark"
    } else if parent.contains("contrast") {
        "High Contrast"
    } else if parent == "default" || parent.contains("light") {
        "Light"
    } else {
        match colors.get("editor_bg") {
            Some(bg) if luminance(bg) > 0.5 => "Light",
            _ => "Dark",
        }
    };
    let mapped = colors.len();
    Ok(Imported { scheme: ColorScheme { name, base: base.into(), colors }, mapped, skipped })
}

/// A scheme name that is free among `taken` (`Name`, `Name (2)`, …); built-in names count.
pub fn free_name(wanted: &str, taken: &[String]) -> String {
    let base = if wanted.trim().is_empty() { "Imported" } else { wanted.trim() };
    let base: String = base.chars().take(70).collect();
    let used = |n: &str| {
        taken.iter().any(|t| t == n) || settings::BUILTIN_SCHEMES.iter().any(|b| b.eq_ignore_ascii_case(n))
    };
    (1..=1000)
        .map(|i| if i == 1 { base.clone() } else { format!("{base} ({i})") })
        .find(|n| !used(n))
        .unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ICLS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- the user's own export -->
<scheme name="My Night &amp; Day" version="142" parent_scheme="Darcula">
  <metaInfo><property name="ide">RustRover</property></metaInfo>
  <colors>
    <option name="CARET_COLOR" value="ffcc00" />
    <option name="CARET_ROW_COLOR" value="2a2a2a"/>
    <option name="SOMETHING_ELSE" value="123456" />
    <option name="LINE_NUMBERS_COLOR" value="8000" />
  </colors>
  <attributes>
    <option name="TEXT">
      <value>
        <option name="FOREGROUND" value="a9b7c6" />
        <option name="BACKGROUND" value="1e1f22" />
      </value>
    </option>
    <option name="DEFAULT_KEYWORD">
      <value><option name="FOREGROUND" value="cc7832" /><option name="FONT_TYPE" value="1" /></value>
    </option>
    <option name="DEFAULT_LINE_COMMENT"><value><option name="FOREGROUND" value="808080" /></value></option>
    <option name="DEFAULT_BLOCK_COMMENT"><value><option name="FOREGROUND" value="ff0000" /></value></option>
    <option name="ERRORS_ATTRIBUTES"><value><option name="EFFECT_COLOR" value="bc3f3c" /><option name="EFFECT_TYPE" value="2" /></value></option>
    <option name="DEFAULT_IDENTIFIER" baseAttributes="TEXT" />
    <option name="UNKNOWN_KEY"><value><option name="FOREGROUND" value="zzzzzz" /></value></option>
  </attributes>
</scheme>"#;

    #[test]
    fn icls_maps_known_keys_to_tokens() {
        let i = from_icls(ICLS).unwrap();
        let c = &i.scheme.colors;
        assert_eq!(i.scheme.name, "My Night & Day");
        assert_eq!(i.scheme.base, "Dark");
        assert_eq!(c.get("caret").map(String::as_str), Some("#FFCC00"));
        assert_eq!(c.get("line_number").map(String::as_str), Some("#008000"), "leading zeros restored");
        assert_eq!(c.get("editor_bg").map(String::as_str), Some("#1E1F22"));
        assert_eq!(c.get("text_editor").map(String::as_str), Some("#A9B7C6"));
        assert_eq!(c.get("keyword").map(String::as_str), Some("#CC7832"));
        assert_eq!(c.get("comment").map(String::as_str), Some("#808080"), "line comments win over block comments");
        assert_eq!(c.get("error").map(String::as_str), Some("#BC3F3C"));
        assert!(i.skipped >= 2, "{i:?}");
        assert!(settings::validate_schemes(std::slice::from_ref(&i.scheme)).is_ok());
    }

    #[test]
    fn icls_base_from_background_and_hostile_input() {
        let light = r#"<scheme name="Paper"><attributes><option name="TEXT"><value><option name="BACKGROUND" value="ffffff"/></value></option></attributes></scheme>"#;
        assert_eq!(from_icls(light).unwrap().scheme.base, "Light");
        assert!(from_icls("<html><body/></html>").is_err());
        assert!(from_icls("").is_err());
        for junk in ["<scheme name=\"x", "<<<>>>", "<scheme name='a' <colors><option name=\"CARET_COLOR\" value=\"\u{0}\"", "<!-- never closed"] {
            let _ = from_icls(junk);
        }
        assert_eq!(icls_color("0"), Some("#000000".into()));
        assert_eq!(icls_color("ff00ff80"), Some("#FF00FF".into()));
        assert_eq!(icls_color("nothex"), None);
        assert_eq!(icls_color("123456789"), None);
    }

    #[test]
    fn json_round_trip_and_names() {
        let mut colors = BTreeMap::new();
        colors.insert("keyword".to_string(), "#FF0000".to_string());
        let s = ColorScheme { name: "Mine".into(), base: "Light".into(), colors };
        let text = to_json(&s).unwrap();
        let back = from_json(&text).unwrap();
        assert_eq!(back.scheme, s);
        let partial =
            from_json(r##"{"name": "X", "base": "sepia", "colors": {"keyword": "red", "nope": "#000000", "string": "00ff00"}}"##)
                .unwrap();
        assert_eq!(partial.scheme.base, "Dark");
        assert_eq!(partial.scheme.colors.len(), 1);
        assert_eq!(partial.skipped, 2);
        assert!(from_json("{").is_err());
        assert!(from_json(r#"{"format": "other/9", "name": "x"}"#).is_err());
        assert_eq!(free_name("Dark", &[]), "Dark (2)");
        assert_eq!(free_name("Mine", &["Mine".into(), "Mine (2)".into()]), "Mine (3)");
        assert_eq!(free_name("  ", &[]), "Imported");
    }
}
