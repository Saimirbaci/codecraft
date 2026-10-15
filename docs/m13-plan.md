# M13: daily-use depth II

Status: done (2026-10-15; all five tracks landed, in-scope parity 407/407; remaining depth is in `docs/gaps.md` items 47–51). Goal: close the "Remaining" lists in `docs/gaps.md` that a
daily user hits first (Git/GitHub, the database work after M12, nothing lost on restart, the
Settings a user expects to change, run configurations) and the last six RustRover menu items, so
in-scope parity reaches 407/407. Same rules as M4/M10–M12: read `AGENTS.md` and
`docs/clean-room.md` (behaviour from public documentation and the running apps only, never
JetBrains code or assets).

Tracks work in their own git worktrees; the lead merges them. Disk is tight: at most four tracks
build at once, each with `CARGO_TARGET_DIR=<repo>/target-<track>`, `CARGO_INCREMENTAL=0`,
`CARGO_BUILD_JOBS=3`.

## Wave 1

| Track | Scope | Owns (new files unless noted; shared files surgically) |
|---|---|---|
| VC Git and GitHub | Changelists (Commit tool window groups, move files/hunks between lists, active list, commit one list, `Tasks & Contexts › Add Change List for 'Default task'`, the dynamic "Edit '<task>'" label); Shelve/Unshelve (patches under `.codecraft/shelf`, a Shelf tab, partial unshelve) if not already there; an editable three-pane merge editor (yours/result/theirs, the result editable, per-change accept arrows, conflict counter, Apply); the gutter change popup shows whether a hunk is staged; GitHub through `gh`: View Pull Requests (list, filters, details, files and diff, checkout, review comments read-only, merge), Share Project on GitHub, Sync Fork | `crates/vcs/src/{changelist,shelf}.rs`, `engine/src/vcs/changelists.rs`, `engine/src/github_prs.rs`, `ui-egui/src/merge_editor.rs`, `ui-egui/src/pull_requests_ui.rs`; surgical in existing vcs/commit UI |
| DX DataGrip follow-ups | Savepoint commands (set/rollback to/release, shown in the transaction indicator); server-side statement timeout (PG `statement_timeout`, MySQL `max_execution_time`, SQL Server `SET LOCK_TIMEOUT`/query timeout); the pending-transaction guard also on closing a secondary project window and opening another project in the window; metadata cache across disconnects (introspected tree kept in `$CODECRAFT_HOME`/project, shown greyed until refreshed); script Run from Caret / Run Selection in a script with the log; schema and table pickers in the Data Tools forms; SQL Server: every result set of a batch and Create/Modify Table; DuckDB Explain Plan; Settings › Database / SQL page (`sql.settings`, `sql.chooseDataSource`: dialect per file/folder, default data source); SQLite schema (attached databases) selector; DDL temp tabs cleaned; Redis consoles without SQL completion; rename of column aliases | `crates/db/src/savepoint.rs`, `engine/src/database/{meta_cache,sql_settings}.rs`, `ui-egui/src/database_settings_ui.rs`; surgical in `crates/db`, `engine/src/database*`, `ui-egui/src/database_*` |
| SR Nothing lost on restart | Hot exit: unsaved and untitled buffers (and scratch buffers without a path) saved to `.codecraft/unsaved/` (config dir when read-only) on quit, crash-safe autosave of their text every few seconds, restored dirty on reopen with a "restored unsaved changes" banner and a diff against disk; Quit no longer prompts when hot exit is on (Settings › System Settings); per-tab soft-wrap/column-mode state saved; project windows' positions and sizes restored; agent sessions: two windows on one project share live jobs (no false Interrupted), a failed `claude --resume` retries with the transcript seed, pruning removes interrupted sessions' worktrees (asking first in the UI), History search/filter/export (Markdown/JSON) | `engine/src/hot_exit.rs`, surgical in `engine/src/{session_state,agent_sessions}.rs`, `ui-egui` windows/agent panel |
| ST Settings and keymaps | Code Style per language (Rust writes `rustfmt.toml`/`.rustfmt.toml` options with a preview; C/C++ `.clang-format` basics; Python formatter chooser Ruff/Black/none replacing the free text; tab/indent per language); editor font family (system monospace fonts listed, fallback chain) and line spacing; editable Live Templates (add/edit/delete/variables/contexts, saved in `~/.codecraft/templates.json`); colour scheme import/export (our JSON; import of `.icls` attribute names that map to our tokens is allowed since it is the user's file); keymap copies with a parent, two-stroke (chord) shortcuts, VS Code and Emacs keymaps from their public references; TODO patterns and filters; crates.io lookups toggle (`rust.cargo.setCratesIoLookups`); file analysis levels persisted; Django Template in Associate with File Type | `engine/src/settings/{code_style,fonts,keymap_copies}.rs`, `ui-egui/src/settings_*` (new pages); surgical in `engine/src/settings*`, `keymap*` |

## Wave 2 (as wave-1 tracks finish)

| Track | Scope |
|---|---|
| RC Run configurations, tests and the last menus | Before-launch tasks (build, another configuration, external tool), configuration templates, "store as project file" (`.codecraft/runConfigurations/*.json` per configuration, shareable), per-configuration debugger options; a CMake Application configuration kind; jest/vitest result trees; Rust test locations from rust-analyzer runnables; gutter Debug for C++ tests and "all tests in file"; coverage table sorting. The last six menu items: View › Tool Windows › Zig (build steps from `zig build -l`, run/debug a step) and TypeScript (tsc project/errors, `tsconfig` targets), Code › Modify Selected Code… (Cmd+\: a prompt popup that sends the selection to the configured agent/AI provider and shows the proposal as a diff to accept), Code Completion › Enable Command Completion (typing `.` `.` in the editor offers actions), Run › Debugging Actions › Compile and Reload Modified Files (rebuild and restart the debug session keeping breakpoints, since Rust has no hot swap) |

## Rules

Never panic, no unsafe, nothing blocks the UI thread, every action is a command, tests are the
gate. Tests never touch the real `~/.codecraft`, `~/.claude`, Keychain, `~/.ssh`, Docker,
GitHub or databases other than ones they start; never run a real agent CLI, `gh` against
GitHub (use fakes on PATH) or the network. No `cargo update`. Track agents don't commit; the
lead merges.
