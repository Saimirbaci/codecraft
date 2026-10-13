# M10: daily-use depth

Status: in progress (started 2026-10-13). After M4 the menus are at 98% of RustRover's core;
this milestone closes the gaps you feel while editing every day. Tracks run in parallel; read
`AGENTS.md` and `docs/m4-plan.md` (same rules and contracts). (M4/M5 numbers were folded into
M2/M3 earlier, hence the jump.)

| Track | Scope | Owns |
|---|---|---|
| A Session restore | Reopen the last project(s) on start with their windows; per project: open tabs, split layout and focused group, carets, scroll, folds, pinned tabs, tool-window layout; restore after crash (periodic save); restore unsaved buffers' text is out of scope (prompt on quit stays) | `ui-egui/src/state.rs` (PersistedState), project-window restore in `project_windows.rs`, a workspace-side serde snapshot (`workspace/src/session_state.rs`), engine commands `session.save/restore/state` |
| B Completion + in-place rename | LSP snippet placeholders (`$1`, `${2:x}`, `$0`, choices) driven by the template-variable machinery from M3b (Tab/Shift+Tab, mirrored edits); `completionItem/resolve` (documentation popup beside the list, `additionalTextEdits` such as auto-imports applied on accept); auto-popup tuning (delay, trigger characters from server capabilities, fuzzy/prefix ranking with server `sortText`/`filterText`, case-insensitive camel-hump matching); Type-Matching completion (`Ctrl+Shift+Space`: filter by expected type when the server marks it, else re-rank); in-place rename (Shift+F6 edits the symbol in the editor with local occurrences mirrored and highlighted, Enter commits through LSP rename + the existing preview for multi-file, Escape cancels; the popup stays as fallback when prepareRename gives no range) | `engine` completion code (`lsp.rs` completion parts, new `completion.rs`), `engine/editing.rs` variable frames (shared with templates — extend additively), completion popup in `ui-egui/src/popups.rs` (surgical) |
| C Editor rendering | Show whitespace (spaces/tabs, trailing only option); wrapped rows keep the line's indentation; inlay hints and semantic tokens in every visible split view (not only the active editor); clickable parameter-name inlay hints (Cmd+click navigates); box selection virtual space; IME pre-edit (composition) rendering; Custom Folding regions (`// region` / `// endregion`, `#region`) and Navigate › Custom Folding… | `ui-egui/src/editor.rs`, `engine/decorations.rs`, `syntax` folding |
| D Inspections | Inspect Code… (scope: file/directory/project) running `cargo clippy --message-format=json` (+ other linters when installed: ruff, eslint, clang-tidy, zig fmt --check) into an Inspection Results tool window grouped by inspection/severity with quick-fixes (machine-applicable suggestions applied through the refactoring preview); Code Cleanup… (`cargo clippy --fix`-equivalent applied from suggestions with preview, plus rustfmt); Analyze Stack Trace or Thread Dump… (paste a panic/backtrace/Python traceback → clickable frames); Rearrange Code (sort items by kind per a simple rule set, preview) if time permits | `engine/inspections.rs`, `engine/inspection_cmds.rs`, `ui-egui/src/inspections_ui.rs`, a new `ToolWindowId::InspectionResults` (surgical in `layout.rs`/`view_cmds.rs`/`panels.rs`) |
| F Menu sweep | The cheap remaining menu items: Tool Windows › Database Changes (pending grid edits across consoles), Recently Changed Files and Navigate › Recent Changes, Compare With… (two files), Compare with Clipboard, Quick Type Definition, Context Info (`Ctrl+Shift+Q`), Stop Background Processes (`Shift+Cmd+F2`), Show Running List, Processes › Show/Auto Show, File Properties › File Encoding/Remove BOM/Add BOM/Make file read-only/Associate with File Type, Keep Tab Open (preview tab: single-click in the project view opens a preview tab that is replaced until edited), Show Hidden Tabs, Configure Editor Tabs (settings page), Power Save Mode (no auto-popup/inlays/background checks), Fill Paragraph (comments), Export to HTML (highlighted), New › Python Package, GitHub/GitLab › Manage Accounts (`gh auth status`/login hint) | new `engine/misc_cmds.rs` + surgical additions where the feature lives |

## Rules

Same as M4: never panic, no unsafe, nothing blocks the UI thread, tests skip cleanly without
external tools, own `CARGO_TARGET_DIR=target-<track>` with `CARGO_INCREMENTAL=0` (disk is tight;
never delete another track's target dir), surgical edits to shared files, no `cargo update`, no
git commits by track agents, tests never touch the real `~/.codecraft` / settings
(`CODECRAFT_HOME`, `CODECRAFT_CONFIG_DIR` overrides).

Editor file ownership: track C owns `ui-egui/src/editor.rs` rendering; tracks B and F add
small hooks there only.
