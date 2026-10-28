//! References from code to requirements.
//!
//! ```text
//! // spec: REQ-12                     a comment (also #, --, /*, *, <!--, ;)
//! // spec: REQ-3, auth/REQ-4          several, optionally qualified by feature
//! #[spec("REQ-12")]                   an attribute on a test or item (Rust)
//! @spec("REQ-12", "REQ-13")           a decorator (Python, TypeScript)
//! ```

use crate::model::{CodeLink, LinkKind};
use crate::{MAX_LINE_BYTES, MAX_LINKS_PER_FILE};

const COMMENT_MARKERS: [&str; 5] = ["//", "#", "--", "/*", "<!--"];

/// Every requirement reference in `text`, in order.
pub fn scan_links(text: &str) -> Vec<CodeLink> {
    let mut out = Vec::new();
    if !mentions_spec(text) {
        return out;
    }
    for (i, line) in text.lines().enumerate() {
        if line.len() > MAX_LINE_BYTES || !mentions_spec(line) {
            continue;
        }
        let no = crate::markdown::line_no(i);
        scan_line(line, no, &mut out);
        if out.len() >= MAX_LINKS_PER_FILE {
            out.truncate(MAX_LINKS_PER_FILE);
            break;
        }
    }
    out
}

/// A cheap test for the common case (a file with no references at all).
pub fn mentions_spec(text: &str) -> bool {
    text.as_bytes().windows(5).any(|w| w.eq_ignore_ascii_case(b"spec:") || w.eq_ignore_ascii_case(b"spec("))
}

fn scan_line(line: &str, no: u32, out: &mut Vec<CodeLink>) {
    let lower = line.to_ascii_lowercase();
    // Comment form: `spec:` after a comment marker (or a `*` that continues a block comment).
    let mut from = 0;
    while let Some(rel) = lower.get(from..).and_then(|l| l.find("spec:")) {
        let at = from.saturating_add(rel);
        let before = line.get(..at).unwrap_or("");
        let word_start = !before.chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
        let in_comment = COMMENT_MARKERS.iter().any(|m| before.contains(m)) || before.trim_start().starts_with('*');
        if word_start && in_comment {
            let after = line.get(at.saturating_add(5)..).unwrap_or("");
            for (feature, id) in refs_in_list(after) {
                out.push(CodeLink { feature, id, line: no, kind: LinkKind::Comment, symbol: None });
            }
        }
        from = at.saturating_add(5);
    }
    // Attribute / decorator form: `#[spec("REQ-1", "REQ-2")]`, `@spec("REQ-1")`.
    let mut from = 0;
    while let Some(rel) = lower.get(from..).and_then(|l| l.find("spec(")) {
        let at = from.saturating_add(rel);
        let before = line.get(..at).unwrap_or("");
        if before.ends_with("#[") || before.ends_with('@') || before.ends_with("#![") {
            let args = line.get(at.saturating_add(5)..).unwrap_or("");
            let args = args.split(')').next().unwrap_or("");
            for lit in string_literals(args) {
                for (feature, id) in refs_in_list(lit) {
                    out.push(CodeLink { feature, id, line: no, kind: LinkKind::Attribute, symbol: None });
                }
            }
        }
        from = at.saturating_add(5);
    }
}

/// The contents of the `"…"` / `'…'` literals in `s` (no escapes: ids contain none).
fn string_literals(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find(['"', '\'']) {
        let quote = rest.get(open..open.saturating_add(1)).unwrap_or("\"");
        let body_start = open.saturating_add(1);
        let Some(after) = rest.get(body_start..) else { break };
        let Some(close) = after.find(quote) else { break };
        out.push(after.get(..close).unwrap_or(""));
        rest = after.get(close.saturating_add(1)..).unwrap_or("");
    }
    out
}

/// A list of references from the start of `s`: `REQ-1, auth/REQ-2 ; REQ-3 trailing words`.
/// Stops at the first token that is not a reference.
fn refs_in_list(s: &str) -> Vec<(Option<String>, String)> {
    let mut out = Vec::new();
    for token in s.split(|c: char| c == ',' || c == ';' || c.is_whitespace()).filter(|t| !t.is_empty()) {
        match parse_ref(token) {
            Some(r) => out.push(r),
            None => break,
        }
    }
    out
}

/// `REQ-12` or `feature/REQ-12`, with trailing punctuation (`.`, `)`, `*/`) ignored.
fn parse_ref(token: &str) -> Option<(Option<String>, String)> {
    let token = token.trim_end_matches(['.', ')', ']', '*', '/', '>', ':']);
    let (feature, id) = match token.rsplit_once('/') {
        Some((f, id)) => (Some(f), id),
        None => (None, token),
    };
    if let Some(f) = feature
        && !is_feature_name(f)
    {
        return None;
    }
    Some((feature.map(str::to_string), parse_req_id(id)?))
}

/// `REQ-007` → `REQ-7`: nothing else may follow the digits.
fn parse_req_id(id: &str) -> Option<String> {
    let digits = crate::markdown::strip_prefix_ci(id, "REQ-")?;
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u32 = digits.parse().ok()?;
    Some(format!("REQ-{n}"))
}

/// What a feature directory may be called: starts with a letter or digit (so `..` is out).
pub fn is_feature_name(f: &str) -> bool {
    f.len() <= 64
        && f.starts_with(|c: char| c.is_ascii_alphanumeric())
        && f.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(text: &str) -> Vec<String> {
        scan_links(text)
            .into_iter()
            .map(|l| {
                format!("{}{}@{}:{:?}", l.feature.map(|f| format!("{f}/")).unwrap_or_default(), l.id, l.line, l.kind)
            })
            .collect()
    }

    #[test]
    fn comments() {
        assert_eq!(view("fn a() {} // spec: REQ-12\n"), ["REQ-12@1:Comment"]);
        assert_eq!(view("x\n# SPEC: REQ-3, auth/REQ-4 and more words"), ["REQ-3@2:Comment", "auth/REQ-4@2:Comment"]);
        assert_eq!(view("/* spec: REQ-1 */"), ["REQ-1@1:Comment"]);
        assert_eq!(view(" * spec: REQ-9\n"), ["REQ-9@1:Comment"]);
        assert_eq!(view("-- spec: REQ-2."), ["REQ-2@1:Comment"]);
    }

    #[test]
    fn attributes_and_decorators() {
        assert_eq!(view("#[spec(\"REQ-12\")]\nfn t() {}"), ["REQ-12@1:Attribute"]);
        assert_eq!(
            view("@spec(\"REQ-1\", 'billing/REQ-2')\ndef t(): pass"),
            ["REQ-1@1:Attribute", "billing/REQ-2@1:Attribute"]
        );
    }

    #[test]
    fn things_that_are_not_links() {
        assert!(view("let spec: REQ-1 = 3;").is_empty(), "no comment marker");
        assert!(view("// the spec: see the docs").is_empty(), "no id after spec:");
        assert!(view("// myspec: REQ-1").is_empty(), "spec must start a word");
        assert!(view("spec(\"REQ-1\")").is_empty(), "an attribute needs #[ or @");
        assert!(view("// spec: REQ-1b").is_empty());
        assert!(view("// spec: ../REQ-1").is_empty());
        assert!(view("").is_empty());
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for input in [
            "spec:",
            "// spec:",
            "// spec: ",
            "// spec: /",
            "#[spec(",
            "#[spec(\"",
            "#[spec(\"REQ-1",
            "@spec('",
            "é// spec: é",
            "// spec: éREQ-1",
            "\0spec:\0",
            "#[spec(\"\"\"\"\")]",
        ] {
            let _ = scan_links(input);
        }
        let long = format!("// spec: {}", "REQ-1, ".repeat(10_000));
        assert!(scan_links(&long).is_empty(), "overlong lines are skipped");
        let many = "// spec: REQ-1\n".repeat(MAX_LINKS_PER_FILE + 10);
        assert_eq!(scan_links(&many).len(), MAX_LINKS_PER_FILE);
    }
}
