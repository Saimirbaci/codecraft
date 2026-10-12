//! In-document find and replace.
//!
//! Results are char ranges (the text model's coordinates). Empty matches (e.g. `^` or `a*`)
//! are skipped everywhere, so find and replace always agree.

use serde::{Deserialize, Serialize};

use crate::util::normalize_newlines;
use crate::{Change, ChangeSet, Document, Edit, Result, SelectionSet, TextError};

/// Find/replace options, mirroring RustRover's find bar toggles (Cc, W, .*).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub pattern: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

impl SearchQuery {
    pub fn plain(pattern: impl Into<String>) -> Self {
        Self { pattern: pattern.into(), ..Self::default() }
    }

    /// Compile to a regex (literal patterns are escaped; whole word adds `\b`, case-insensitive adds `(?i)`).
    ///
    /// Regex patterns are multi-line (`^`/`$` match at line boundaries, as in the IDE). For a
    /// literal pattern, whole word only adds `\b` on a side that starts/ends with a word char,
    /// so `->` with whole word still matches.
    pub fn to_regex(&self) -> Result<regex::Regex> {
        let mut p = if self.regex { self.pattern.clone() } else { regex::escape(&self.pattern) };
        if self.whole_word {
            if self.regex {
                p = format!(r"\b(?:{p})\b");
            } else {
                let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
                let lead = if word(self.pattern.chars().next()) { r"\b" } else { "" };
                let trail = if word(self.pattern.chars().next_back()) { r"\b" } else { "" };
                p = format!("{lead}(?:{p}){trail}");
            }
        }
        if self.regex {
            p = format!("(?m){p}");
        }
        if !self.case_sensitive {
            p = format!("(?i){p}");
        }
        regex::Regex::new(&p).map_err(|e| TextError::BadPattern(e.to_string()))
    }
}

/// `None` for an empty pattern (matches nothing), else the compiled query.
fn compile(query: &SearchQuery) -> Result<Option<regex::Regex>> {
    if query.pattern.is_empty() { Ok(None) } else { query.to_regex().map(Some) }
}

/// Byte offsets of `text` (the document's text) to char offsets.
fn to_chars(doc: &Document, start: usize, end: usize) -> Option<std::ops::Range<usize>> {
    let s = doc.rope().try_byte_to_char(start).ok()?;
    let e = doc.rope().try_byte_to_char(end).ok()?;
    Some(s..e)
}

/// All matches as char ranges, at most `limit`. Empty pattern → no matches.
pub fn find_all(doc: &Document, query: &SearchQuery, limit: usize) -> Result<Vec<std::ops::Range<usize>>> {
    let Some(re) = compile(query)? else { return Ok(Vec::new()) };
    let text = doc.text();
    Ok(re
        .find_iter(&text)
        .filter(|m| !m.is_empty())
        .filter_map(|m| to_chars(doc, m.start(), m.end()))
        .take(limit)
        .collect())
}

/// The next (or previous) match from `from`, optionally wrapping around.
///
/// Forward: the first match starting at or after `from`. Backward: the last match starting
/// before `from`. With `wrap`, the search continues from the other end of the document.
pub fn find_next(
    doc: &Document,
    query: &SearchQuery,
    from: usize,
    forward: bool,
    wrap: bool,
) -> Result<Option<std::ops::Range<usize>>> {
    let Some(re) = compile(query)? else { return Ok(None) };
    let text = doc.text();
    let from_byte = doc.rope().try_char_to_byte(from.min(doc.len_chars())).unwrap_or(text.len());
    let hit = if forward {
        first_match_at(&re, &text, from_byte).or_else(|| if wrap { first_match_at(&re, &text, 0) } else { None })
    } else {
        let (mut before, mut last) = (None, None);
        for m in re.find_iter(&text).filter(|m| !m.is_empty()) {
            if m.start() < from_byte {
                before = Some((m.start(), m.end()));
            }
            last = Some((m.start(), m.end()));
        }
        before.or(if wrap { last } else { None })
    };
    Ok(hit.and_then(|(s, e)| to_chars(doc, s, e)))
}

/// The first non-empty match starting at or after byte `at` (context before `at` still counts
/// for `\b` and `^`).
fn first_match_at(re: &regex::Regex, text: &str, at: usize) -> Option<(usize, usize)> {
    let mut at = at;
    while at <= text.len() {
        let m = re.find_at(text, at)?;
        if !m.is_empty() {
            return Some((m.start(), m.end()));
        }
        // Step past the empty match to the next char boundary.
        at = m.start() + text.get(m.start()..)?.chars().next()?.len_utf8();
    }
    None
}

/// Rewrite an IDE-style regex replacement for `regex`: `$1` → `${1}` (so `$1a` is group 1 then
/// `a`, as in IntelliJ), and `\n`, `\t`, `\\`, `\$` escapes.
fn regex_template(replacement: &str) -> String {
    let mut out = String::with_capacity(replacement.len() + 8);
    let mut chars = replacement.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('$') => out.push_str("$$"),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '$' if chars.peek().is_some_and(char::is_ascii_digit) => {
                out.push_str("${");
                while let Some(d) = chars.next_if(char::is_ascii_digit) {
                    out.push(d);
                }
                out.push('}');
            }
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                out.push_str("$$");
            }
            _ => out.push(c),
        }
    }
    out
}

/// Replace every match; with `regex` the replacement may use `$1` groups.
///
/// Without `regex` the replacement is literal. The selections are mapped through the edit.
pub fn replace_all(doc: &Document, query: &SearchQuery, replacement: &str, sels: &SelectionSet) -> Result<Edit> {
    let sels = sels.clamp(doc.len_chars());
    let Some(re) = compile(query)? else { return Ok(Edit::none(sels)) };
    let text = doc.text();
    let template = if query.regex { regex_template(replacement) } else { String::new() };
    let literal = normalize_newlines(replacement);
    let mut changes = Vec::new();
    for caps in re.captures_iter(&text) {
        let Some(m) = caps.get(0).filter(|m| !m.is_empty()) else { continue };
        let Some(range) = to_chars(doc, m.start(), m.end()) else { continue };
        let new = if query.regex {
            let mut dst = String::new();
            caps.expand(&template, &mut dst);
            normalize_newlines(&dst)
        } else {
            literal.clone()
        };
        changes.push(Change::new(range.start, range.end, new));
    }
    let changes = ChangeSet::new(changes)?;
    let len = changes.len_after(doc.len_chars());
    let selections = sels.map(&changes).clamp(len);
    Ok(Edit { changes, selections })
}

#[cfg(test)]
mod tests;
