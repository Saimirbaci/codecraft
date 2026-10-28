//! `requirements.md`: a list of requirements with stable ids.
//!
//! A requirement is a heading (`### REQ-12: Title`, or Kiro's `### Requirement 12`) whose body
//! runs to the next heading of the same or a higher level, or a bullet (`- REQ-12: WHEN … THE
//! SYSTEM SHALL …`) with its indented continuation lines. Ids are `REQ-<number>`; the number is
//! what identifies the requirement, whatever else changes.

use std::collections::BTreeMap;

use crate::hash::fingerprint;
use crate::markdown::{self, Line};
use crate::model::{Diagnostic, EarsKind, Requirement, Severity};
use crate::{MAX_REQUIREMENTS, MAX_TEXT_CHARS};

/// What `requirements.md` parsed into.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequirementsDoc {
    /// The first level-1 heading that is not a requirement.
    pub title: Option<String>,
    pub requirements: Vec<Requirement>,
    pub diagnostics: Vec<Diagnostic>,
}

const FILE: &str = "requirements.md";

/// Where a requirement starts and how it is written.
struct Start {
    index: usize,
    /// Heading level; 0 for the bullet form.
    level: u8,
    number: u32,
    /// Heading: the rest of the heading. Bullet: the first line of the text.
    rest: String,
}

/// `REQ-12: Title` / `Requirement 12 - Title` → `(12, "Title")`. The prefix must be followed by
/// digits and then the end or a non-alphanumeric character (`REQ-12b` is not a requirement).
fn parse_ref(text: &str) -> Option<(u32, String)> {
    let t = text.trim_start_matches(['*', '_']).trim_start();
    let rest = markdown::strip_prefix_ci(t, "REQ-")
        .or_else(|| markdown::strip_prefix_ci(t, "REQ "))
        .or_else(|| markdown::strip_prefix_ci(t, "Requirement "))?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let number: u32 = rest.get(..digits)?.parse().ok()?;
    let after = rest.get(digits..)?;
    if after.chars().next().is_some_and(char::is_alphanumeric) {
        return None;
    }
    let title =
        after.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '-' | '–' | '—' | '.' | '*' | '_'));
    Some((number, title.trim().to_string()))
}

/// The EARS template a sentence follows.
pub fn ears_kind(text: &str) -> EarsKind {
    let words: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if !words.contains(&"SHALL") {
        return EarsKind::Unstructured;
    }
    let has = |w: &str| words.contains(&w);
    let triggers = ["WHEN", "WHILE", "WHERE"].iter().filter(|w| has(w)).count();
    if triggers > 1 {
        EarsKind::Complex
    } else if has("IF") && has("THEN") {
        EarsKind::Unwanted
    } else if has("WHEN") {
        EarsKind::EventDriven
    } else if has("WHILE") {
        EarsKind::StateDriven
    } else if has("WHERE") {
        EarsKind::Optional
    } else {
        EarsKind::Ubiquitous
    }
}

/// Parse `requirements.md`. Never fails: problems come back as diagnostics.
pub fn parse_requirements(text: &str) -> RequirementsDoc {
    let lines = markdown::lines(text);
    let mut doc = RequirementsDoc::default();

    let mut starts: Vec<Start> = Vec::new();
    for (index, l) in lines.iter().enumerate() {
        if l.fenced {
            continue;
        }
        if let Some((level, h)) = markdown::heading(l.text) {
            match parse_ref(h) {
                Some((number, rest)) => starts.push(Start { index, level, number, rest }),
                None if level == 1 && doc.title.is_none() && !h.is_empty() => doc.title = Some(h.to_string()),
                None => {}
            }
        } else if let Some((number, rest)) = markdown::bullet(l.text).and_then(parse_ref) {
            starts.push(Start { index, level: 0, number, rest });
        }
    }

    let mut first_line: BTreeMap<u32, u32> = BTreeMap::new();
    for (n, start) in starts.iter().enumerate() {
        if doc.requirements.len() >= MAX_REQUIREMENTS {
            let line = lines.get(start.index).map_or(0, |l| l.no);
            doc.diagnostics.push(diag(
                line,
                Severity::Warning,
                format!("more than {MAX_REQUIREMENTS} requirements: the rest are ignored"),
            ));
            break;
        }
        let next_start = starts.get(n + 1).map(|s| s.index);
        let end = body_end(&lines, start, next_start);
        let (title, body) = split(&lines, start, end);
        let line = lines.get(start.index).map_or(0, |l| l.no);
        let end_line = last_content_line(&lines, start.index, end);
        let id = format!("REQ-{}", start.number);
        if let Some(&first) = first_line.get(&start.number) {
            doc.diagnostics.push(diag(line, Severity::Error, format!("{id} is already defined on line {first}")));
            continue;
        }
        first_line.insert(start.number, line);
        if body.trim().is_empty() && title.is_empty() {
            doc.diagnostics.push(diag(line, Severity::Warning, format!("{id} has no text")));
        }
        let hash = fingerprint(&[&title, &body]);
        let ears = ears_kind(&body);
        doc.requirements.push(Requirement { id, number: start.number, title, text: body, line, end_line, ears, hash });
    }
    doc
}

fn diag(line: u32, severity: Severity, message: String) -> Diagnostic {
    Diagnostic { file: FILE.into(), line, severity, message }
}

/// The index just past the last line of the requirement that starts at `start`.
fn body_end(lines: &[Line<'_>], start: &Start, next_start: Option<usize>) -> usize {
    let limit = next_start.unwrap_or(lines.len());
    for i in start.index.saturating_add(1)..limit {
        let Some(l) = lines.get(i) else { break };
        if l.fenced {
            continue;
        }
        if start.level > 0 {
            // A heading of the same or a higher level ends the requirement; deeper ones
            // (`#### Acceptance Criteria`) belong to it.
            if markdown::heading(l.text).is_some_and(|(level, _)| level <= start.level) {
                return i;
            }
        } else if l.text.trim().is_empty() || markdown::heading(l.text).is_some() || markdown::indent_width(l.text) == 0
        {
            // A bullet's text continues on indented lines only.
            return i;
        }
    }
    limit
}

/// Title and text of the requirement spanning `lines[start.index..end]`.
fn split(lines: &[Line<'_>], start: &Start, end: usize) -> (String, String) {
    let body_from = start.index.saturating_add(1);
    let mut body: Vec<&str> = Vec::new();
    if start.level == 0 && !start.rest.is_empty() {
        body.push(start.rest.as_str());
    }
    for l in lines.get(body_from..end).unwrap_or_default() {
        body.push(l.text.trim_end());
    }
    let joined = body.join("\n");
    let text: String = joined.trim_matches(|c| c == '\n' || c == '\r').trim().chars().take(MAX_TEXT_CHARS).collect();
    let title = if start.level == 0 { String::new() } else { start.rest.chars().take(200).collect() };
    (title, dedent(&text))
}

/// Remove the indentation the lines share (a bullet's continuation lines are indented).
fn dedent(text: &str) -> String {
    let common = text.lines().skip(1).filter(|l| !l.trim().is_empty()).map(markdown::indent_width).min().unwrap_or(0);
    if common == 0 {
        return text.to_string();
    }
    let mut out = String::new();
    for (i, l) in text.lines().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let skip = if i == 0 { 0 } else { l.chars().take(common).take_while(|c| c.is_whitespace()).count() };
        out.extend(l.chars().skip(skip));
    }
    out
}

fn last_content_line(lines: &[Line<'_>], from: usize, end: usize) -> u32 {
    (from..end)
        .rev()
        .filter_map(|i| lines.get(i))
        .find(|l| !l.text.trim().is_empty())
        .or_else(|| lines.get(from))
        .map_or(0, |l| l.no)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIRO: &str = "# Requirements: Login\n\n\
### Requirement 1\n\n**User Story:** As a user, I want to sign in.\n\n#### Acceptance Criteria\n\n\
1. WHEN the user submits valid credentials THEN the system SHALL open a session\n\n\
### REQ-2: Lockout\nIF there are five failures THEN THE SYSTEM SHALL lock the account.\n\n\
## Notes\nnothing here\n";

    #[test]
    fn reads_headings_titles_bodies_and_lines() {
        let doc = parse_requirements(KIRO);
        assert_eq!(doc.title.as_deref(), Some("Requirements: Login"));
        assert_eq!(doc.requirements.len(), 2);
        let r1 = &doc.requirements[0];
        assert_eq!((r1.id.as_str(), r1.number, r1.line), ("REQ-1", 1, 3));
        assert!(r1.text.contains("Acceptance Criteria"), "deeper headings stay in the body: {r1:?}");
        assert!(r1.text.contains("open a session"));
        let r2 = &doc.requirements[1];
        assert_eq!((r2.id.as_str(), r2.title.as_str(), r2.line, r2.end_line), ("REQ-2", "Lockout", 11, 12));
        assert_eq!(r2.ears, EarsKind::Unwanted);
        assert!(!r2.text.contains("Notes"), "a same-level heading ends the requirement");
        assert!(doc.diagnostics.is_empty(), "{:?}", doc.diagnostics);
    }

    #[test]
    fn bullets_continue_on_indented_lines() {
        let doc = parse_requirements(
            "- **REQ-7:** WHEN a file is saved THE SYSTEM SHALL\n  format it.\n- REQ-8 THE SYSTEM SHALL log.\n\nplain\n",
        );
        let ids: Vec<_> = doc.requirements.iter().map(|r| (r.id.as_str(), r.text.as_str())).collect();
        assert_eq!(
            ids,
            [("REQ-7", "WHEN a file is saved THE SYSTEM SHALL\nformat it."), ("REQ-8", "THE SYSTEM SHALL log.")]
        );
        assert_eq!(doc.requirements[0].ears, EarsKind::EventDriven);
        assert_eq!(doc.requirements[0].display_title(), "WHEN a file is saved THE SYSTEM SHALL");
    }

    #[test]
    fn duplicate_ids_are_reported_and_the_first_wins() {
        let doc = parse_requirements("## REQ-1: A\nTHE SYSTEM SHALL a.\n## REQ-1: B\nTHE SYSTEM SHALL b.\n");
        assert_eq!(doc.requirements.len(), 1);
        assert_eq!(doc.requirements[0].title, "A");
        assert_eq!(doc.diagnostics.len(), 1);
        assert_eq!(doc.diagnostics[0].severity, Severity::Error);
        assert_eq!(doc.diagnostics[0].line, 3);
    }

    #[test]
    fn code_fences_hide_requirements() {
        let doc = parse_requirements("# T\n```\n## REQ-1: not real\n```\n## REQ-2: real\nTHE SYSTEM SHALL x.\n");
        let ids: Vec<_> = doc.requirements.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["REQ-2"]);
    }

    #[test]
    fn near_misses_are_not_requirements() {
        let doc =
            parse_requirements("## REQ-12b: no\n## REQ-: no\n## Requirements overview\n## REQ-99999999999: too big\n");
        assert!(doc.requirements.is_empty(), "{:?}", doc.requirements);
    }

    #[test]
    fn ears_templates() {
        assert_eq!(ears_kind("THE SYSTEM SHALL do it"), EarsKind::Ubiquitous);
        assert_eq!(ears_kind("WHEN a THE SYSTEM SHALL b"), EarsKind::EventDriven);
        assert_eq!(ears_kind("WHILE a THE SYSTEM SHALL b"), EarsKind::StateDriven);
        assert_eq!(ears_kind("WHERE a THE SYSTEM SHALL b"), EarsKind::Optional);
        assert_eq!(ears_kind("WHILE a WHEN b THE SYSTEM SHALL c"), EarsKind::Complex);
        assert_eq!(ears_kind("IF a THEN THE SYSTEM SHALL b"), EarsKind::Unwanted);
        assert_eq!(ears_kind("when a the system shall b"), EarsKind::Unstructured, "keywords are upper case");
        assert_eq!(ears_kind("it should work"), EarsKind::Unstructured);
    }

    #[test]
    fn the_hash_ignores_layout_but_not_words() {
        let a = parse_requirements("## REQ-1: T\nTHE SYSTEM SHALL do  x.\n");
        let b = parse_requirements("## REQ-1: T\n\n   THE SYSTEM SHALL\ndo x.\n\n");
        let c = parse_requirements("## REQ-1: T\nTHE SYSTEM SHALL do y.\n");
        assert_eq!(a.requirements[0].hash, b.requirements[0].hash);
        assert_ne!(a.requirements[0].hash, c.requirements[0].hash);
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for input in [
            "",
            "\0\0\0",
            "##",
            "## REQ-1",
            "- REQ-1",
            "- REQ-1\n\t\t\t",
            "```",
            "## REQ-4294967295: max\nTHE SYSTEM SHALL x.",
            "## REQ-4294967296: over",
            "## Requirement 1é: boundary\n## REQ-2é",
            "\r\n\r\n## REQ-1\r\ntext\r\n",
            "- REQ-1: é\n  é\n   é",
        ] {
            let _ = parse_requirements(input);
        }
        let many = "## REQ-1\n".repeat(10) + &"- REQ-2: x\n".repeat(5);
        assert_eq!(parse_requirements(&many).requirements.len(), 2);
        let huge: String = (1..=crate::MAX_REQUIREMENTS + 50).map(|n| format!("## REQ-{n}\nx\n")).collect();
        let doc = parse_requirements(&huge);
        assert_eq!(doc.requirements.len(), crate::MAX_REQUIREMENTS);
        assert!(doc.diagnostics.iter().any(|d| d.message.contains("ignored")));
    }

    #[test]
    fn long_text_is_capped() {
        let long = "word ".repeat(crate::MAX_TEXT_CHARS);
        let doc = parse_requirements(&format!("## REQ-1: T\n{long}\n"));
        assert!(doc.requirements[0].text.chars().count() <= crate::MAX_TEXT_CHARS);
    }
}
