//! The little Markdown the spec files need: headings, list items and code fences.

/// One line of a file.
pub(crate) struct Line<'a> {
    /// 1-based.
    pub no: u32,
    pub text: &'a str,
    /// Inside a code fence (the fence lines too): never a heading or list item.
    pub fenced: bool,
}

pub(crate) fn line_no(index: usize) -> u32 {
    u32::try_from(index.saturating_add(1)).unwrap_or(u32::MAX)
}

/// Split `text` into lines (`\n` or `\r\n`), marking those inside code fences. An unclosed fence
/// runs to the end of the file, as Markdown renders it.
pub(crate) fn lines(text: &str) -> Vec<Line<'_>> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let t = raw.trim_start();
        let is_fence = t.starts_with("```") || t.starts_with("~~~");
        let fenced = in_fence || is_fence;
        if is_fence {
            in_fence = !in_fence;
        }
        out.push(Line { no: line_no(i), text: raw, fenced });
    }
    out
}

/// `## Title` → `(2, "Title")`: one to six `#`, then a space (or nothing). Up to three spaces of
/// indentation are allowed, as in CommonMark; closing `#`s are dropped.
pub(crate) fn heading(line: &str) -> Option<(u8, &str)> {
    let indent = line.len().saturating_sub(line.trim_start_matches(' ').len());
    if indent > 3 {
        return None;
    }
    let t = line.trim_start_matches(' ');
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = t.get(hashes..)?;
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim_end();
    Some((u8::try_from(hashes).unwrap_or(6), text))
}

/// `s` without `prefix`, ignoring ASCII case. Never splits a character.
pub(crate) fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) { s.get(prefix.len()..) } else { None }
}

/// The text of a list item (`- x`, `* x`, `+ x`), without its marker.
pub(crate) fn bullet(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let rest = t.strip_prefix(['-', '*', '+'])?;
    rest.strip_prefix([' ', '\t'])
}

/// Number of leading columns of whitespace (a tab counts as four).
pub(crate) fn indent_width(line: &str) -> usize {
    line.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(|c| if c == '\t' { 4 } else { 1 })
        .fold(0usize, usize::saturating_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings() {
        assert_eq!(heading("# A"), Some((1, "A")));
        assert_eq!(heading("### Requirement 1 ###"), Some((3, "Requirement 1")));
        assert_eq!(heading("   ## Indented"), Some((2, "Indented")));
        assert_eq!(heading("    ## Code block"), None);
        assert_eq!(heading("#NoSpace"), None);
        assert_eq!(heading("####### seven"), None);
        assert_eq!(heading("#"), Some((1, "")));
        assert_eq!(heading("text # not"), None);
    }

    #[test]
    fn fences_hide_their_lines_and_unclosed_fences_run_to_the_end() {
        let ls = lines("a\n```\n# not a heading\n```\nb\n```\nc");
        let fenced: Vec<bool> = ls.iter().map(|l| l.fenced).collect();
        assert_eq!(fenced, [false, true, true, true, false, true, true]);
        assert_eq!(ls.get(2).map(|l| l.no), Some(3));
    }

    #[test]
    fn prefix_matching_never_splits_a_character() {
        assert_eq!(strip_prefix_ci("Req-1", "REQ-"), Some("1"));
        assert_eq!(strip_prefix_ci("é", "REQ-"), None);
        assert_eq!(strip_prefix_ci("ééé", "REQ-"), None);
        assert_eq!(strip_prefix_ci("RE", "REQ-"), None);
    }

    #[test]
    fn bullets_and_indentation() {
        assert_eq!(bullet("  - [ ] x"), Some("[ ] x"));
        assert_eq!(bullet("* y"), Some("y"));
        assert_eq!(bullet("-y"), None);
        assert_eq!(bullet("1. z"), None);
        assert_eq!(indent_width("\t  x"), 6);
        assert_eq!(indent_width("x"), 0);
    }
}
