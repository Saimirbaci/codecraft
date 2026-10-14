# M12: replace DataGrip, persistent agent sessions, and the rest of the suite

Status: done (2026-10-14; all nine tracks landed; remaining depth is in `docs/gaps.md` items 38–46). Goal: a user can drop DataGrip and do their daily database
work in CodeCraft, reliably (every transaction ends committed or rolled back, never half-done);
agent chats survive restarts and can be resumed; and the remaining RustRover menus plus the
headline CLion/PyCharm features land. Same rules as M4/M10/M11: read `AGENTS.md` and
`docs/clean-room.md` (behaviour from public documentation and the running apps only, never
JetBrains code or assets).

Already done before the tracks: SSH tunnels defined on the data source (host, port, user, agent /
key / password, like DataGrip's SSH/SSL tab), commit `79ced9a`.

Tracks work in their own git worktrees (no half-built code from another track breaks a build);
the lead merges them. Disk is tight: at most four tracks build at once, each with
`CARGO_TARGET_DIR=<repo>/target-<track>`, `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=3`.

## Wave 1

| Track | Scope | Owns (new files unless noted; shared files surgically) |
|---|---|---|
| DA DataGrip schema tools | Create Table / Modify Table dialogs (columns, types, defaults, nullability, primary key, indexes, foreign keys, checks) with a live SQL preview per dialect; Drop / Rename / Truncate with preview and confirmation; SQL Generator (DDL of a table, view, index, schema; Copy DDL); ER diagram of a schema (tables, columns, FK edges, auto layout, zoom/pan, export PNG/SVG-less image); Compare schemas (two data sources or schemas: added/removed/changed tables, columns, indexes, FKs) with a migration script; column rename that offers to update the project's SQL files | `crates/db/src/ddl.rs`, `crates/db/src/schema_diff.rs`, `engine/src/database/schema_tools.rs`, `engine/src/database_cmds/schema.rs`, `ui-egui/src/database_schema_ui.rs`, `ui-egui/src/er_diagram.rs` |
| DB DataGrip data and transactions | Robustness first: transaction modes (auto/manual) with isolation levels, a transaction indicator and Commit/Rollback on every console and grid; pending-transaction guard (ask Commit/Rollback on disconnect, closing a console, project close, quit); grid edits submitted in one transaction with rollback and an exact error on failure; statement timeout and Cancel that really cancels (PG cancel request, MySQL KILL QUERY); connection keep-alive and transparent reconnect of idle connections (never retrying a write); reconnect state visible. Then: Compare Data (two tables or queries, by key; rows added/removed/changed); Dump and Restore with `pg_dump`/`pg_restore`/`psql`, `mysqldump`/`mysql`, SQLite `.dump` (found on PATH; tests use fakes); full-table export to file streamed in pages (CSV/TSV/JSON/SQL INSERTs/Markdown/XLSX-less); Full-text search across tables of a schema | `crates/db/src/tx.rs`, `crates/db/src/compare.rs`, `crates/db/src/dump.rs`, `engine/src/database/tx.rs`, `engine/src/database/data_tools.rs`, `engine/src/database_cmds/data.rs`, `ui-egui/src/database_data_ui.rs`; surgical in `crates/db/src/{pg,my,sqlite,lib}.rs`, `engine/src/database.rs` |
| DC DataGrip SQL editor and explorer | Go to Declaration from SQL to the table/column (opens its DDL or the tree node), Quick Documentation (DDL and comment), Find Usages of a table/column in the project's SQL and Rust SQL strings; SQL inspections (UPDATE/DELETE without WHERE, unresolved table/column, ambiguous column, INSERT column count mismatch) with quick-fixes; rename of an alias/CTE in a statement; Run SQL file against a data source (attach `.sql` files to a data source/schema; script mode: stop on error / continue / ask; per-statement results and an execution log); Go to Database Object (popup over every table/view/routine of connected sources); explorer filter, schemas to introspect per data source, object counts; pinned result tabs and several result sets per run; SQL live templates (`sel`, `ins`, `upd`, `del`, `j`) | `engine/src/database/sql_nav.rs`, `engine/src/database/script.rs`, `engine/src/database_cmds/sql_editor.rs`, `ui-egui/src/database_editor_ui.rs`; surgical in `engine/src/sql_lang.rs`, `syntax/src/sql*.rs`, `ui-egui/src/database_panel.rs` |
| AG Agent sessions | Every agent job (Agents tool window, feedback runs, Generate run configurations) is saved per project under `$CODECRAFT_HOME/agent-sessions/<project key>/` (prompt, activity feed, status, cost, backend, mode, the backend's own session id, worktree); the Agents tool window lists earlier sessions after a restart; Resume continues one (`claude --resume <id>`, ACP `session/load` when offered, Codex `exec resume`, else a new session seeded with the transcript); delete/rename sessions; size caps and pruning | `engine/src/agent_sessions.rs`, surgical in `engine/src/agents.rs`, `agent_cmds.rs`, `feedback*.rs`, `crates/agent`, `ui-egui/src/agent_panel.rs` |

## Wave 2 (as wave-1 tracks finish)

| Track | Scope |
|---|---|
| DD More data sources | Redis (keys browser, types, TTL, console), ClickHouse (HTTP interface), Microsoft SQL Server, DuckDB (CLI), MongoDB if the driver is affordable |
| RS Rust depth | Show macro expansion (`rust-analyzer/expandMacro`), Cargo.toml dependency versions from crates.io (completion, "newer version" hint, update), Rust REPL (evcxr), `cargo audit`/`cargo deny` in Inspect Code, postfix completion |
| ED Editor and IDE | Structural Search/Replace, macros (record, play, save), Locate Duplicates, Data Flow to/from Here, Run Inspection by Name, Silent Code Cleanup, Configure Current File Analysis, Save File as Template, Update Copyright, window layouts, Active Tool Window actions, Tasks & Contexts, Markdown export, Presentation Assistant |
| CL CLion | GoogleTest/Catch2/doctest/Boost.Test runners, CMake presets and a CMake tool window, Valgrind Memcheck, sanitizer reports, Conan/vcpkg, Attach to an Unstarted Process, Reset Frame, Manage Targets |
| PY PyCharm | Jupyter notebooks (`.ipynb`: cells, run in a kernel process, text/image outputs), DataFrame/array viewer, Ruff/Black formatting, Django/Jinja templates, Python Process Output |

## Rules

Never panic, no unsafe, nothing blocks the UI thread, every action is a command, tests are the
gate. Tests never touch the real `~/.codecraft`, Keychain, `~/.ssh`, Docker or databases other
than ones they start; never run a real agent CLI, `pg_dump` against a real server or the network.
No `cargo update`. Track agents don't commit; the lead merges.
