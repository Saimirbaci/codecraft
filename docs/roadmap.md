# Roadmap

Status as of 2026-10-13: **every milestone below has landed** (M4 delivered M3b and M6–M9 in parallel
tracks, see `docs/m4-plan.md`). Menu parity: 129/131 core and 216/275 in-scope items live; see
`docs/parity-checklist.md`. What remains is depth, listed in `docs/gaps.md`.

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

## Current focus

Depth over breadth: the remaining items in `docs/gaps.md` (in-place rename, editable 3-way merge,
restoring tabs/splits/windows on restart, notarized builds, Windows/Linux testing, more
language-server settings), plus the two core menu items still missing.
