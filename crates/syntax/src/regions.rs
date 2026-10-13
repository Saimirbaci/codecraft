//! Custom folding regions: comment markers that open and close a named, foldable region.
//!
//! Recognized after a line comment (`//`, `#`, `--` or `<!--` by language), at the start of
//! the line (after indentation):
//!
//! - `region Name` … `endregion` (also `#region` / `#endregion`, as VS Code and C# write them);
//! - `<editor-fold desc="Name">` … `</editor-fold>` (NetBeans/IntelliJ style).
//!
//! C and C++ `#pragma region Name` … `#pragma endregion` count too. Regions nest; an
//! unmatched marker is ignored.

use serde::{Deserialize, Serialize};

use crate::Language;

/// A custom folding region: lines `start_line..=end_line` (the marker lines included).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CustomRegion {
    pub start_line: usize,
    pub end_line: usize,
    /// The description after the start marker (may be empty).
    pub name: String,
}

/// Most regions reported for one text (guards against absurd input).
const MAX_REGIONS: usize = 10_000;
/// Longest region name kept (chars).
const MAX_NAME: usize = 200;

/// Line comment prefixes that may carry region markers.
fn comment_prefixes(language: Language) -> &'static [&'static str] {
    match language {
        Language::Rust
        | Language::C
        | Language::Cpp
        | Language::Zig
        | Language::TypeScript
        | Language::JavaScript
        | Language::Json => &["//"],
        Language::Python | Language::Shell | Language::Yaml | Language::Toml | Language::Dockerfile => &["#"],
        Language::Http => &["#", "//"],
        Language::Sql => &["--"],
        Language::Markdown => &["<!--"],
        Language::PlainText => &["//", "#"],
    }
}

enum Marker {
    Start(String),
    End,
}

/// `word` at the start of `s`, followed by whitespace or nothing; returns the rest.
fn keyword<'a>(s: &'a str, word: &str) -> Option<&'a str> {
    let rest = s.strip_prefix(word)?;
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then_some(rest)
}

fn clip_name(s: &str) -> String {
    s.trim().chars().take(MAX_NAME).collect()
}

/// The `desc="…"` attribute of an `<editor-fold …>` tag.
fn fold_desc(attrs: &str) -> String {
    let Some(i) = attrs.find("desc=") else { return String::new() };
    let rest = attrs.get(i + 5..).unwrap_or("");
    let mut chars = rest.chars();
    match chars.next() {
        Some(q @ ('"' | '\'')) => clip_name(&chars.take_while(|c| *c != q).collect::<String>()),
        _ => String::new(),
    }
}

/// The marker in the text after a comment prefix, if any.
fn marker_in_comment(body: &str) -> Option<Marker> {
    let body = body.trim_start();
    let body = body.strip_suffix("-->").unwrap_or(body);
    if let Some(rest) = keyword(body, "region").or_else(|| keyword(body, "#region")) {
        return Some(Marker::Start(clip_name(rest)));
    }
    if keyword(body.trim_end(), "endregion").or_else(|| keyword(body.trim_end(), "#endregion")).is_some() {
        return Some(Marker::End);
    }
    if let Some(rest) = body.strip_prefix("<editor-fold") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('>') {
            return Some(Marker::Start(fold_desc(rest)));
        }
        return None;
    }
    if body.starts_with("</editor-fold") {
        return Some(Marker::End);
    }
    None
}

/// The region marker on a line (indentation allowed before it).
fn marker(language: Language, line: &str) -> Option<Marker> {
    let t = line.trim_start();
    if matches!(language, Language::C | Language::Cpp)
        && let Some(rest) = t.strip_prefix('#')
    {
        let rest = rest.trim_start();
        if let Some(p) = keyword(rest, "pragma") {
            let p = p.trim_start();
            if let Some(name) = keyword(p, "region") {
                return Some(Marker::Start(clip_name(name)));
            }
            if keyword(p.trim_end(), "endregion").is_some() {
                return Some(Marker::End);
            }
        }
        return None;
    }
    comment_prefixes(language).iter().find_map(|p| {
        let body = t.strip_prefix(p)?;
        // `///` and `//!` are doc comments, not markers.
        if *p == "//" && (body.starts_with('/') || body.starts_with('!')) {
            return None;
        }
        marker_in_comment(body)
    })
}

/// Custom folding regions of `text`, sorted by start line (outer regions first).
pub fn custom_regions(language: Language, text: &str) -> Vec<CustomRegion> {
    let mut open: Vec<(usize, String)> = Vec::new();
    let mut out = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        // Cheap pre-check: every marker contains one of these.
        if !(line.contains("region") || line.contains("editor-fold")) {
            continue;
        }
        match marker(language, line) {
            Some(Marker::Start(name)) => {
                if open.len() < MAX_REGIONS {
                    open.push((i, name));
                }
            }
            Some(Marker::End) => {
                if let Some((start, name)) = open.pop()
                    && i > start
                    && out.len() < MAX_REGIONS
                {
                    out.push(CustomRegion { start_line: start, end_line: i, name });
                }
            }
            None => {}
        }
    }
    out.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(b.end_line.cmp(&a.end_line)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(language: Language, text: &str) -> Vec<(usize, usize, String)> {
        custom_regions(language, text).into_iter().map(|r| (r.start_line, r.end_line, r.name)).collect()
    }

    #[test]
    fn rust_line_comment_regions_nest() {
        let text = "// region Setup\nfn a() {}\n    // region inner part\n    fn b() {}\n    // endregion\n// endregion\n";
        assert_eq!(spans(Language::Rust, text), vec![(0, 5, "Setup".into()), (2, 4, "inner part".into())]);
    }

    #[test]
    fn hash_region_and_editor_fold_forms() {
        let py = "#region Imports\nimport os\n#endregion\n# region Two\nx = 1\n# endregion\n";
        assert_eq!(spans(Language::Python, py), vec![(0, 2, "Imports".into()), (3, 5, "Two".into())]);
        let ts = "//#region helpers\nconst a = 1;\n//#endregion\n";
        assert_eq!(spans(Language::TypeScript, ts), vec![(0, 2, "helpers".into())]);
        let js = "// <editor-fold desc=\"Getters\">\nget() {}\n// </editor-fold>\n";
        assert_eq!(spans(Language::JavaScript, js), vec![(0, 2, "Getters".into())]);
        let zig = "// <editor-fold>\nconst a = 1;\n// </editor-fold>\n";
        assert_eq!(spans(Language::Zig, zig), vec![(0, 2, String::new())]);
        let sql = "-- region q\nSELECT 1;\n-- endregion\n";
        assert_eq!(spans(Language::Sql, sql), vec![(0, 2, "q".into())]);
        let md = "<!-- #region notes -->\ntext\n<!-- #endregion -->\n";
        assert_eq!(spans(Language::Markdown, md), vec![(0, 2, "notes".into())]);
    }

    #[test]
    fn c_pragma_and_comment_regions() {
        let c = "#pragma region Api\nint f();\n#pragma endregion\n// region more\nint g();\n// endregion\n";
        assert_eq!(spans(Language::C, c), vec![(0, 2, "Api".into()), (3, 5, "more".into())]);
    }

    #[test]
    fn near_misses_are_not_markers() {
        let text = "// regional office\n/// region doc\n//! region doc\nlet region = 1;\n// endregion\n";
        assert!(spans(Language::Rust, text).is_empty());
        // `#region` is not a comment in Rust.
        assert!(spans(Language::Rust, "#region a\nx\n#endregion\n").is_empty());
        // Unmatched markers.
        assert!(spans(Language::Rust, "// endregion\n// region a\nx\n").is_empty());
        assert!(spans(Language::Rust, "").is_empty());
    }

    #[test]
    fn regions_become_fold_ranges() {
        let text = "// region A\nfn a() {}\nfn b() {}\n// endregion\n";
        let folds = crate::folding_ranges(Language::Rust, text);
        assert!(folds.contains(&crate::FoldRange { start_line: 0, end_line: 3, kind: crate::FoldKind::Region }));
        // The incremental highlighter's folds agree.
        let ih = crate::IncrementalHighlighter::new(Language::Rust, text);
        assert_eq!(ih.folding_ranges(text), folds);
        let py = "# region A\nx = 1\n# endregion\n";
        assert!(crate::folding_ranges(Language::Python, py).iter().any(|f| f.kind == crate::FoldKind::Region));
    }

    #[test]
    fn hostile_input_is_bounded() {
        let many = "// region x\n".repeat(20_000);
        assert!(custom_regions(Language::Rust, &many).is_empty());
        let deep = format!("{}{}", "// region x\n".repeat(20_000), "// endregion\n".repeat(20_000));
        assert!(custom_regions(Language::Rust, &deep).len() <= MAX_REGIONS);
        let long = format!("// region {}\nx\n// endregion", "n".repeat(10_000));
        assert_eq!(custom_regions(Language::Rust, &long)[0].name.chars().count(), MAX_NAME);
        assert!(custom_regions(Language::JavaScript, "// <editor-fold desc=\"unterminated\n// </editor-fold>").len() == 1);
    }
}
