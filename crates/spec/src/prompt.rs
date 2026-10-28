//! The briefs a coding agent gets for spec work: implement a requirement, or reconcile a
//! requirement with code that changed under it. Pure text: the caller decides which agent runs it
//! and how its changes are reviewed.

use std::fmt::Write as _;
use std::path::Path;

use crate::index::{CodeChange, CodeChangeKind, LinkRef};
use crate::model::{Feature, Requirement, Task, TaskState};
use crate::{DESIGN_FILE, REQUIREMENTS_FILE, SPEC_DIR, TASKS_FILE};

/// Most code references, changes, tasks and headings a brief lists (the agent can read the rest).
const MAX_LISTED: usize = 30;

/// How a requirement is named in the briefs: `login/REQ-3`.
fn name(feature: &Feature, req: &Requirement) -> String {
    format!("{}/{}", feature.name, req.id)
}

/// `path` relative to the project root, `/`-separated.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

fn spec_file(feature: &Feature, file: &str) -> String {
    format!("{SPEC_DIR}/{}/{file}", feature.name)
}

fn requirement_block(out: &mut String, feature: &Feature, req: &Requirement) {
    let _ = writeln!(out, "## The requirement ({}, line {})", spec_file(feature, REQUIREMENTS_FILE), req.line);
    if req.title.is_empty() {
        let _ = writeln!(out, "{}: {}", req.id, req.text.trim());
    } else {
        let _ = writeln!(out, "{}: {}\n{}", req.id, req.title.trim(), req.text.trim());
    }
    out.push('\n');
}

fn more(out: &mut String, shown: usize, total: usize) {
    if total > shown {
        let _ = writeln!(out, "- … and {} more", total - shown);
    }
}

fn linked_block(out: &mut String, root: &Path, links: &[LinkRef], heading: &str) {
    if links.is_empty() {
        return;
    }
    let _ = writeln!(out, "{heading}");
    for l in links.iter().take(MAX_LISTED) {
        match &l.symbol {
            Some(sym) => {
                let _ = writeln!(out, "- {}:{} `{}`", rel(root, &l.path), l.line, sym.signature);
            }
            None => {
                let _ = writeln!(out, "- {}:{}", rel(root, &l.path), l.line);
            }
        }
    }
    more(out, MAX_LISTED, links.len());
    out.push('\n');
}

fn task_line(t: &Task) -> String {
    let mark = match t.state {
        TaskState::Done => "x",
        TaskState::Doing => "-",
        TaskState::Todo => " ",
    };
    let id = if t.id.is_empty() { String::new() } else { format!("{} ", t.id) };
    format!("- [{mark}] {id}{} (line {} of {TASKS_FILE})", t.text.trim(), t.line)
}

/// The tasks that name `req` (the one to do first when `focus` is given).
fn tasks_block(out: &mut String, feature: &Feature, req: &Requirement, focus: Option<&Task>) {
    let related: Vec<&Task> = feature.tasks.iter().filter(|t| t.requirements.contains(&req.id)).collect();
    if focus.is_none() && related.is_empty() {
        return;
    }
    let _ = writeln!(out, "## Tasks ({})", spec_file(feature, TASKS_FILE));
    if let Some(t) = focus {
        let _ = writeln!(out, "Your task:\n{}\n", task_line(t));
    }
    let others: Vec<&&Task> = related.iter().filter(|t| focus.is_none_or(|f| f.line != t.line)).collect();
    if !others.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            if focus.is_some() { "Other tasks for the requirement:" } else { "Tasks for the requirement:" }
        );
        for t in others.iter().take(MAX_LISTED) {
            let _ = writeln!(out, "{}", task_line(t));
        }
        more(out, MAX_LISTED, others.len());
    }
    out.push('\n');
}

fn design_block(out: &mut String, feature: &Feature) {
    if feature.design_file.is_none() || feature.design.is_empty() {
        return;
    }
    let _ = writeln!(out, "## Design notes\nRead {} before you start. Its outline:", spec_file(feature, DESIGN_FILE));
    for h in feature.design.iter().take(MAX_LISTED) {
        let depth = usize::from(h.level.saturating_sub(1)).min(5);
        let _ = writeln!(out, "{}- {}", "  ".repeat(depth), h.text.trim());
    }
    more(out, MAX_LISTED, feature.design.len());
    out.push('\n');
}

/// The brief for implementing `req` of `feature`: what to build, how to link the code back and
/// what not to touch. `task` narrows it to one task of `tasks.md`; `links` is the code that
/// already points at the requirement.
pub fn implement_prompt(
    root: &Path,
    feature: &Feature,
    req: &Requirement,
    task: Option<&Task>,
    links: &[LinkRef],
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Implement requirement {} in this project.\n", name(feature, req));
    requirement_block(&mut out, feature, req);
    design_block(&mut out, feature);
    tasks_block(&mut out, feature, req, task);
    linked_block(
        &mut out,
        root,
        links,
        "## Code that already points at it\nIt may need to change to match the requirement as written now:",
    );
    let requirements = spec_file(feature, REQUIREMENTS_FILE);
    let tasks = spec_file(feature, TASKS_FILE);
    let id = &req.id;
    let _ = write!(
        out,
        "## How to work\n\
         1. Read {requirements} for the requirement in context and follow the project's conventions (AGENTS.md / CLAUDE.md).\n\
         2. Make the smallest change that satisfies the requirement as written. Do not implement other requirements.\n\
         3. Link the code back: put a `// spec: {id}` comment (use the language's comment syntax) on the line above \
         every function, type or test you add or change for this requirement. Without the link the requirement stays unlinked.\n\
         4. Add or update tests that show the requirement holds, and link them the same way.\n\
         5. Do not edit anything under {SPEC_DIR}/ except to tick the boxes in {tasks} for the tasks you finish. If the \
         requirement is wrong, ambiguous or cannot be met, say so in your final message instead of rewriting it.\n\
         6. Run the tests for what you changed.\n\
         7. Finish with a short summary: what you changed, where, and anything left open.\n"
    );
    out
}

/// What changed in code, one line per item.
fn changes_block(out: &mut String, changes: &[CodeChange]) {
    let _ = writeln!(out, "## What changed in the code since the last sync");
    for c in changes.iter().take(MAX_LISTED) {
        let what = match c.change {
            CodeChangeKind::Modified => "modified",
            CodeChangeKind::Removed => "removed (deleted, renamed, or no longer linked)",
            CodeChangeKind::Added => "newly linked",
        };
        let at = c.line.map_or_else(String::new, |l| format!(":{l}"));
        let _ = writeln!(out, "- {what}: {}{at} `{}`", c.path, c.symbol);
    }
    more(out, MAX_LISTED, changes.len());
    out.push('\n');
}

/// The brief for reconciling `req` with `changes` in the code that points at it: classify the
/// change and, only when the spec is out of date, propose the edit to the spec. The agent edits
/// nothing but spec files, so what it proposes is a diff of `specs/` to review.
pub fn reconcile_prompt(
    root: &Path,
    feature: &Feature,
    req: &Requirement,
    changes: &[CodeChange],
    links: &[LinkRef],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "The code that implements requirement {} changed after the requirement was last marked in sync. \
         Decide whether the spec still describes the code, and propose a spec edit only if it does not.\n",
        name(feature, req)
    );
    requirement_block(&mut out, feature, req);
    changes_block(&mut out, changes);
    linked_block(&mut out, root, links, "## Code that points at the requirement now");
    let requirements = spec_file(feature, REQUIREMENTS_FILE);
    let id = &req.id;
    let _ = write!(
        out,
        "## How to work\n\
         1. Read the changed code. The working tree has the current state; `git log` and `git diff` show how it got there.\n\
         2. Start your final message with one line, `Classification: <kind>`, where kind is exactly one of:\n\
         \x20  - refactor: the behaviour is unchanged. Edit nothing.\n\
         \x20  - violation: the code now contradicts the requirement (a bug). Edit nothing; describe the fix in your message.\n\
         \x20  - evolution: the behaviour changed on purpose and the requirement is out of date. Update it.\n\
         \x20  - addition: the code does something the spec does not mention. Add a requirement for it.\n\
         3. For evolution, edit {requirements} so {id} says what the code does now: keep its id and heading, \
         keep the EARS form (`WHEN … THE SYSTEM SHALL …`), and change only what the code change makes untrue.\n\
         4. For addition, append a requirement to {requirements} in the same style with the next free number \
         (one more than the highest REQ-n anywhere under {SPEC_DIR}/), and name the item that should carry its \
         `// spec:` link in your message.\n\
         5. Edit only files under {SPEC_DIR}/, and nothing else in them. Never edit code or tests, and never \
         delete a requirement.\n\
         6. After the classification, give two or three sentences of reasoning that a reviewer can check against the code.\n"
    );
    out
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::code::Symbol;
    use crate::feature::parse_feature;
    use crate::model::LinkKind;

    const REQS: &str = "# Requirements: Login\n\n## REQ-1: Sign in\nWHEN valid credentials are given THE SYSTEM SHALL open a session.\n\n## REQ-2: Lockout\nIF five attempts fail THEN THE SYSTEM SHALL lock the account.\n";
    const DESIGN: &str = "# Design\n\n## Components\n\n### Session store\n";
    const TASKS: &str =
        "- [ ] 1. Open the session (REQ-1)\n- [x] 2. Count failures (REQ-2)\n- [ ] 3. Lock the account (REQ-2)\n";

    fn feature() -> Feature {
        parse_feature("login", PathBuf::from("/p/specs/login"), Some(REQS), Some(DESIGN), Some(TASKS))
    }

    fn link(path: &str, line: u32, signature: &str) -> LinkRef {
        LinkRef {
            path: PathBuf::from(path),
            line,
            kind: LinkKind::Comment,
            symbol: Some(Symbol {
                signature: signature.into(),
                start_line: line + 1,
                end_line: line + 3,
                fingerprint: "0000000000000000".into(),
            }),
        }
    }

    #[test]
    fn the_implement_brief_names_the_requirement_the_links_and_the_limits() {
        let f = feature();
        let req = f.requirement("REQ-2").unwrap();
        let prompt =
            implement_prompt(Path::new("/p"), &f, req, None, &[link("/p/src/auth.rs", 7, "fn lock(user: &str)")]);
        assert!(prompt.starts_with("Implement requirement login/REQ-2 in this project."), "{prompt}");
        assert!(prompt.contains("IF five attempts fail THEN THE SYSTEM SHALL lock the account."), "{prompt}");
        assert!(prompt.contains("specs/login/requirements.md, line"), "{prompt}");
        assert!(prompt.contains("// spec: REQ-2"), "{prompt}");
        assert!(prompt.contains("src/auth.rs:7 `fn lock(user: &str)`"), "{prompt}");
        assert!(prompt.contains("Do not edit anything under specs/ except to tick the boxes in specs/login/tasks.md"));
        // The design's headings are an outline, and only this requirement's tasks are listed.
        assert!(prompt.contains("- Components"), "{prompt}");
        assert!(prompt.contains("Tasks for the requirement:"), "{prompt}");
        assert!(prompt.contains("- [x] 2 Count failures"), "{prompt}");
        assert!(prompt.contains("- [ ] 3 Lock the account"), "{prompt}");
        assert!(!prompt.contains("Open the session"), "{prompt}");
    }

    #[test]
    fn a_task_focuses_the_brief_and_the_rest_of_its_requirement_s_tasks_follow() {
        let f = feature();
        let req = f.requirement("REQ-2").unwrap();
        let task = f.tasks.iter().find(|t| t.id == "3").unwrap();
        let prompt = implement_prompt(Path::new("/p"), &f, req, Some(task), &[]);
        assert!(prompt.contains("Your task:\n- [ ] 3 Lock the account (REQ-2) (line 3 of tasks.md)"), "{prompt}");
        assert!(prompt.contains("Other tasks for the requirement:\n- [x] 2 Count failures"), "{prompt}");
        assert!(!prompt.contains("Code that already points at it"), "{prompt}");
    }

    #[test]
    fn a_feature_without_design_or_tasks_still_gets_a_brief() {
        let f = parse_feature("login", PathBuf::from("/p/specs/login"), Some(REQS), None, None);
        let req = f.requirement("REQ-1").unwrap();
        let prompt = implement_prompt(Path::new("/p"), &f, req, None, &[]);
        assert!(!prompt.contains("## Design notes"), "{prompt}");
        assert!(!prompt.contains("## Tasks"), "{prompt}");
        assert!(prompt.contains("## How to work"), "{prompt}");
    }

    #[test]
    fn long_lists_are_cut_and_say_so() {
        let f = feature();
        let req = f.requirement("REQ-1").unwrap();
        let links: Vec<LinkRef> = (0..MAX_LISTED + 5).map(|i| link("/p/src/a.rs", i as u32 + 1, "fn f()")).collect();
        let prompt = implement_prompt(Path::new("/p"), &f, req, None, &links);
        assert!(prompt.contains("- … and 5 more"), "{prompt}");
        assert_eq!(prompt.matches("src/a.rs:").count(), MAX_LISTED);
    }

    #[test]
    fn the_reconcile_brief_lists_the_changes_and_confines_the_agent_to_the_specs() {
        let f = feature();
        let req = f.requirement("REQ-2").unwrap();
        let changes = [
            CodeChange {
                path: "src/auth.rs".into(),
                symbol: "fn lock(user: &str)".into(),
                line: Some(8),
                change: CodeChangeKind::Modified,
            },
            CodeChange {
                path: "src/old.rs".into(),
                symbol: "fn gone()".into(),
                line: None,
                change: CodeChangeKind::Removed,
            },
        ];
        let prompt = reconcile_prompt(Path::new("/p"), &f, req, &changes, &[]);
        assert!(prompt.starts_with("The code that implements requirement login/REQ-2 changed after"), "{prompt}");
        assert!(prompt.contains("- modified: src/auth.rs:8 `fn lock(user: &str)`"), "{prompt}");
        assert!(
            prompt.contains("- removed (deleted, renamed, or no longer linked): src/old.rs `fn gone()`"),
            "{prompt}"
        );
        for kind in ["refactor", "violation", "evolution", "addition"] {
            assert!(prompt.contains(&format!("- {kind}:")), "{kind} missing: {prompt}");
        }
        assert!(prompt.contains("Classification: <kind>"), "{prompt}");
        assert!(prompt.contains("Edit only files under specs/"), "{prompt}");
        assert!(prompt.contains("Never edit code or tests"), "{prompt}");
        assert!(prompt.contains("keep the EARS form"), "{prompt}");
    }

    #[test]
    fn a_title_less_bullet_requirement_is_quoted_by_its_text() {
        let f = parse_feature(
            "x",
            PathBuf::from("/p/specs/x"),
            Some("# Requirements: X\n\n- REQ-4: THE SYSTEM SHALL log in.\n"),
            None,
            None,
        );
        let req = f.requirement("REQ-4").unwrap();
        let prompt = implement_prompt(Path::new("/p"), &f, req, None, &[]);
        assert!(prompt.contains("REQ-4: THE SYSTEM SHALL log in."), "{prompt}");
    }
}
