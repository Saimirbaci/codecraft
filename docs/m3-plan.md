# M3: Terminal and Git (with file watching)

Status: in progress (started 2026-10-12). Tracks run in parallel; each owns its files and talks
to the rest through the contracts below. Read `AGENTS.md` and `docs/m2-plan.md` (same rules).

## Tracks

| Track | Crate | Engine | UI |
|---|---|---|---|
| W File watching (lead, done) | `fs::ProjectWatcher` (notify/FSEvents) | `watch.rs`: reload unmodified docs, rescan tree, call `vcs::files_changed` | — |
| T Terminal | `term` (new, L0) | `terminal.rs`, `terminal_cmds.rs` | `terminal_panel.rs` |
| G Git | `vcs` (new, L0, over the `git` CLI) | `vcs.rs`, `vcs_cmds.rs` | `vcs_panels.rs` (Commit + Git tool windows, branch widget), VCS decorations in `editor.rs` (gutter change markers, annotate) and `panels.rs` (project tree colours) |

## Shared contracts (in the tree)

- `ToolWindowId::{Terminal, Commit, Git}` are implemented tool windows with toggles
  `view.toolWindows.{terminal,commit,git}` (Alt+F12, Cmd+0, Cmd+9) and stub panels
  `terminal_panel::show`, `vcs_panels::show` (Commit) and `vcs_panels::show_git` (Git).
- `Session::terminal: TerminalState`, `Session::vcs: VcsState`, each with `pump`/`shutdown`.
- **File changes:** the engine's watcher calls `crate::vcs::files_changed(&mut Session, &[PathBuf])`
  (debounced batches, including `.git/HEAD`, `.git/index`, `.git/refs/*`) — track G refreshes
  status from it (no polling needed beyond that).
- **Claude Code in a terminal:** `Session::ide.env_for_child()` returns the environment that makes
  `claude` started in a CodeCraft terminal connect to CodeCraft automatically (track T sets it
  on every shell it spawns).
- **Diffs:** reuse `crates/ui-egui/src/diff_view.rs` and `codecraft_agent::diff` (the `similar`
  crate) for every diff view; extend them only additively.
- Command ids: use the catalog's `git.*` ids (Git menu: commit `Cmd+K`, push `Shift+Cmd+K`,
  update project `Cmd+T`, branches, new branch `Alt+Cmd+N`, …) and `view.toolWindows.*`.

## Rules

Same as M2: never panic, no unsafe, nothing blocks the UI thread, process groups killed on
stop/shutdown, tests skip cleanly without external tools, own `CARGO_TARGET_DIR`, surgical edits
to shared files only, no `cargo update`, no git commits by track agents.
