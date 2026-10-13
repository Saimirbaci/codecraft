# M4: the rest of the roadmap (M3b, M6–M9 and the M2/M3 follow-ups)

Status: done (2026-10-13; all seven tracks landed, 1,095 tests passing). Tracks run in parallel in two waves; each owns its
files and talks to the rest through the contracts below. Read `AGENTS.md` and `docs/m3-plan.md`
(same rules).

## Wave 1

| Track | Scope | Owns |
|---|---|---|
| E Editor depth (M3b) | Soft wrap; column selection mode (`Shift+Cmd+8`) and Alt+drag box selection; breadcrumbs under the editor and the navigation bar (`Cmd+Up`); live templates (`Cmd+J`, Tab expansion, `$VAR$` placeholders with Tab/Shift+Tab, Save as Live Template, built-in Rust/Python/TS/C templates), Surround With (`Alt+Cmd+T`), Unwrap/Remove; Next Edit Location; Complete Current Statement; Auto-Indent Lines; Optimize Imports (LSP `source.organizeImports`); Move Element Left/Right; Find › Scroll to Results During Typing; Cyclic Expand Word (`Alt+/`) | `engine/editing_cmds.rs` (+ new engine modules it needs, e.g. `templates.rs`), `text` crate additions, `ui-egui/src/editor.rs` (rendering: wrap, column selection, breadcrumbs), new `ui-egui/src/navbar.rs` |
| S Splits + incremental | Editor groups: split right/down, split and move, unsplit, unsplit all, go to next/previous splitter, change splitter orientation, maximize/normalize, close all but pinned, close read-only, keep tab open; each group has its own tab bar. Incremental lexing (re-lex only from the edited line, with lexer state at line starts) and incremental LSP sync (`TextDocumentSyncKind::Incremental` when the server offers it). Reformat keeps the caret (map through the diff). | `workspace` editor/tab model (`lib.rs` editors section, new `splits.rs`), `engine/split_cmds.rs`, `engine/window_cmds.rs`, `ui-egui/src/chrome.rs` (`editor_area`, `tabs`), `syntax` (incremental highlight API), `lsp` sync, `engine/lsp.rs` sync code |
| T Test runner + coverage (M6) | Test tree for cargo test (libtest output), pytest, ctest/zig test where cheap: suites → tests, pass/fail/ignored, durations, output per test, navigate to test; Rerun Failed, Rerun, toggle "show passed", test history (last N runs, persisted under the project's `.codecraft/`); coverage via `cargo llvm-cov --lcov` (install hint when missing), Run with Coverage, Coverage tool window (per-file %), gutter coverage marks, Manage Coverage Reports | `engine/testing.rs`, `engine/testing_cmds.rs`, test parsing in `runs.rs`/`cargo` crate (additively), `ui-egui/src/testing_panel.rs`, the test-tree part of `runs_view` in `panels.rs` |
| R Refactorings (M7) | Refactor This (`Ctrl+T`) listing LSP refactor code actions; extract variable/function/constant, inline, change signature through rust-analyzer assists, each with a diff preview (reuse `diff_view.rs`) before applying; rename with preview of all edits and conflict reporting; Move File (`F6`), Copy File (`F5`), Safe Delete (`Cmd+Backspace`, checks references first); project-view rename/delete/drag-and-drop; Call Hierarchy (`Ctrl+Alt+H`), Type Hierarchy (`Ctrl+H`) via LSP, Super Method (`Cmd+U`), Generate (`Cmd+N`), Implement/Override Methods (`Ctrl+I`/`Ctrl+O`) | `engine/refactor.rs`, `engine/refactor_cmds.rs`, `lsp` crate request helpers (additively), `ui-egui/src/refactor_ui.rs`, project-view context menu in `panels.rs` |

## Wave 2 (starts as wave-1 tracks finish)

| Track | Scope |
|---|---|
| K Settings and keymaps (M8) | Full Settings dialog (Appearance, Editor, Keymap, Languages, Tools, Version Control), Windows/Linux keymap (Ctrl-based) selectable in settings, keymap editor (rebind, conflicts), colour schemes (Dark, Light, High contrast) and Quick Switch Scheme (`` Ctrl+` ``), Edit Configurations dialog (args, env, working dir, persisted) |
| N Native menus and packaging (M9) | Global macOS menu bar (`muda`) driven by the same catalog/menus as the in-window bar; `cargo xtask bundle` → `CodeCraft.app` (Info.plist, icon, ad-hoc codesign) and `.dmg` (hdiutil); Linux tarball + `.desktop`; Windows zip; multiple project windows (Next/Previous Project Window); Help |
| X Follow-ups | Debugger: `&str`/`String` summaries, attach to process; Git: 3-way merge view, per-hunk staging, worktrees, GitHub via `gh` (clone, create PR); database: in-grid editing, Postgres TLS; bookmarks (F3, `Cmd+F3`, Bookmarks tool window), TODO tool window, local history, scratch files (`Shift+Cmd+N`); gutter run icons for C/C++/Python/Zig/TS |

## Shared contracts (in the tree)

- **Tool windows:** `ToolWindowId::{Bookmarks, Todo, Coverage, Hierarchy}` are live with toggles
  `view.toolWindows.{bookmarks,todo,coverage,hierarchy}` and stub panels
  `bookmarks_panel::{show_bookmarks, show_todo}`, `testing_panel::show_coverage`,
  `refactor_ui::show_hierarchy`.
- **Session fields:** `Session::testing: testing::TestingState` and
  `Session::refactor: refactor::RefactorState`, each with `pump`.
- **Command modules** are registered in `commands.rs` already: `editing_cmds` (E), `split_cmds`
  (S), `testing_cmds` (T), `refactor_cmds` (R). Use the catalog's exact ids.
- **Editors:** `ui-egui::editor::show(ui, s, st, eid)` renders one editor. Track S may call it
  several times per frame (one per group); track E must keep it self-contained per `EditorId`
  (no "the active editor" assumptions inside rendering; use `eid`). `Workspace::active()` stays
  the focused group's active editor; track S keeps `ws.tabs`/`ws.active_editor` working (or
  migrates every use in the same change).
- **Previews:** every multi-file edit (refactorings, rename, templates are single-file) goes
  through a preview built on `ui-egui/src/diff_view.rs` and `codecraft_agent::diff`; agents'
  approval cards already use them.
- **Edits:** text changes go through `Session` edit helpers so undo, LSP sync, decorations and
  `last_edit` stay consistent.

## Rules

Same as M2/M3: never panic, no unsafe, nothing blocks the UI thread, process groups killed on
stop/shutdown, tests skip cleanly without external tools (`cargo-llvm-cov`, language servers),
own `CARGO_TARGET_DIR` with `CARGO_INCREMENTAL=0` (disk is tight), surgical edits to shared
files only, no `cargo update`, no git commits by track agents.
