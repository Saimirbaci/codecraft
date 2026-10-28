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
| **Unlinked** | No code points at it. |
| **Linked** | Code points at it; no sync has been recorded. |
| **In sync** | Code points at it and its text is the one recorded at the last sync. |

"Sync" is `spec.markSynced` (context menu: *Mark In Sync*): you say "the code matches this text".
The fingerprint ignores whitespace, so re-wrapping a paragraph is not a change. The baseline is
`.codecraft/spec-state.json`: private to the checkout, not committed, so each clone starts with
everything *Linked* or *Unlinked*. Unreadable or hostile state files read as empty.

## Commands (also agent tools through `codecraft_execute`)

| Id | What |
|---|---|
| `view.toolWindows.spec` | Show or hide the Spec View |
| `spec.list {waitMs?}` | Features, requirements with status, link counts, diagnostics, dangling references. Starts a scan if none ran and answers `pending: true` until it finishes; `waitMs` (agents) waits for it |
| `spec.show {feature, id}` | A requirement's full text, status, fingerprints, linked code and tasks |
| `spec.open {feature, id?, file?}` | Open a requirement or one of the three files |
| `spec.openLink {path, line}` | Open code that points at a requirement |
| `spec.refresh` | Read the specs and scan the project again |
| `spec.new {name}` | Create `specs/<name>/` with the three files and open `requirements.md`. Never overwrites |
| `spec.markSynced {feature, id?}` | Record the current text as implemented |

Lines in results are 1-based. Agent capability: `spec.list/show/open/openLink/refresh` are `read`;
`spec.new` and `spec.markSynced` need `write`.

## How it works

- `crates/spec` (`codecraft-spec`, L0, std + serde): parsers, the reference scanner, link
  resolution, status rules, baseline and scaffold. Reading never fails on bad content (problems are
  diagnostics); files over 1 MiB are not read; symbolic links are not followed; counts and lengths
  are capped.
- `crates/engine/src/spec_view.rs`: `Session::spec`, the scan on a worker thread (first time the
  window is shown, or when an agent asks), the file watcher hook (spec files re-read, changed code
  re-scanned), unsaved buffers of open code files while the window is shown (not in Power Save
  Mode), and the `spec.*` commands.
- `crates/workspace`: `ToolWindowId::Spec`, and `Layout::left_lower`, a second left slot so the
  Spec View sits **under** Project instead of replacing it (its size is a height, remembered per
  window).
- `crates/ui-egui/src/spec_panel.rs`: drawing and clicks only.

## Not built yet

This is the first slice of the plan: the read-only viewer plus the traceability needed to make it
useful (links, dangling references, tasks, baseline, scaffold). Still open:

- **Implement** and **Reconcile**: a requirement change handed to an agent in a worktree and back
  as a diff to approve, and the reverse (a code change proposing a spec edit, classified as
  refactor / violation / evolution / addition, accepted by a human). Auto-trigger would be a
  setting, off by default.
- Code fingerprints (symbol signature and body) to detect *code changed*, and a test-result signal
  (a linked test that starts failing).
- An "Unspecified code" list (public items with no reference), opt-in per module.
- Editing in the view (add a requirement), EARS linting beyond the hint, "generate a spec from
  existing code".
- Unsaved edits in the spec files themselves show once saved; the code side is live.
- The specs directory is fixed at `specs/`.

## Validation

`cargo test -p codecraft-spec` (38 tests: parsers on hostile and malformed input, link scanner,
resolution, status, baseline round-trip and corruption, scaffold, symlinks);
`cargo test -p codecraft-engine --lib spec_view` (6: scan, resolution, sync and edit,
live and unsaved code, scaffold, open); `cargo test -p codecraft-ui-egui --test spec_ui` (3: layout
under Project, content and statuses, clicks, empty project); `cargo test -p codecraft-workspace`
(the lower-left slot). The whole engine test run (53 suites, 893 library tests), strict
clippy for the touched crates, `cargo xtask layers` and `cargo xtask parity` (unchanged:
192/192 core, 407/407 in scope) pass.
