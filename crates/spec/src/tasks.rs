//! `tasks.md` (a checklist) and `design.md` (only its outline is read).
//!
//! ```text
//! - [x] 1. Parse the config file
//!   - [ ] 1.1 Handle a missing file _Requirements: REQ-2, REQ-3_
//! - [-] 2. Write the migration
//! ```

use crate::markdown;
use crate::model::{Heading, Task, TaskState};
use crate::{MAX_HEADINGS, MAX_TASKS, MAX_TEXT_CHARS};

/// Parse `tasks.md`: every checkbox list item (`[ ]`, `[-]`, `[x]`) outside code fences.
pub fn parse_tasks(text: &str) -> Vec<Task> {
    let mut out = Vec::new();
    for l in markdown::lines(text) {
        if l.fenced {
            continue;
        }
        let Some(item) = markdown::bullet(l.text) else { continue };
        let Some((state, rest)) = checkbox(item) else { continue };
        if out.len() >= MAX_TASKS {
            break;
        }
        let (id, text) = split_number(rest.trim());
        let depth = u8::try_from((markdown::indent_width(l.text) / 2).min(8)).unwrap_or(8);
        out.push(Task {
            id,
            requirements: requirement_ids(text),
            text: text.chars().take(MAX_TEXT_CHARS).collect(),
            state,
            depth,
            line: l.no,
        });
    }
    out
}

fn checkbox(item: &str) -> Option<(TaskState, &str)> {
    let rest = item.strip_prefix('[')?;
    let mut chars = rest.chars();
    let mark = chars.next()?;
    let after = chars.as_str().strip_prefix(']')?;
    if !after.is_empty() && !after.starts_with([' ', '\t']) {
        return None;
    }
    let state = match mark {
        ' ' => TaskState::Todo,
        '-' | '~' => TaskState::Doing,
        'x' | 'X' => TaskState::Done,
        _ => return None,
    };
    Some((state, after))
}

/// `2.1 Write it` → `("2.1", "Write it")`; `Write it` → `("", "Write it")`.
fn split_number(text: &str) -> (String, &str) {
    let end = text.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(text.len());
    let (num, rest) = text.split_at_checked(end).unwrap_or(("", text));
    // `2. text` / `2.1 text`, not `2024 was`, `1.5x faster` or a bare number.
    let numbered = num.contains('.')
        && num.starts_with(|c: char| c.is_ascii_digit())
        && rest.starts_with([' ', '\t'])
        && num.len() <= 12;
    if numbered && !rest.trim().is_empty() {
        (num.trim_end_matches('.').to_string(), rest.trim_start())
    } else {
        (String::new(), text)
    }
}

/// The requirement ids a task names: `REQ-3` anywhere, and the numbers after `Requirements:`
/// (`_Requirements: 1.1, 2_` is Kiro's way; `1.1` means requirement 1).
pub fn requirement_ids(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |n: u32| {
        let id = format!("REQ-{n}");
        if !out.contains(&id) {
            out.push(id);
        }
    };
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(rel) = text.get(i..).and_then(|t| find_ci(t, "REQ-")) {
        let start = i.saturating_add(rel).saturating_add(4);
        let digits = bytes.get(start..).unwrap_or_default().iter().take_while(|b| b.is_ascii_digit()).count();
        if (1..=9).contains(&digits)
            && let Some(n) = text.get(start..start.saturating_add(digits)).and_then(|d| d.parse::<u32>().ok())
        {
            add(n);
        }
        i = start;
    }
    if let Some(pos) = find_ci(text, "requirements:").or_else(|| find_ci(text, "requirement:")) {
        let after = text.get(pos..).and_then(|t| t.split_once(':')).map_or("", |(_, r)| r);
        for token in after.split([',', ';']) {
            let token = token.trim().trim_matches(|c| c == '_' || c == '*').trim();
            if token.to_ascii_uppercase().starts_with("REQ-") {
                continue; // already added above
            }
            let head: String = token.chars().take_while(char::is_ascii_digit).collect();
            match head.parse::<u32>() {
                Ok(n) if !head.is_empty() && head.len() <= 9 => add(n),
                _ => break,
            }
        }
    }
    out
}

/// Byte offset of the first ASCII-case-insensitive `needle` in `hay`.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    let n = needle.len();
    hay.char_indices()
        .map(|(i, _)| i)
        .find(|&i| hay.get(i..i.saturating_add(n)).is_some_and(|s| s.eq_ignore_ascii_case(needle)))
}

/// The headings of a document down to level 3, outside code fences.
pub fn parse_outline(text: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    for l in markdown::lines(text) {
        if l.fenced {
            continue;
        }
        if let Some((level, t)) = markdown::heading(l.text).filter(|(level, t)| *level <= 3 && !t.is_empty()) {
            if out.len() >= MAX_HEADINGS {
                break;
            }
            out.push(Heading { level, text: t.chars().take(200).collect(), line: l.no });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_states_numbers_depth_and_requirements() {
        let tasks = parse_tasks(
            "# Tasks\n- [x] 1. Parse the config file\n  - [ ] 1.1 Handle a missing file _Requirements: REQ-2, 3_\n- [-] Write it\n- [X]\n- [ ] 2024 was a year\n- plain item\n- [y] nope\n",
        );
        let view: Vec<_> = tasks.iter().map(|t| (t.id.as_str(), t.text.as_str(), t.state, t.depth, t.line)).collect();
        assert_eq!(
            view,
            [
                ("1", "Parse the config file", TaskState::Done, 0, 2),
                ("1.1", "Handle a missing file _Requirements: REQ-2, 3_", TaskState::Todo, 1, 3),
                ("", "Write it", TaskState::Doing, 0, 4),
                ("", "", TaskState::Done, 0, 5),
                ("", "2024 was a year", TaskState::Todo, 0, 6),
            ]
        );
        assert_eq!(tasks[1].requirements, ["REQ-2", "REQ-3"]);
    }

    #[test]
    fn requirement_references() {
        assert_eq!(requirement_ids("implements REQ-12 and req-3, REQ-12"), ["REQ-12", "REQ-3"]);
        assert_eq!(requirement_ids("_Requirements: 1.1, 2.3, 7_"), ["REQ-1", "REQ-2", "REQ-7"]);
        assert_eq!(requirement_ids("Requirements: REQ-5, 6 then more text"), ["REQ-5", "REQ-6"]);
        assert!(requirement_ids("nothing here").is_empty());
        assert!(requirement_ids("REQ-").is_empty());
        assert!(requirement_ids("REQ-99999999999999").is_empty());
    }

    #[test]
    fn code_fences_hide_tasks() {
        assert!(parse_tasks("```\n- [ ] not a task\n```\n").is_empty());
    }

    #[test]
    fn the_outline_stops_at_level_three() {
        let h = parse_outline("# A\n## B\n### C\n#### D\n```\n# no\n```\n#\n");
        let view: Vec<_> = h.iter().map(|h| (h.level, h.text.as_str(), h.line)).collect();
        assert_eq!(view, [(1, "A", 1), (2, "B", 2), (3, "C", 3)]);
    }

    #[test]
    fn hostile_input_does_not_panic() {
        for input in [
            "",
            "- [",
            "- [ ",
            "- []",
            "- [ ]é",
            "-\t[x]\té",
            "\0",
            "- [ ] REQ-é",
            "- [ ] Requirements:é",
            "- [ ] 99999999999999999999 x",
        ] {
            let _ = parse_tasks(input);
            let _ = parse_outline(input);
        }
        let many = "- [ ] x\n".repeat(crate::MAX_TASKS + 10);
        assert_eq!(parse_tasks(&many).len(), crate::MAX_TASKS);
    }
}
