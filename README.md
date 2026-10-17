<h1 align="center">Code Craft</h1>

<p align="center">
  <b>A native Rust IDE, written in Rust: a clean-room reimplementation of the RustRover workflow.</b><br>
  The menus, shortcuts and tool windows you know, in a small native egui app.
  rust-analyzer provides language intelligence and Cargo builds, runs and tests.
</p>

<p align="center">
  <img alt="100% Rust" src="https://img.shields.io/badge/100%25-Rust-b7410e?style=flat-square&logo=rust">
  <img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-3a3a3a?style=flat-square">
  <img alt="Status: pre-alpha" src="https://img.shields.io/badge/status-pre--alpha-d69e2e?style=flat-square">
</p>

## What works today

- **Editor:** a custom-painted code editor with rope storage, multiple carets and selections,
  grouped undo/redo, auto-closing brackets and quotes, smart Enter and Backspace, line and block
  comments, duplicate, move and join lines, Extend/Shrink Selection, Add Selection for Next
  Occurrence, a find/replace bar (case, whole word, regex), code folding, indent guides, a
  right margin, matching-brace highlighting, identifier occurrence highlighting, inline
  diagnostics and an error stripe.
- **Rust:** lexical highlighting from `rustc_lexer`, gutter ▶ icons for `fn main`,
  `#[test]` functions and `mod tests` (they run exactly that test), File Structure and
  next/previous method.
- **rust-analyzer** (when installed): completion with auto-import, Go to Declaration or Usages,
  Implementations, Type Declaration, Find Usages, Quick Documentation, Parameter Info, Rename,
  Show Context Actions (⌥↩), Reformat Code, Go to Symbol/Type, live diagnostics and Cargo check on save.
- **Cargo:** run configurations derived from your targets; Build, Rebuild, Run, Stop, Test, Check,
  Clippy and Doc; a console with clickable `file:line:col` links; test results; compiler
  problems in the Problems tool window.
- **Navigation:** Go to File (fuzzy, `name:line`), Search Everywhere (double ⇧), Find Action,
  Recent Files, Recent Locations, Back/Forward, Last Edit Location, Go to Line:Column, F2 to the
  next error, Select In Project View.
- **Project:** an ignore-aware project tree, Find and Replace in Files, a Cargo tool window,
  atomic saves, save on focus loss and reload of changed files on focus (like RustRover).
- **Agent-ready:** every action is a command (`codecraft-cli commands`). The CLI drives the same
  engine headlessly: `codecraft-cli check <project>`, `codecraft-cli run <project> <command> '<json>'`.

RustRover's menus were observed from the running app (522 entries). **111 of 129 core menu
items** are live; see `docs/parity-checklist.md`.

## Get started

```sh
rustup component add rust-analyzer     # recommended: completion, navigation, diagnostics
cargo run --release -p codecraft -- path/to/your/project
```

Without rust-analyzer, editing, highlighting, Cargo build/run/test, search and navigation by
file all still work. The status bar shows why language features are off.

```sh
cargo run -p codecraft-cli -- check path/to/project        # cargo check through the engine
cargo run -p codecraft-cli -- commands                     # every command, with shortcuts
cargo run -p codecraft-ui-egui --example snapshot -- path/to/project out.png view.toolWindows.problems
```

## Under the hood

- **Engine first:** a pure-data workspace model and a command engine, with a thin egui UI on
  top. `cargo xtask layers` enforces the layering across 8 crates.
- **Never crash:** no `unwrap`/`expect`/`panic!` outside tests (denied by lints), no `unsafe`.
  A panic-hunt test fires every command with hostile params in every session state.
- **Nothing blocks the UI:** rust-analyzer, Cargo, project scans and Find in Files run on worker
  threads; the engine drains them once per frame.
- **Clean-room:** RustRover's behaviour and look are observed from the running app (Accessibility
  menu walk, bundle metadata); no JetBrains code or assets are used. See `docs/clean-room.md`.

## Download

Installers for macOS (Apple silicon), Windows and Linux (`.deb` and tarball) are attached to each
[GitHub release](https://github.com/Saimirbaci/codecraft/releases), with install notes and
checksums. Maintainers cut a release with Actions › New version (see `docs/release.md`).

## Documentation

`AGENTS.md` (start here) · `docs/architecture.md` · `docs/development.md` ·
`docs/roadmap.md` · `docs/gaps.md` · `docs/parity-checklist.md` · `docs/observation/`

## License

[MIT](LICENSE). RustRover is a trademark of JetBrains s.r.o.; CodeCraft is not affiliated
with or endorsed by JetBrains.
