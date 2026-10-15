# Roadmap

Status as of 2026-10-15: **M0–M14 have landed** (M4 delivered M3b and M6–M9 in parallel
tracks, see `docs/m4-plan.md`). Menu parity (after capturing the lazily filled submenus): 192/192 core and 407/407 in-scope items
live; see `docs/parity-checklist.md`. What remains is depth, listed in `docs/gaps.md`.

| Milestone | Scope | Status |
|---|---|---|
| M0 Foundations | Workspace, layering, observation of RustRover, catalog → command ids, never-crash lints | ✅ |
| M1 Editor + Cargo + LSP core | Editor widget, multi-caret ops, find/replace, folding; project tree; Go to File/Symbol/Action; Cargo run/build/test with run configs; rust-analyzer completion, navigation, rename, context actions, diagnostics | ✅ |
| M2 Polyglot + debugger + databases + agents + feedback | Language servers for C/C++ (clangd), Python, Zig (zls), TypeScript; run configurations for CMake/Make/Zig/npm/pytest; DAP debugger (lldb-dap, debugpy); DataGrip-like database explorer; coding agents (Claude Code, Codex, Grok, Gemini) with Auto / Ask / Review modes; Claude Code IDE bridge; Evolvo-style feedback overlay. Plan: `docs/m2-plan.md` | ✅ |
| M3 Terminal, Git, file watching, semantic highlighting | PTY terminal with Claude Code connected to the IDE; Git commit/log/branches/remote operations, gutter markers, annotate; live file watching; semantic tokens and inlay hints. Plan: `docs/m3-plan.md` | ✅ |
| M3b Editor depth | Incremental highlighting and LSP sync, soft wrap, split editors, column selection, live templates, breadcrumbs, navigation bar | ✅ (2026-10-13) |
| M6 Test runner | Test tree tool window, rerun failed, history, coverage | ✅ (2026-10-13) |
| M7 Refactorings | Extract/inline/change signature via language-server assists, with preview; file operations; hierarchies | ✅ (2026-10-13) |
| M8 Settings and keymaps | Full Settings dialog, Windows/Linux keymaps, colour schemes | ✅ (2026-10-13) |
| M9 Native macOS menus, packaging | NSMenu global menu bar, signed .app/.dmg, Linux and Windows packages, project windows | ✅ (2026-10-13) |
| M10 Daily-use depth | Session restore, snippet placeholders and completion resolve, in-place rename, whitespace/wrap rendering, inspections (Clippy) with quick-fixes, remaining cheap menu items, RustRover's lazily filled submenus. Plan: `docs/m10-plan.md` | ✅ (2026-10-13) |
| M11 From the suite + remote | CLion debugger views (memory, disassembly, registers, watchpoints), profiler; DataGrip query history, explain plan, grid extras, SQL in Rust strings; HTTP Client; PyCharm interpreters, packages and console; Docker in Services, SSH deployment, remote run targets and Remote Development. Plan: `docs/m11-plan.md` | ✅ (2026-10-13) |
| M12 Replace DataGrip + agent sessions | Schema tools (create/modify table, DDL, ER diagram, schema compare), robust transactions, compare data, dump/restore, SQL navigation and inspections, script runs; agent sessions saved per project and resumable; more data sources; Rust/editor/CLion/PyCharm depth. Plan: `docs/m12-plan.md` | ✅ (2026-10-14) |
| M13 Daily-use depth II | Changelists, shelf, editable merge, GitHub pull requests; DataGrip follow-ups (savepoints, server timeouts, metadata cache, SQL settings); hot exit and restored windows; Code Style, fonts, editable live templates, keymap copies and chords; before-launch tasks and the last six menu items. Plan: `docs/m13-plan.md` | ✅ (2026-10-15) |
| M14 Debug, analyze, profile for people and agents | MCP control plane over the engine (IDE and headless) with an access policy; debugging depth for Rust, C/C++ and Python verified with real adapters; Inspect Code with scopes, profiles, baselines and SARIF; CPU and memory profiling for native code and Python; a hands-on QA pass. Plan: `docs/m14-plan.md` | ✅ (2026-10-15) |

## Current focus

Every observed in-scope menu item is live. What remains is depth, in the "Remaining" parts of
`docs/gaps.md`: testing against real servers we don't have here (MySQL, SQL Server, ClickHouse,
DuckDB, SSH tunnels, GitHub), notarized builds and Windows/Linux testing, language-server-backed
SQL and structural search, MongoDB, and live progress in the C++/JS test trees.
