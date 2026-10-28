# Spec View

A tool window under Project that shows the project's **specs**: what the software is supposed to do,
written as numbered requirements, and which code points at each one. It is a CodeCraft extension
(RustRover has nothing like it), so it has no entry in the observed menus or the parity checklist.
Open it from the left stripe (📋) or with the command `view.toolWindows.spec`.

The aim is **spec-anchored** development: specs and code evolve together and every link between
them is machine-checked. It is deliberately not spec-as-source. CodeCraft never regenerates code
from a spec behind your back and never rewrites a spec from code; a change on either side is a
reviewable diff, the same rule as every other agent edit.

## Where specs live

```text
specs/<feature>/
  requirements.md   requirements with stable ids, ideally in EARS form
  design.md         free-form; the Spec View shows its outline (levels 1–3)
  tasks.md          a checklist; each task names the requirements it implements
```

`specs/` is an ordinary tracked directory, so specs show up in the Project view and Find in Files,
are watched, diff in the Commit tool window and are included in Review jobs' worktrees. (An earlier
sketch put them under `.codecraft/specs/`; that directory is CodeCraft's private state — excluded
from scans and git-ignored in worktrees — so specs there would have been invisible to people and
agents.) The layout follows Kiro's public description; no code or text was taken from it.

### requirements.md

```markdown
# Requirements: Login

## REQ-1: Sign in
WHEN valid credentials are given THE SYSTEM SHALL open a session.

## REQ-2: Lockout
IF five attempts fail THEN THE SYSTEM SHALL lock the account.
```

- A requirement is a heading (`## REQ-12: Title`, or Kiro's `### Requirement 12`) whose body runs
  to the next heading of the same or a higher level, or a bullet (`- REQ-12: WHEN … SHALL …`) with
  its indented continuation lines. Code fences are skipped.
- **Ids are stable and project-wide.** `spec.new` continues numbering across features, so a bare
  `REQ-12` in code is unambiguous. A duplicate id inside a file is an error and the first one wins.
  Never reuse the number of a deleted requirement.
- EARS keywords (`WHEN`, `WHILE`, `IF … THEN`, `WHERE`, `SHALL`) are matched in upper case, as EARS
  writes them. A requirement without `SHALL` is marked "Not in EARS form" in its tooltip; that is a
  hint, not an error.

### tasks.md

`- [ ] 1. Parse the config`, `- [-] 2.1 Doing` and `- [x] 3. Done` items count; indentation nests.
`_Requirements: REQ-2, 3_` (Kiro's form, `3` meaning `REQ-3`) or `REQ-2` anywhere in the text links
the task to a requirement. A task naming a requirement that does not exist is a warning.

### Code → requirement

```rust
// spec: REQ-12                  // a comment: //, #, --, /*, *, <!--
// spec: REQ-3, auth/REQ-4       // several; `feature/REQ-n` is the unambiguous form
#[spec("REQ-12")]                // an attribute or decorator: #[spec(…)] and @spec(…)
fn lockout_after_five_failures() {}
```

Only code is scanned: Markdown, text, reStructuredText and AsciiDoc files are skipped (they quote
examples), as are `specs/` and `.codecraft/`. A reference to a requirement that does not exist is
listed under **Dangling references** (the requirement was deleted, renumbered or mistyped: the code
and the spec disagree); a bare id defined by several features under **Ambiguous references**.

## Status of a requirement

| Status | Meaning |
|---|---|
| **Spec changed** | Its text differs from the text recorded at the last sync. The code may no longer match. |
| **Code changed** | The text is as recorded, but the code that points at it differs from the code recorded at the last sync. The spec may no longer describe the code. |
| **Unlinked** | No code points at it. |
| **Linked** | Code points at it; no sync has been recorded. |
| **In sync** | Code points at it and its text is the one recorded at the last sync. |

"Sync" is `spec.markSynced` (context menu: *Mark In Sync*): you say "the code matches this text".
It records the requirement's text and the code that points at it. The fingerprint ignores whitespace,
so re-wrapping a paragraph is not a change. The baseline is `.codecraft/spec-state.json`: private to
the checkout, not committed, so each clone starts with everything *Linked* or *Unlinked*. Unreadable
or hostile state files read as empty. A spec change wins over a code change in the chip.

### Code fingerprints

A reference is attached to the item that follows it: the function, type or statement after the
`// spec:` comment (skipping blank lines, other comments, attributes and decorators), or the line it
trails (`fn a() {} // spec: REQ-1`). Put the marker above the thing that implements the requirement.
The item's extent comes from a lexical scan that knows comments, strings (raw strings, character
literals and lifetimes in Rust, backticks in JavaScript, triple quotes in Python) and brackets:
braces for Rust and the C family, indentation for Python. The fingerprint covers the item's code
with comments and layout removed (`check( x )` and `check(x)` are the same; `ab` and `a b`, or two
different strings, are not).

At sync the baseline records, per requirement, each item's fingerprint under the key
`path#signature`. Later the view compares: an item whose code differs is *modified*; one that is
no longer there (deleted, renamed, signature changed, reference removed) is *removed*; one that
points at the requirement now but did not is *added*. Any of them makes the requirement *Code
changed*; the link rows list them. Limits:

- A reference whose item cannot be delimited (an unterminated item, a line over 2000 bytes, nothing
  after it) is listed but not tracked. Items are followed for at most 3000 lines, and about 200,000
  lines are examined per file.
- A marker inside a function body attaches to the statement after it, not to the function.
- Moving an item to another file reads as removed + added. Requirements synced before this
  existed have no recorded code and are not compared until synced again.

## Commands (also agent tools through `codecraft_execute`)

| Id | What |
|---|---|
| `view.toolWindows.spec` | Show or hide the Spec View |
| `spec.list {waitMs?}` | Features, requirements with status, link counts, diagnostics, dangling references. Starts a scan if none ran and answers `pending: true` until it finishes; `waitMs` (agents) waits for it |
| `spec.show {feature, id}` | A requirement's full text, status, fingerprints, linked code (with the item each reference is attached to), `codeChanges` since the last sync and tasks |
| `spec.open {feature, id?, file?}` | Open a requirement or one of the three files |
| `spec.openLink {path, line}` | Open code that points at a requirement |
| `spec.refresh` | Read the specs and scan the project again |
| `spec.addRequirement {feature, title?, text?}` | Append a requirement to the feature's `requirements.md` in its style, with the next free `REQ-n` (project-wide) |
| `spec.editRequirement {feature, id, title?, text?}` | Replace a requirement's title and/or text, keeping its id and every other requirement |
| `spec.setTask {feature, line, state}` | Tick or clear one checkbox in `tasks.md` (`todo`, `doing`, `done`) |
| `spec.unspecified {path?}` | Public items no requirement covers, in files that carry `// spec: required` (`waitMs` waits for the scan over MCP) |
| `spec.new {name}` | Create `specs/<name>/` with the three files and open `requirements.md`. Never overwrites |
| `spec.markSynced {feature, id?}` | Record the current text and the code that points at it as implemented. Fails while the scan is running |
| `spec.implement {feature, id? \| task?, backend?, mode?}` | Hand a requirement (or the requirement a task names) to a coding agent; returns the job. See *Implement and Reconcile* |
| `spec.reconcile {feature, id, backend?, mode?}` | For "Code changed": an agent classifies the change and proposes the edit to the spec |

Lines in results are 1-based. Agent capability: `spec.list/show/open/openLink/refresh/unspecified` are `read`;
`spec.new`, `spec.markSynced`, `spec.addRequirement`, `spec.editRequirement` and `spec.setTask`
need `write`. `spec.implement` and `spec.reconcile` start agents, so an agent may not run them
(the same rule as `agent.start`). `spec.list` and `spec.show` carry `agentJob` for each requirement:
`{id, kind: implement|reconcile, status, backend, mode, working}` or `null`.

## Unspecified code

The reverse question to "is this requirement implemented?": "is this public code traced to a
requirement?". It is **opt-in per file**: put `// spec: required` (any comment marker) in a file
and its public items are listed under *Unspecified code* in the view and by `spec.unspecified`
unless a reference covers them. Public items are Rust `pub fn`, `struct`, `enum`, `trait`,
`type`, `const`, `static`, `union` (not `pub(crate)`, `pub use` or `pub mod`; code under
`#[cfg(test)]` and `#[test]` is not API), Python module-level `def` and `class` without a leading
underscore, TypeScript and JavaScript `export function`, `class`, `interface`, `type`, `enum`,
`const`, `let`, `var`, and Go capitalised `func` (methods included) and `type`. Other languages
list nothing. Text inside strings and comments is not mistaken for code.

An item is covered by a reference attached to it (the `// spec: REQ-n` line above it, or an
attribute), by one inside it, or by one attached to an item that contains it (a marker above an
`impl` block covers its methods; above a class, its members). A reference to a requirement that
does not exist still counts as an attempt: the dangling list flags it. Deliberately not covered
code belongs outside the opted-in files. At most 500 items are listed per file.

## Editing in the view

*＋ Requirement* (next to the file links of a feature) and *Edit…* (context menu of a requirement)
open an inline form with a title and a text; ticking a task is a click on its box under the
requirement (the ⛓/☰ toggle). They run `spec.addRequirement`, `spec.editRequirement` and
`spec.setTask`, which change one requirement or one checkbox and nothing else in the file: the
heading level, bullet form, blank line after a heading and line endings are kept, and the open
editor is used when the file is open (the change is one undo step and is saved). Every edit is read
back before it is applied: text that would split a requirement, add another one or duplicate an id
(a `## REQ-9` line typed into a body) is refused and the form says why. A requirement written as a
bullet has no title. Editing the text of a synced requirement makes it *Spec changed*, which is
the point: the code now has to catch up.

## Implement and Reconcile

Both hand one requirement to a coding agent as an **ordinary agent job**, the one `agent.start`
runs: it appears in the Agent tool window and History, takes follow-ups, and asks for approvals
like any other. Right-click a requirement for *Implement with Agent* or, when the code under it
changed, *Reconcile with Agent*; right-click a task (under the ⛓/☰ toggle) for *Implement Task
with Agent*. The row then carries a chip such as `Implement · reviewing` that opens the job.

| | Implement (`spec.implement`) | Reconcile (`spec.reconcile`) |
|---|---|---|
| For | A requirement to build or to bring up to date after its text changed. With `task` (a line of `tasks.md`), the requirement that task names, with that task first | A requirement whose linked code changed since the last sync (*Code changed*) |
| Refused when | An earlier job for it is still building or waiting for its review | The same, or the text also changed (the spec leads: implement it, or *Mark In Sync* to accept the text as it is), or the code did not change |
| The agent is told | The requirement, the design outline, its tasks, the code already linked, and to: make the smallest change, put `// spec: REQ-n` above every function and test it adds or changes, add tests, tick finished tasks, touch nothing else under `specs/`, run the tests, and say what is left open | The requirement, which items changed (modified, removed, newly linked), the linked code, and to: read the change (`git log` and `git diff` are available), start its answer with `Classification: refactor\|violation\|evolution\|addition`, edit only files under `specs/` (evolution: reword the requirement, keeping its id and EARS form; addition: append a requirement with the next free number), and never edit code |
| Comes back as | A diff of code, tests and `tasks.md` | Nothing for a refactor or a violation (the answer says what to do); a diff of `specs/…` for an evolution or an addition |

- **Defaults.** The first installed agent (Claude Code, Codex, Grok, Antigravity, in that order) and
  **review mode**: the agent works in a git worktree that carries your uncommitted changes, and
  nothing reaches the project until you accept a file in the diff. That needs a git repository with
  a commit; without one the view says so (`mode: "auto"` or `"ask"` on the command skips the
  worktree). The menu uses the defaults; the commands take `backend` and `mode`.
- **What the agent reads.** The spec files as they are on disk, which is also what a review
  worktree holds. Unsaved edits in an open spec file are not in the brief.
- **After a Reconcile.** Accepting an evolution changes the requirement's text, so it shows
  *Spec changed* until you press *Mark In Sync*, which records the new text with the code as it
  is. After a refactor, press *Mark In Sync* directly. After a violation, fix the code (it can be
  another agent job) and then sync. Nothing is marked in sync for you.
- **A human decides.** The classification is the agent's judgment, written on the first line of
  its answer and not parsed by CodeCraft; what changes the spec is the diff you accept.
- **One job per requirement.** A finished job stays on the row until the next one replaces it.
  The sync baseline stores fingerprints, not old code, so a Reconcile brief lists *which* items
  changed and leaves the *how* to the agent's `git diff`.

Not built: starting either by itself when a requirement or its code changes. That would be a
setting, off by default; today every run starts from a click or a command.

## How it works

- `crates/spec` (`codecraft-spec`, L0, std + serde): parsers, the reference scanner, link
  resolution, status rules, baseline, scaffold and the two agent briefs (`implement_prompt`,
  `reconcile_prompt`: pure text). Reading never fails on bad content (problems are
  diagnostics); files over 1 MiB are not read; symbolic links are not followed; counts and lengths
  are capped.
- `crates/engine/src/spec_view.rs`: `Session::spec`, the scan on a worker thread (first time the
  window is shown, or when an agent asks), the file watcher hook (spec files re-read, changed code
  re-scanned), unsaved buffers of open code files while the window is shown (not in Power Save
  Mode), the `spec.*` commands, and the job started for each requirement (`SpecState::jobs`,
  looked up in the agents' job list; a discarded job disappears from the row).
- `crates/workspace`: `ToolWindowId::Spec`, and `Layout::left_lower`, a second left slot so the
  Spec View sits **under** Project instead of replacing it (its size is a height, remembered per
  window).
- `crates/ui-egui/src/spec_panel.rs`: drawing and clicks only.

## Not built yet

What the plan still leaves open:

- Auto-trigger: an agent started by a spec or code change without a click (a setting, off by
  default).
- A test-result signal (a linked test that starts failing) to join the code fingerprints.
- EARS linting beyond the hint, deleting a requirement from the view (ids are never reused, so
  this stays a deliberate edit of the file), adding a task, "generate a spec from existing code".
- Choosing the agent and mode in the view (the commands take them; the menu uses the defaults).
- Unsaved edits in the spec files themselves show once saved; the code side is live.
- The specs directory is fixed at `specs/`.

## Validation

`cargo test -p codecraft-spec` (74 tests: parsers on hostile and malformed input, link scanner,
resolution, status, code fingerprints, unspecified-code detection, edits, the agent briefs,
baseline round-trip and corruption, scaffold, symlinks);
`cargo test -p codecraft-engine --lib spec_view` (15: scan, resolution, sync, edits, live and
unsaved code, unspecified items, scaffold, open); `--test spec_agents` (6: capabilities, `waitMs`,
agent edits showing as spec or code changes, agents refused the two agent-starting commands) and
`--test spec_implement` (4: fake agent CLIs in a real git repository: Implement end to end through
review and accept, Reconcile with an accepted spec edit and the sync that follows, a task-led
brief, refused requests); `cargo test -p codecraft-ui-egui --test spec_ui` (12: layout under
Project, statuses, clicks, editing, unspecified list, the agent menu items, chips and inline
refusals); `cargo test -p codecraft-workspace` (the lower-left slot). Strict clippy for the touched
crates, `cargo xtask layers` and `cargo xtask parity` (unchanged: 192/192 core, 407/407 in scope)
pass. Not exercised against a real agent CLI: the fake CLIs print the stream formats the real
ones use, as for every other agent test.
