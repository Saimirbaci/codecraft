# AGENTS.md: guide for AI agents and contributors

CodeCraft is an open-source, native Rust IDE written in **Rust only**: a clean-room
reimplementation of the behaviour and workflow of JetBrains RustRover. **No Tauri, Electron,
webview or JVM.** The desktop app is native egui/eframe. Language intelligence comes from
rust-analyzer over LSP; build/run/test from Cargo. The app's display name is **Code Craft**
(window title, app menu, About, installers: `codecraft_engine::about::DISPLAY_NAME`); prose and
docs still say CodeCraft; machine names are lowercase (`codecraft-*` crates, `codecraft` binary).
MIT licensed.

The goal is RustRover parity: the same menus, shortcuts (macOS keymap first), tool windows and
behaviour, with a smaller, faster native app where every action is drivable by agents.

## 1. Orientation

| Read | Why |
|---|---|
| `docs/architecture.md` | Crate map, layers, the engine/UI seam, threading model |
| `docs/clean-room.md` | What we may and may not use from RustRover. **Read before any observation work.** |
| `docs/observation/` | Evidence captured from the installed RustRover (menus, shortcuts, bundle inventory) |
| `docs/target-app-parity.md` | Where we stand against RustRover, dimension by dimension |
| `docs/gaps.md` | Ranked work list |
| `docs/parity-checklist.md` | Generated: every observed RustRover menu item, live or missing |
| `docs/roadmap.md` | Milestones and the current focus |

## 2. Workspace map

```text
crates/
  text syntax fs lsp cargo   L0 standalone (no workspace deps): text model, lexical syntax,
                                project files, LSP client, Cargo integration
  workspace                  L1 pure-data model: project, documents, editors, tool windows, run configs
  engine                     L2 Session + services + command registry (every action is a command)
  ui-egui                    L3 egui shell (thin: all actions go through the engine)
apps/
  codecraft                  desktop app (eframe)
  codecraft-cli              headless CLI (commands, check, diagnostics)
xtask/                       cargo xtask layers | catalog | parity
tools/observe/               observation scripts (Accessibility menu dump), dev-only
```

**Layering is enforced** by `cargo xtask layers`: a crate depends only on lower layers; L0 crates
depend on nothing in the workspace; nothing below `ui-egui` uses egui/eframe/rfd.

## 3. Golden rules

### Never crash

A crash loses someone's code. Malformed files, bad command params, a dead language server, a
cargo process that prints garbage, a full disk: all produce an error the user or agent can act
on, never a panic.

- **Non-test code never panics.** No `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!`,
  `unimplemented!`. The workspace lints deny them; `clippy.toml` allows them in tests only.
  Return the crate's error type; use `?`, `ok_or`, `let … else`, `unwrap_or*` where a fallback is
  truly correct.
- **No `unsafe`.** `unsafe_code = "forbid"` workspace-wide.
- **Input-derived numbers are hostile.** Use `get()` not `[i]` for indices from files, params,
  positions or arithmetic on them; slice strings only at char boundaries; `checked_*`/`saturating_*`
  for lengths and offsets; cap allocations sized by input.
- **Don't cascade.** Handle lock poisoning (`lock().unwrap_or_else(PoisonError::into_inner)`);
  thread joins are `Result`s; a dead child process is an event, not a panic.
- **Prove it.** Every crash fix comes with a regression test that panicked before.

### The rules

1. **Everything is a command.** User-visible behaviour = a command in the engine
   (`crates/engine/src/*_cmds.rs`, registered in `commands.rs`) with id, label, menu path,
   shortcut, params doc, `enabled` and `run`. The UI, CLI and (later) MCP dispatch by id. Use the
   **exact id from `crates/engine/src/catalog.rs`** (generated from the observed RustRover menus by
   `cargo xtask catalog`) and the menu item goes live. Pure view state (panel sizes, scroll) lives in
   the shell.
2. **Clean-room.** We study RustRover for *behaviour and look only*: what it does, its menus,
   shortcuts and layout, observed from the running app and public documentation. **Never decompile,
   disassemble or copy JetBrains' proprietary code, icons, color-scheme files or other assets.** See
   `docs/clean-room.md`. Permissively licensed references (IntelliJ Community, Apache-2.0; the
   archived intellij-rust plugin, MIT) may be consulted for behaviour, with attribution in
   `ATTRIBUTION.md` if anything is adapted.
3. **Coordinates.** The text model uses char offsets; LSP uses (line, UTF-16 column); rustc uses
   1-based (line, char column). Convert only through `codecraft_text::Document` helpers.
4. **Nothing blocks the UI thread.** Language server, cargo, file scans and find-in-files run on
   worker threads and report through channels the engine drains in `Session::pump()`.
5. **Tests are the gate.** Every change comes with tests. L0 crates use synthetic inputs and
   fixtures; never depend on the network. Tests that need `cargo`/`rust-analyzer` binaries skip
   cleanly when they are missing.
6. **The UI is thin and data-driven.** State the control channel may need lives in serde types in
   `workspace`/`engine`; colours come from `ui-egui/src/theme.rs` tokens, never hard-coded.

## 4. Before you finish a task

```sh
cargo test -p <crates you touched>
cargo clippy -p <crates> --all-targets -- -D warnings
cargo xtask layers
cargo xtask parity    # if you added commands; commit the regenerated docs/parity-checklist.md
```

Don't run `cargo update`: `unicode-ident` is pinned to 1.0.24 so it matches
`unicode-properties` (ra-ap-rustc_lexer checks that they agree at compile time).

## 5. Parallel agents

- Use your own target dir (`CARGO_TARGET_DIR=target-<name>`) to avoid the build lock, and edit
  only the files you own.
- Keep every `Cargo.toml` valid at all times (the `crates/*` glob means one broken manifest breaks
  everyone). Don't add workspace dependencies without coordinating; dev-dependencies in your own
  crate are fine.
- Keep public signatures marked as contract stable; add, don't rename.
