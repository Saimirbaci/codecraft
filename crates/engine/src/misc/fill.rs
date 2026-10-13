//! Edit › Fill Paragraph: rewrap the comment (or plain-text) paragraph at the caret to the right
//! margin, keeping each line's indent and comment marker.

use std::ops::Range;

use codecraft_text::Document;

/// A line split into its prefix (indent + comment marker + one space) and its text.
#[derive(Debug, PartialEq, Eq)]
struct Parts {
    /// Indent and marker (`    ///`), which lines of one paragraph share.
    key: String,
    /// What precedes the text when writing a line (`    /// `).
    prefix: String,
    text: String,
}

fn split(line: &str, markers: &[&str]) -> Parts {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, rest) = line.split_at(indent_len);
    for m in markers {
        let Some(after) = rest.strip_prefix(m) else { continue };
        // `*` continues a block comment (` * text`), but `*/` closes it.
        if *m == "*" && !(after.is_empty() || after.starts_with(' ')) {
            continue;
        }
        let key = format!("{indent}{m}");
        let (sep, text) = match after.strip_prefix(' ') {
            Some(t) => (" ", t),
            None => ("", after),
        };
        let sep = if text.is_empty() { " " } else { sep };
        return Parts { prefix: format!("{key}{sep}"), key, text: text.to_string() };
    }
    Parts { key: indent.to_string(), prefix: indent.to_string(), text: rest.to_string() }
}

/// Comment markers for a line comment (`//` also covers `///` and `//!`), longest first.
fn markers(line_comment: Option<&str>, block: bool) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = match line_comment {
        Some("//") => vec!["///", "//!", "//"],
        Some("#") => vec!["#"],
        Some("--") => vec!["--"],
        Some(";") => vec![";;", ";"],
        _ => Vec::new(),
    };
    if block {
        v.push("*");
    }
    v.push(">");
    v
}

/// Words of `parts` re-laid to lines of at most `margin` chars (a longer word gets its own line).
fn wrap(prefix: &str, words: &[&str], margin: usize) -> Vec<String> {
    let width = prefix.chars().count();
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = width;
    for w in words {
        let wl = w.chars().count();
        if !cur.is_empty() && cur_len + 1 + wl > margin {
            lines.push(format!("{prefix}{cur}"));
            cur.clear();
            cur_len = width;
        }
        if !cur.is_empty() {
            cur.push(' ');
            cur_len += 1;
        }
        cur.push_str(w);
        cur_len += wl;
    }
    if !cur.is_empty() {
        lines.push(format!("{prefix}{cur}"));
    }
    lines
}

/// The paragraph at `line` rewrapped: its char range (whole lines, without the last newline) and
/// the new text. `None` on a blank line or when nothing changes.
pub fn fill_paragraph(
    doc: &Document,
    line: usize,
    margin: usize,
    line_comment: Option<&str>,
    block_comments: bool,
) -> Option<(Range<usize>, String)> {
    let markers = markers(line_comment, block_comments);
    let at = |l: usize| doc.line_text(l).map(|t| split(&t, &markers));
    let here = at(line)?;
    if here.text.trim().is_empty() {
        return None;
    }
    let same = |p: &Parts| p.key == here.key && !p.text.trim().is_empty();
    let mut first = line;
    while first > 0 && at(first - 1).is_some_and(|p| same(&p)) {
        first -= 1;
    }
    let mut last = line;
    while at(last + 1).is_some_and(|p| same(&p)) {
        last += 1;
    }
    let prefix = at(first).map(|p| p.prefix)?;
    let texts: Vec<String> = (first..=last).filter_map(|l| at(l).map(|p| p.text)).collect();
    let words: Vec<&str> = texts.iter().flat_map(|t| t.split_whitespace()).collect();
    let new = wrap(&prefix, &words, margin.max(prefix.chars().count() + 1)).join("\n");
    let range = doc.line_start(first)..doc.line_end(last);
    if doc.slice(range.clone()) == new {
        return None;
    }
    Some((range, new))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(text: &str, line: usize, margin: usize, lc: Option<&str>) -> String {
        let mut doc = Document::from_text(text);
        if let Some((r, new)) = fill_paragraph(&doc, line, margin, lc, true) {
            let edit = codecraft_text::Edit {
                changes: codecraft_text::ChangeSet::single(r.start, r.end, new),
                selections: codecraft_text::SelectionSet::caret(0),
            };
            let sel = codecraft_text::SelectionSet::caret(0);
            doc.apply(&edit, &sel, codecraft_text::EditGroup::Standalone).unwrap();
        }
        doc.text()
    }

    #[test]
    fn rewraps_doc_comments_and_stops_at_blank_comment_lines() {
        let src = "fn x() {}\n    /// one two three four five six seven\n    /// eight nine\n    ///\n    /// keep me\nfn y() {}\n";
        let out = fill(src, 2, 24, Some("//"));
        assert_eq!(
            out,
            "fn x() {}\n    /// one two three\n    /// four five six\n    /// seven eight nine\n    ///\n    /// keep me\nfn y() {}\n"
        );
        // Joining short lines back up.
        let out = fill(&out, 1, 100, Some("//"));
        assert!(out.contains("    /// one two three four five six seven eight nine\n    ///\n"), "{out}");
    }

    #[test]
    fn plain_text_python_and_block_comments() {
        assert_eq!(fill("aaa bbb\nccc\n\nddd\n", 0, 8, None), "aaa bbb\nccc\n\nddd\n");
        assert_eq!(fill("aaa bbb ccc\n\nddd\n", 0, 7, None), "aaa bbb\nccc\n\nddd\n");
        assert_eq!(fill("# a b c d\n", 0, 5, Some("#")), "# a b\n# c d\n");
        assert_eq!(fill("/*\n * a b c d\n */\n", 1, 7, Some("//")), "/*\n * a b\n * c d\n */\n");
        // Blank line, out of range and a word longer than the margin.
        assert_eq!(fill("\n", 0, 10, None), "\n");
        assert_eq!(fill("x", 7, 10, None), "x");
        assert_eq!(fill("// abcdefghijkl mn\n", 0, 5, Some("//")), "// abcdefghijkl\n// mn\n");
    }
}
