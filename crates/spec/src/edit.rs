//! Edits to the spec files as pure text transforms (the caller reads and writes the file).
//!
//! Each one keeps the file's style (heading level, bullet form, blank line after a heading, line
//! endings) and reads its own result back: an edit that would change any other requirement (a
//! heading or a duplicate id typed into a body) is refused instead of applied.

use crate::hash::fingerprint;
use crate::markdown;
use crate::model::{Requirement, Severity, TaskState};
use crate::requirements::{RequirementsDoc, parse_requirements, ref_prefix};
use crate::store::SpecError;
use crate::tasks::parse_tasks;
use crate::{MAX_REQUIREMENTS, MAX_TEXT_CHARS};

/// Longest title kept.
const MAX_TITLE_CHARS: usize = 200;

fn invalid(message: impl Into<String>) -> SpecError {
    SpecError::Invalid(message.into())
}

/// The file as lines, how it ends its lines and whether the last one is terminated.
struct Doc {
    lines: Vec<String>,
    eol: &'static str,
    trailing: bool,
}

impl Doc {
    fn new(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_string).collect(),
            eol: if text.contains("\r\n") { "\r\n" } else { "\n" },
            trailing: text.ends_with('\n') || text.is_empty(),
        }
    }

    fn render(&self) -> String {
        let mut out = self.lines.join(self.eol);
        if self.trailing && !self.lines.is_empty() {
            out.push_str(self.eol);
        }
        out
    }

    /// 0-based index of the 1-based `line`, if it exists.
    fn index(&self, line: u32) -> Option<usize> {
        let i = usize::try_from(line.checked_sub(1)?).ok()?;
        (i < self.lines.len()).then_some(i)
    }
}

/// A title is one line: whitespace collapses, length is capped.
fn clean_title(title: &str) -> String {
    title.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(MAX_TITLE_CHARS).collect()
}

/// Body lines as typed, trailing whitespace dropped and surrounding blank lines removed.
fn body_lines(body: &str) -> Result<Vec<String>, SpecError> {
    if body.chars().count() > MAX_TEXT_CHARS {
        return Err(invalid(format!("the text is longer than {MAX_TEXT_CHARS} characters")));
    }
    let mut lines: Vec<String> = body.lines().map(|l| l.trim_end().to_string()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let lead = lines.iter().take_while(|l| l.is_empty()).count();
    lines.drain(..lead);
    Ok(lines)
}

/// Change a requirement's title and/or text (`None` keeps it). A bullet requirement has no title.
pub fn edit_requirement(text: &str, id: &str, title: Option<&str>, body: Option<&str>) -> Result<String, SpecError> {
    let before = parse_requirements(text);
    let req = before.requirements.iter().find(|r| r.id == id).ok_or_else(|| invalid(format!("no requirement {id}")))?;
    let mut doc = Doc::new(text);
    let first = doc.index(req.line).ok_or_else(|| invalid("the requirement is outside the file"))?;
    let last = doc.index(req.end_line).ok_or_else(|| invalid("the requirement is outside the file"))?.max(first);
    let head = doc.lines.get(first).cloned().unwrap_or_default();

    let new_body = match body {
        Some(b) => body_lines(b)?,
        None => Vec::new(),
    };
    let (block, expected_title, expected_body) = match markdown::heading(&head) {
        Some((level, h)) => {
            let title = title.map_or_else(|| req.title.clone(), clean_title);
            let prefix = ref_prefix(h);
            let prefix = if prefix.is_empty() { format!("REQ-{}", req.number) } else { prefix.to_string() };
            let mut block = vec![heading_line(level, &prefix, &title)];
            let existing: Vec<String> = doc.lines.get(first + 1..=last).unwrap_or_default().to_vec();
            let rest = if body.is_some() {
                // Keep the blank line a heading is followed by, if it had one.
                let spaced = existing.first().is_some_and(|l| l.trim().is_empty());
                let mut rest = Vec::new();
                if spaced && !new_body.is_empty() {
                    rest.push(String::new());
                }
                rest.extend(new_body.clone());
                rest
            } else {
                existing
            };
            block.extend(rest);
            let text = body.map_or_else(|| req.text.clone(), |_| new_body.join("\n"));
            (block, title, text)
        }
        None => {
            if title.is_some_and(|t| !t.trim().is_empty()) {
                return Err(invalid("a requirement written as a bullet has no title: edit its text"));
            }
            let content = markdown::bullet(&head).unwrap_or_default();
            let marker = head.get(..head.len().saturating_sub(content.len())).unwrap_or("- ").to_string();
            let prefix = ref_prefix(content);
            let prefix = if prefix.is_empty() { format!("REQ-{}", req.number) } else { prefix.to_string() };
            let sentence = match body {
                Some(_) => new_body.clone(),
                None => req.text.lines().map(str::to_string).collect(),
            };
            let text = sentence.join("\n");
            (bullet_block(&marker, &prefix, &sentence), String::new(), text)
        }
    };
    doc.lines.splice(first..=last, block);
    let after = doc.render();
    verify(&before, &after, id, &expected_title, &expected_body)?;
    Ok(after)
}

fn heading_line(level: u8, prefix: &str, title: &str) -> String {
    let hashes = "#".repeat(usize::from(level.clamp(1, 6)));
    if title.is_empty() { format!("{hashes} {prefix}") } else { format!("{hashes} {prefix}: {title}") }
}

/// `- REQ-3: first line` and the rest indented under it.
fn bullet_block(marker: &str, prefix: &str, sentence: &[String]) -> Vec<String> {
    let indent = " ".repeat(marker.chars().count().max(2));
    let mut block = Vec::new();
    for (i, line) in sentence.iter().enumerate() {
        if i == 0 {
            block.push(format!("{marker}{prefix}: {line}"));
        } else if line.is_empty() {
            block.push(String::new());
        } else {
            block.push(format!("{indent}{line}"));
        }
    }
    if block.is_empty() {
        block.push(format!("{marker}{prefix}"));
    }
    block
}

/// Add a requirement with the given number after the last one (in the style of the last one: a
/// heading of the same level, or a bullet; a level-2 heading in a file with none yet).
pub fn add_requirement(text: &str, number: u32, title: &str, body: &str) -> Result<String, SpecError> {
    if number == 0 {
        return Err(invalid("requirement numbers start at 1"));
    }
    let before = parse_requirements(text);
    if before.requirements.len() >= MAX_REQUIREMENTS {
        return Err(invalid(format!("more than {MAX_REQUIREMENTS} requirements")));
    }
    if before.requirements.iter().any(|r| r.number == number) {
        return Err(invalid(format!("REQ-{number} already exists")));
    }
    let title = clean_title(title);
    let body = body_lines(body)?;
    let mut doc = Doc::new(text);
    let last = before.requirements.last();
    let level = last
        .and_then(|r| doc.index(r.line))
        .and_then(|i| doc.lines.get(i))
        .map_or(2, |l| markdown::heading(l).map_or(0, |(level, _)| level));
    let prefix = format!("REQ-{number}");
    let (block, expected_title, expected_body) = if level == 0 {
        // Bullets have no title; with only a title, it is the sentence.
        let sentence = if body.is_empty() && !title.is_empty() { vec![title] } else { body };
        let marker = last
            .and_then(|r| doc.index(r.line))
            .and_then(|i| doc.lines.get(i))
            .and_then(|l| markdown::bullet(l).map(|c| l.get(..l.len().saturating_sub(c.len())).unwrap_or("- ")))
            .unwrap_or("- ")
            .to_string();
        (bullet_block(&marker, &prefix, &sentence), String::new(), sentence.join("\n"))
    } else {
        let spaced =
            last.and_then(|r| doc.index(r.line)).and_then(|i| doc.lines.get(i + 1)).is_none_or(|l| l.trim().is_empty());
        let mut block = vec![heading_line(level, &prefix, &title)];
        if spaced && !body.is_empty() {
            block.push(String::new());
        }
        let expected = body.join("\n");
        block.extend(body);
        (block, title, expected)
    };

    match last.and_then(|r| doc.index(r.end_line)) {
        Some(end) => {
            let at = end + 1;
            // Headings are set apart by a blank line; a list of bullets stays a list.
            let mut insert = if level == 0 { Vec::new() } else { vec![String::new()] };
            insert.extend(block);
            if level != 0 && doc.lines.get(at).is_some_and(|l| !l.trim().is_empty()) {
                insert.push(String::new());
            }
            doc.lines.splice(at..at, insert);
        }
        None => {
            if doc.lines.last().is_some_and(|l| !l.trim().is_empty()) {
                doc.lines.push(String::new());
            }
            doc.lines.extend(block);
            doc.trailing = true;
        }
    }
    let after = doc.render();
    let id = format!("REQ-{number}");
    let found = parse_requirements(&after);
    if found.requirements.len() != before.requirements.len() + 1 {
        return Err(invalid("that text would change the structure of the file (a heading or id inside it?)"));
    }
    verify_unchanged_others(&before.requirements, &found.requirements, &id)?;
    let added =
        found.requirements.iter().find(|r| r.id == id).ok_or_else(|| invalid("the new requirement was lost"))?;
    if fingerprint(&[&added.title, &added.text]) != fingerprint(&[&expected_title, &expected_body]) {
        return Err(invalid("that text would change the structure of the file (a heading or id inside it?)"));
    }
    Ok(after)
}

/// Read the edit back: the same requirements, the edited one as intended, nobody else touched,
/// and no new errors.
fn verify(before: &RequirementsDoc, after_text: &str, id: &str, title: &str, body: &str) -> Result<(), SpecError> {
    let after = parse_requirements(after_text);
    let refuse = || invalid("that text would change the structure of the file (a heading or id inside it?)");
    if after.requirements.len() != before.requirements.len() {
        return Err(refuse());
    }
    verify_unchanged_others(&before.requirements, &after.requirements, id)?;
    let edited = after.requirements.iter().find(|r| r.id == id).ok_or_else(refuse)?;
    if fingerprint(&[&edited.title, &edited.text]) != fingerprint(&[title, body]) {
        return Err(refuse());
    }
    let errors = |d: &[crate::model::Diagnostic]| d.iter().filter(|d| d.severity == Severity::Error).count();
    if errors(&after.diagnostics) > errors(&before.diagnostics) {
        return Err(refuse());
    }
    Ok(())
}

fn verify_unchanged_others(before: &[Requirement], after: &[Requirement], id: &str) -> Result<(), SpecError> {
    for r in before.iter().filter(|r| r.id != id) {
        match after.iter().find(|a| a.id == r.id) {
            Some(a) if a.hash == r.hash => {}
            _ => return Err(invalid(format!("that text would change {}", r.id))),
        }
    }
    Ok(())
}

/// Set the checkbox of the task on `line` (1-based) of `tasks.md`.
pub fn set_task_state(text: &str, line: u32, state: TaskState) -> Result<String, SpecError> {
    let task = parse_tasks(text)
        .into_iter()
        .find(|t| t.line == line)
        .ok_or_else(|| invalid(format!("line {line} is not a task")))?;
    let mut doc = Doc::new(text);
    let i = doc.index(task.line).ok_or_else(|| invalid("the task is outside the file"))?;
    let current = doc.lines.get(i).cloned().unwrap_or_default();
    let content = markdown::bullet(&current).ok_or_else(|| invalid(format!("line {line} is not a task")))?;
    let at = current.len().saturating_sub(content.len());
    let mark = match state {
        TaskState::Todo => ' ',
        TaskState::Doing => '-',
        TaskState::Done => 'x',
    };
    // `[` + the mark + `]`: three ASCII bytes after the bullet marker.
    let (head, tail) = (current.get(..at), current.get(at.saturating_add(3)..));
    let (Some(head), Some(tail)) = (head, tail) else { return Err(invalid(format!("line {line} is not a task"))) };
    let updated = format!("{head}[{mark}]{tail}");
    if let Some(slot) = doc.lines.get_mut(i) {
        *slot = updated;
    }
    let after = doc.render();
    let ok = parse_tasks(&after).iter().any(|t| t.line == line && t.state == state);
    if ok { Ok(after) } else { Err(invalid(format!("line {line} is not a task"))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TaskState;

    const HEADINGS: &str = "# Requirements: Login\n\n## REQ-1: Sign in\n\nWHEN valid credentials are given THE SYSTEM SHALL open a session.\n\n#### Acceptance\n- a\n\n## REQ-2: Lockout\nIF five attempts fail THEN THE SYSTEM SHALL lock the account.\n\n## Notes\nx\n";

    fn req(text: &str, id: &str) -> Requirement {
        parse_requirements(text).requirements.into_iter().find(|r| r.id == id).unwrap()
    }

    #[test]
    fn a_title_and_a_body_are_replaced_in_place() {
        let out = edit_requirement(
            HEADINGS,
            "REQ-1",
            Some("  Sign   in  with a password "),
            Some("THE SYSTEM SHALL sign users in.\n\n"),
        )
        .unwrap();
        assert!(
            out.contains("## REQ-1: Sign in with a password\n\nTHE SYSTEM SHALL sign users in.\n\n## REQ-2: Lockout\n"),
            "{out}"
        );
        assert!(!out.contains("Acceptance"), "the body, sub-headings included, was replaced");
        assert_eq!(req(&out, "REQ-2").hash, req(HEADINGS, "REQ-2").hash, "the other requirement is untouched");
        assert!(out.ends_with("## Notes\nx\n"));
        // Title only: the body stays, sub-heading and all.
        let t = edit_requirement(HEADINGS, "REQ-1", Some("Login"), None).unwrap();
        assert_eq!(req(&t, "REQ-1").title, "Login");
        assert_eq!(req(&t, "REQ-1").text, req(HEADINGS, "REQ-1").text);
        // Body only: the title stays.
        let b = edit_requirement(HEADINGS, "REQ-2", None, Some("THE SYSTEM SHALL lock after 3 failures.")).unwrap();
        assert_eq!(req(&b, "REQ-2").title, "Lockout");
        assert!(
            b.contains("## REQ-2: Lockout\nTHE SYSTEM SHALL lock after 3 failures.\n\n## Notes"),
            "no blank line was invented: {b}"
        );
    }

    #[test]
    fn kiros_heading_style_and_line_endings_survive() {
        let kiro = "### Requirement 1\r\n\r\n**User Story:** x\r\n\r\n### Requirement 2: Two\r\nbody\r\n";
        let out = edit_requirement(kiro, "REQ-1", Some("One"), Some("new text")).unwrap();
        assert_eq!(out, "### Requirement 1: One\r\n\r\nnew text\r\n\r\n### Requirement 2: Two\r\nbody\r\n");
        let out = edit_requirement(kiro, "REQ-2", Some(""), None).unwrap();
        assert!(out.contains("### Requirement 2\r\nbody\r\n"), "{out:?}");
    }

    #[test]
    fn a_bullet_requirement_edits_its_sentence_and_has_no_title() {
        let bullets = "# R\n\n- REQ-1: WHEN a THE SYSTEM SHALL b.\n  more\n- REQ-2: THE SYSTEM SHALL c.\n";
        let out = edit_requirement(bullets, "REQ-1", None, Some("THE SYSTEM SHALL x.\nSecond line.")).unwrap();
        assert_eq!(out, "# R\n\n- REQ-1: THE SYSTEM SHALL x.\n  Second line.\n- REQ-2: THE SYSTEM SHALL c.\n");
        assert!(matches!(edit_requirement(bullets, "REQ-1", Some("Title"), None), Err(SpecError::Invalid(_))));
        assert_eq!(edit_requirement(bullets, "REQ-1", Some("  "), None).unwrap(), bullets, "nothing changed");
    }

    #[test]
    fn an_edit_that_would_change_the_structure_is_refused() {
        for body in ["fine\n## REQ-9: Sneaky\nx", "fine\n## Another section", "fine\n## REQ-2: Dup"] {
            let r = edit_requirement(HEADINGS, "REQ-1", None, Some(body));
            assert!(matches!(r, Err(SpecError::Invalid(_))), "{body}: {r:?}");
        }
        // Deeper headings and fenced code are fine.
        assert!(edit_requirement(HEADINGS, "REQ-1", None, Some("x\n#### Deeper\ny\n```\n## in a fence\n```")).is_ok());
        assert!(matches!(edit_requirement(HEADINGS, "REQ-7", None, Some("x")), Err(SpecError::Invalid(_))));
        assert!(matches!(
            edit_requirement(HEADINGS, "REQ-1", None, Some(&"x".repeat(MAX_TEXT_CHARS + 1))),
            Err(SpecError::Invalid(_))
        ));
    }

    #[test]
    fn requirements_are_added_after_the_last_one_in_its_style() {
        let out = add_requirement(HEADINGS, 3, "Reset", "WHEN the user asks THE SYSTEM SHALL mail a link.").unwrap();
        assert!(
            out.contains(
                "lock the account.\n\n## REQ-3: Reset\nWHEN the user asks THE SYSTEM SHALL mail a link.\n\n## Notes\n"
            ),
            "follows the last one, which is compact: {out}"
        );
        assert_eq!(parse_requirements(&out).requirements.len(), 3);
        // A compact file (no blank line after headings) stays compact.
        let compact = "## REQ-1: A\ntext\n";
        assert_eq!(add_requirement(compact, 2, "B", "more").unwrap(), "## REQ-1: A\ntext\n\n## REQ-2: B\nmore\n");
        // Bullets continue as bullets; a title alone becomes the sentence.
        let bullets = "- REQ-1: THE SYSTEM SHALL a.\n";
        assert_eq!(
            add_requirement(bullets, 2, "", "THE SYSTEM SHALL b.\nand c").unwrap(),
            "- REQ-1: THE SYSTEM SHALL a.\n- REQ-2: THE SYSTEM SHALL b.\n  and c\n"
        );
        assert!(
            add_requirement(bullets, 2, "THE SYSTEM SHALL d.", "").unwrap().ends_with("- REQ-2: THE SYSTEM SHALL d.\n")
        );
        // An empty or heading-less file.
        assert_eq!(add_requirement("", 1, "First", "x").unwrap(), "## REQ-1: First\n\nx\n");
        assert_eq!(add_requirement("# Title", 4, "T", "").unwrap(), "# Title\n\n## REQ-4: T\n");
    }

    #[test]
    fn adding_refuses_duplicates_and_structure_changes() {
        assert!(
            matches!(add_requirement(HEADINGS, 1, "x", "y"), Err(SpecError::Invalid(m)) if m.contains("already exists"))
        );
        assert!(matches!(add_requirement(HEADINGS, 0, "x", "y"), Err(SpecError::Invalid(_))));
        assert!(matches!(add_requirement(HEADINGS, 9, "x", "y\n## REQ-1: Dup"), Err(SpecError::Invalid(_))));
        assert!(matches!(add_requirement(HEADINGS, 9, "x", "y\n## Notes 2"), Err(SpecError::Invalid(_))));
    }

    #[test]
    fn task_boxes_are_set_in_place() {
        let tasks = "# Tasks\n\n- [ ] 1. Parse _Requirements: REQ-1_\n  - [x] 1.1 Nested\n* [-] 2. Doing\n\n```\n- [ ] in a fence\n```\n";
        let done = set_task_state(tasks, 3, TaskState::Done).unwrap();
        assert!(done.contains("- [x] 1. Parse _Requirements: REQ-1_\n"), "{done}");
        assert_eq!(set_task_state(tasks, 4, TaskState::Todo).unwrap(), tasks.replace("  - [x] 1.1", "  - [ ] 1.1"));
        assert_eq!(set_task_state(tasks, 5, TaskState::Doing).unwrap(), tasks, "unchanged is fine");
        assert!(set_task_state(tasks, 1, TaskState::Done).is_err(), "a heading is not a task");
        assert!(set_task_state(tasks, 8, TaskState::Done).is_err(), "a fence hides its lines");
        assert!(set_task_state(tasks, 99, TaskState::Done).is_err());
        assert!(set_task_state(tasks, 0, TaskState::Done).is_err());
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for text in ["", "\n", "##", "## REQ-1", "## REQ-1:\n", "- REQ-1:", "- REQ-1", "\r\n\r\n", "é## REQ-1: é\né"]
        {
            for id in ["REQ-1", "REQ-2"] {
                let _ = edit_requirement(text, id, Some("é"), Some("é\n## REQ-5\n"));
                let _ = edit_requirement(text, id, None, None);
            }
            let _ = add_requirement(text, 7, "é", "é");
            let _ = set_task_state(text, 1, TaskState::Done);
        }
    }
}
