# Architecture

## Crates and layers

| Layer | Crate | Role |
|---|---|---|
| L0 | `codecraft-text` | Rope `Document` (char offsets, `\n` internally, line-ending memory), undo groups, `SelectionSet` (multi-caret), `ChangeSet`, editing `ops`, caret `movement`, in-document `search`. |
| L0 | `codecraft-syntax` | `Language` detection; lexical highlighting (Rust via `ra_ap_rustc_lexer`, TOML, Markdown, JSON, YAML, shell); bracket matching; folding ranges; lexical outline. |
| L0 | `codecraft-fs` | Ignore-aware `FileTree` scan, IntelliJ-style `fuzzy_match`/`rank`, `find_in_files`, safe text I/O (binary detection, size caps, atomic writes). |
| L0 | `codecraft-lsp` | Thread-based LSP client: framing, request/response correlation, handshake + queueing, auto-answers to common server requests, rust-analyzer discovery, `file://` URIs. |
| L0 | `codecraft-cargo` | `cargo metadata` model, `CargoInvocation` builder, process runner streaming `RunEvent`s (lines, compiler messages, libtest results, exit), rustfmt. |
| L1 | `codecraft-workspace` | Pure data: `Workspace` (project, documents, editors/tabs, problems, run tabs, find results, navigation history, layout, notifications, settings). |
| L2 | `codecraft-engine` | `Session` (workspace + popups + services), the command `registry`, the observed menu `catalog`. |
| L3 | `codecraft-ui-egui` | The egui shell: menu bar from the catalog, editor widget, tool windows, popups, keymap. |
| app | `codecraft` | Desktop binary (eframe). |
| app | `codecraft-cli` | Headless binary over the same engine. |

`cargo xtask layers` fails if a crate depends on its own layer or a higher one, or if anything
below L3 depends on a UI crate.

## Everything is a command

`CommandSpec { id, label, shortcut, params, context, enabled, run }`. Ids for menu items come from
`crates/engine/src/catalog.rs`, which `cargo xtask catalog` generates from the observed RustRover
menus (`docs/observation/rustrover-2026.1-menus.json`). Registering a spec with a catalog id puts
it live in the menu with RustRover's shortcut. Editor actions that have no menu entry (typing,
caret motion, clicks) are commands too (`editor.*`).

`Session::execute(id, params)` checks `enabled`, runs the command inside `catch_unwind` (a last-resort
guard; the panic-hunt test makes sure it never triggers), and returns JSON.

Keymap: each spec's own shortcut wins over the catalog shortcut for the same key and context.
`Context::Editor` shortcuts apply only while the editor has focus.

## Threads and the pump

```
UI thread (egui frame)
  ├─ Session::pump()   drain: LSP events · cargo RunEvents · job results (scan, find, rustfmt)
  ├─ input → keymap → Session::execute(id, params)
  └─ paint (reads Workspace; editor widget turns pointer input into editor.* commands)

worker threads
  ├─ codecraft-lsp: writer, reader, stderr, reaper
  ├─ codecraft-cargo: stdout/stderr readers, waiter (per process)
  └─ jobs: project scan + cargo metadata, find in files, rustfmt
```

Document sync to rust-analyzer is full-text, at most once per frame per changed document, and
always flushed before a position-based request.

## Coordinates

- Text model: char offsets; `Position { line, column }` with char columns.
- LSP: (line, UTF-16 column): convert with `Document::offset_to_utf16` / `utf16_to_offset`.
- rustc: 1-based line and char column: `runs::problem_from_diagnostic` converts.

## Engine-owned UI state

Popups (`Popup`), the find bar (`FindBar`), the status message and effects (`Effect`) live in the
engine, so the CLI or an agent can read and drive them like the UI does. The shell keeps only
view state: caches, focus, panel sizes, caret screen position.
