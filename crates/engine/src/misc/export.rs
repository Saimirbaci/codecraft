//! File › Export › Files or Selection to HTML and File › Print: the lexically highlighted text as
//! a standalone HTML page coloured with the editor's colour scheme tokens.

use std::collections::BTreeMap;

use codecraft_syntax::{HighlightKind, Highlights};

/// The colour token of a highlight class (the names user schemes use; see `settings::COLOR_TOKENS`).
pub fn token_of(kind: HighlightKind) -> Option<&'static str> {
    Some(match kind {
        HighlightKind::Keyword | HighlightKind::SelfKeyword | HighlightKind::Boolean | HighlightKind::Heading => {
            "keyword"
        }
        HighlightKind::String | HighlightKind::Char | HighlightKind::Code => "string",
        HighlightKind::Number => "number",
        HighlightKind::Comment => "comment",
        HighlightKind::DocComment => "doc_comment",
        HighlightKind::Function => "function",
        HighlightKind::Macro => "macro",
        HighlightKind::Type => "type",
        HighlightKind::Lifetime => "lifetime",
        HighlightKind::Attribute => "attribute",
        HighlightKind::Constant => "constant",
        HighlightKind::Field | HighlightKind::Key => "field",
        HighlightKind::Link => "link",
        HighlightKind::Invalid => "error",
        _ => return None,
    })
}

const CLASSES: &[&str] = &[
    "keyword",
    "string",
    "number",
    "comment",
    "doc_comment",
    "function",
    "macro",
    "type",
    "lifetime",
    "attribute",
    "constant",
    "field",
    "link",
    "error",
];

fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// A colour value safe to put in CSS (`#RRGGBB` only).
fn css_color(palette: &BTreeMap<String, String>, token: &str) -> Option<String> {
    palette.get(token).and_then(|c| crate::settings::parse_color(c).ok())
}

/// What to export.
pub struct Source<'a> {
    pub title: &'a str,
    pub text: &'a str,
    pub highlights: &'a Highlights,
    /// Zero-based lines to include (a selection exports whole lines).
    pub lines: std::ops::Range<usize>,
    pub line_numbers: bool,
}

/// The HTML page. `palette` maps colour tokens (`editor_bg`, `text_editor`, `keyword`, …) to
/// `#RRGGBB`; missing tokens are left to the browser's defaults.
pub fn to_html(src: &Source<'_>, palette: &BTreeMap<String, String>) -> String {
    let mut css = String::from("body{margin:0}pre{margin:0;padding:12px;font:13px/1.45 ui-monospace,Menlo,monospace;");
    if let Some(bg) = css_color(palette, "editor_bg") {
        css.push_str(&format!("background:{bg};"));
    }
    if let Some(fg) = css_color(palette, "text_editor") {
        css.push_str(&format!("color:{fg};"));
    }
    css.push('}');
    if let Some(ln) = css_color(palette, "line_number") {
        css.push_str(&format!(".ln{{color:{ln};user-select:none}}"));
    } else {
        css.push_str(".ln{opacity:.5;user-select:none}");
    }
    for class in CLASSES {
        if let Some(c) = css_color(palette, class) {
            css.push_str(&format!(".{class}{{color:{c}}}"));
        }
    }
    css.push_str(".comment,.doc_comment{font-style:italic}@media print{pre{white-space:pre-wrap}}");
    let mut body = String::new();
    let all: Vec<&str> = src.text.split('\n').collect();
    let end = src.lines.end.min(all.len());
    let width = end.to_string().len();
    for (i, line) in all.iter().enumerate().take(end).skip(src.lines.start) {
        if src.line_numbers {
            body.push_str(&format!("<span class=\"ln\">{:>width$} </span>", i + 1));
        }
        let chars: Vec<char> = line.chars().collect();
        let mut col = 0usize;
        for span in src.highlights.line(i) {
            let (start, stop) = (span.start.min(chars.len()), span.end.min(chars.len()));
            if start < col || stop <= start {
                continue;
            }
            let text_of = |r: std::ops::Range<usize>| chars.get(r).map(|c| c.iter().collect::<String>()).unwrap_or_default();
            escape(&text_of(col..start), &mut body);
            let piece = text_of(start..stop);
            match token_of(span.kind) {
                Some(class) => {
                    body.push_str(&format!("<span class=\"{class}\">"));
                    escape(&piece, &mut body);
                    body.push_str("</span>");
                }
                None => escape(&piece, &mut body),
            }
            col = stop;
        }
        escape(&chars.get(col..).map(|c| c.iter().collect::<String>()).unwrap_or_default(), &mut body);
        body.push('\n');
    }
    let mut title = String::new();
    escape(src.title, &mut title);
    format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title><style>{css}</style></head>\n<body><pre>{body}</pre></body></html>\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_syntax::Language;

    #[test]
    fn highlights_escapes_and_colours_from_the_palette() {
        let text = "fn main() { let s = \"<b>&\"; }\n// done\n";
        let hl = codecraft_syntax::highlight(Language::Rust, text);
        let palette: BTreeMap<String, String> = [
            ("keyword".to_string(), "#CC7832".to_string()),
            ("editor_bg".to_string(), "#2B2B2B".to_string()),
            ("comment".to_string(), "not a colour; } body { x".to_string()),
        ]
        .into();
        let html = to_html(&Source { title: "a<b>.rs", text, highlights: &hl, lines: 0..3, line_numbers: true }, &palette);
        assert!(html.contains("<title>a&lt;b&gt;.rs</title>"));
        assert!(html.contains(".keyword{color:#CC7832}"));
        assert!(html.contains("background:#2B2B2B"));
        assert!(!html.contains("not a colour"), "invalid colours are dropped");
        assert!(html.contains("<span class=\"keyword\">fn</span>"));
        assert!(html.contains("&quot;&lt;b&gt;&amp;&quot;"));
        assert!(html.contains("<span class=\"comment\">// done</span>"));
        assert!(html.contains("<span class=\"ln\">1 </span>"));
        // A line range exports only those lines; ranges past the end are clamped.
        let html = to_html(&Source { title: "x", text, highlights: &hl, lines: 1..99, line_numbers: false }, &palette);
        assert!(!html.contains("main") && html.contains("done"));
    }
}
