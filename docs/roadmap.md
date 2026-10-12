# Roadmap

Status as of 2026-10-12: **M1 landed** (the IDE opens, edits, navigates, builds, runs and tests Rust
projects). Menu parity: 111/129 core items live; see `docs/parity-checklist.md`.

| Milestone | Scope | Status |
|---|---|---|
| M0 Foundations | Workspace, layering, observation of RustRover, catalog → command ids, never-crash lints | ✅ |
| M1 Editor + Cargo + LSP core | Editor widget, multi-caret ops, find/replace, folding; project tree; Go to File/Symbol/Action; Cargo run/build/test with run configs; rust-analyzer completion, navigation, rename, context actions, diagnostics | ✅ |
| M2 Editor depth | Semantic highlighting (LSP semantic tokens), inlay hints, incremental highlighting, soft wrap, split editors, column selection, live templates, Complete Current Statement, Auto-Indent, Optimize Imports, breadcrumbs, navigation bar | next |
| M3 Terminal | Terminal tool window (pty + VT parser, all in Rust) | |
| M4 VCS | Git: commit tool window, diff viewer, log, branches, blame/annotate, local history | |
| M5 Debugger | Debug run configurations over LLDB/GDB (DAP via `lldb-dap` / CodeLLDB), breakpoints, frames, variables, evaluate | |
| M6 Test runner | Test tree tool window, rerun failed, history, coverage (`cargo llvm-cov`) | |
| M7 Refactorings | Extract function/variable, inline, change signature, move: via rust-analyzer assists, with a refactoring preview | |
| M8 Settings and keymaps | Full Settings dialog, Windows/Linux keymaps, color schemes, settings sync to file | |
| M9 Native macOS menus, packaging | NSMenu global menu bar, signed .app/.dmg, Linux and Windows packages | |
| M10 Agents | JSON control channel and MCP server (every command already has an id and params) | |

## Current focus

M2: semantic highlighting and inlay hints from rust-analyzer, then incremental highlighting
(today the whole file is lexed on every edit), then split editors.
