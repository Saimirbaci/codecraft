# Roadmap

Status as of 2026-10-13: **M1–M3 landed**; M4 (everything below, in parallel tracks) is in progress,
see `docs/m4-plan.md`. Menu parity: 116/131 core items live; see `docs/parity-checklist.md`.

| Milestone | Scope | Status |
|---|---|---|
| M0 Foundations | Workspace, layering, observation of RustRover, catalog → command ids, never-crash lints | ✅ |
| M1 Editor + Cargo + LSP core | Editor widget, multi-caret ops, find/replace, folding; project tree; Go to File/Symbol/Action; Cargo run/build/test with run configs; rust-analyzer completion, navigation, rename, context actions, diagnostics | ✅ |
| M2 Polyglot + debugger + databases + agents + feedback | Language servers for C/C++ (clangd), Python, Zig (zls), TypeScript; run configurations for CMake/Make/Zig/npm/pytest; DAP debugger (lldb-dap, debugpy); DataGrip-like database explorer; coding agents (Claude Code, Codex, Grok, Gemini) with Auto / Ask / Review modes; Claude Code IDE bridge; Evolvo-style feedback overlay. Plan: `docs/m2-plan.md` | ✅ |
| M3 Terminal, Git, file watching, semantic highlighting | PTY terminal with Claude Code connected to the IDE; Git commit/log/branches/remote operations, gutter markers, annotate; live file watching; semantic tokens and inlay hints. Plan: `docs/m3-plan.md` | ✅ |
| M3b Editor depth | Incremental highlighting and LSP sync, soft wrap, split editors, column selection, live templates, breadcrumbs, navigation bar | M4 wave 1 |
| M6 Test runner | Test tree tool window, rerun failed, history, coverage | M4 wave 1 |
| M7 Refactorings | Extract/inline/change signature via language-server assists, with preview; file operations; hierarchies | M4 wave 1 |
| M8 Settings and keymaps | Full Settings dialog, Windows/Linux keymaps, colour schemes | M4 wave 2 |
| M9 Native macOS menus, packaging | NSMenu global menu bar, signed .app/.dmg, Linux and Windows packages | M4 wave 2 |

## Current focus

M4 (`docs/m4-plan.md`). Wave 1: editor depth, split editors with incremental highlighting/sync,
the test runner with coverage, refactorings with preview. Wave 2: settings and keymaps, native
menus and packaging, and the M2/M3 follow-ups in `docs/gaps.md`.
