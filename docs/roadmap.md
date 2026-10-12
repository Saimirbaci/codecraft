# Roadmap

Status as of 2026-10-12: **M1 landed, M2 in progress** (the IDE opens, edits, navigates, builds, runs and tests Rust
projects). Menu parity: 111/129 core items live; see `docs/parity-checklist.md`.

| Milestone | Scope | Status |
|---|---|---|
| M0 Foundations | Workspace, layering, observation of RustRover, catalog → command ids, never-crash lints | ✅ |
| M1 Editor + Cargo + LSP core | Editor widget, multi-caret ops, find/replace, folding; project tree; Go to File/Symbol/Action; Cargo run/build/test with run configs; rust-analyzer completion, navigation, rename, context actions, diagnostics | ✅ |
| M2 Polyglot + debugger + databases + agents + feedback | Language servers for C/C++ (clangd), Python, Zig (zls), TypeScript; run configurations for CMake/Make/Zig/npm/pytest; DAP debugger (lldb-dap, debugpy); DataGrip-like database explorer; coding agents (Claude Code, Codex, Grok, Gemini) with Auto / Ask / Review modes; Claude Code IDE bridge; Evolvo-style feedback overlay. Plan: `docs/m2-plan.md` | in progress |
| M3 Editor depth | Semantic highlighting, inlay hints, incremental highlighting, soft wrap, split editors, column selection, live templates, breadcrumbs, navigation bar | |
| M4 Terminal | Terminal tool window (pty + VT parser), launching agents already connected to the IDE | |
| M5 VCS | Git: commit tool window, diff viewer (shared with agent review), log, branches, blame, local history | |
| M6 Test runner | Test tree tool window, rerun failed, history, coverage | |
| M7 Refactorings | Extract/inline/change signature via language-server assists, with preview | |
| M8 Settings and keymaps | Full Settings dialog, Windows/Linux keymaps, colour schemes | |
| M9 Native macOS menus, packaging | NSMenu global menu bar, signed .app/.dmg, Linux and Windows packages | |

## Current focus

M2 (see `docs/m2-plan.md`): the debugger first (the most-used workflow), then databases and
agent modes, with the language platform (multi-server LSP, memory supervision, polyglot run
configurations) underneath.
