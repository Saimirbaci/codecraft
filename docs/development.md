# Development

## Build and run

```sh
cargo run -p codecraft -- <file-or-project>          # desktop app
cargo run -p codecraft-cli -- check <project>          # headless cargo check through the engine
cargo run -p codecraft-cli -- run <project> <command-id> '<json params>'
```

The app stores settings and recent projects in `~/Library/Application Support/CodeCraft/state.json`
(`$XDG_CONFIG_HOME/codecraft` elsewhere). `CODECRAFT_CONFIG_DIR` overrides it.

## Test

```sh
cargo test --workspace
cargo test -p codecraft-engine --test panic_hunt      # every command × hostile params
cargo test -p codecraft-ui-egui --test ui_smoke       # real key events through the app shell
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask layers
cargo xtask parity                                    # regenerate docs/parity-checklist.md
```

Tests that need `rust-analyzer` skip themselves when it is not installed. Cargo integration tests
build tiny offline projects in temp dirs.

## See the UI without a window

```sh
cargo run -p codecraft-ui-egui --example snapshot -- <file-or-project> out.png [command-id[=json] ...]
```

The snapshot renders offscreen through egui_kittest + wgpu, after running the given commands, e.g.
`cargo.check 'editor.setSelection={"anchor":30}' run.contextConfiguration`. Look at the PNG
after UI changes.

## Packaging

```sh
cargo xtask bundle [--release]           # target/bundle/CodeCraft.app, signed
cargo xtask dmg [--release]              # target/bundle/CodeCraft-<version>.dmg (macOS, hdiutil)
cargo xtask package-linux [--release]    # target/package/codecraft-<version>-linux-<arch>.tar.gz
cargo xtask package-windows [--release]  # target/package/codecraft-<version>-windows-<arch>.zip
```

`--layout-only` writes the package layout without building or signing. Outputs go under the Cargo
target directory (`CARGO_TARGET_DIR` when set).

- **macOS bundle.** `Contents/MacOS/codecraft`, `Contents/Info.plist` (bundle id
  `dev.codecraft.CodeCraft`, version from the workspace, `LSMinimumSystemVersion` 11.0,
  `NSHighResolutionCapable`), `Contents/Resources/CodeCraft.icns` and `PkgInfo`. The icon is our
  own, drawn by `xtask/src/icon.rs` at every icon-set size and converted with `iconutil`. The
  bundle is signed ad hoc (`codesign --force --deep -s -`) and verified; set
  `CODECRAFT_SIGN_IDENTITY` to sign with a Developer ID (adds the hardened runtime and a
  timestamp; notarize separately with `xcrun notarytool`). No document types are declared yet: the
  app takes its project from the command line or File › Open, not from Finder's open events.
- **dmg.** The signed bundle plus an `/Applications` link, compressed (UDZO).
- **Linux.** On Linux: `bin/codecraft`, `bin/codecraft-cli`,
  `share/applications/dev.codecraft.CodeCraft.desktop`, `share/icons/hicolor/<size>/apps/*.png`,
  README and licenses, as a tarball (install with `cp -r bin share ~/.local/`). Elsewhere the task
  writes the layout only (no cross-compiling).
- **Windows.** On Windows: `codecraft.exe`, `codecraft-cli.exe`, `codecraft.png`, README and
  licenses, zipped with the `tar` that ships with Windows 10+. Not exercised on Windows yet;
  elsewhere it writes the layout only.

## macOS menu bar and project windows

On macOS the menus live in the global menu bar (`crates/ui-egui/src/native_menu.rs`, through
`muda`), built from the same catalog and `EXTRA` table as the in-window bar; Settings › Appearance
can switch back to the in-window bar (`native_menu_bar`). Chords (Cmd/Ctrl, function keys,
Alt with a non-character key) are native key equivalents; a key equivalent that fires is fed back
to egui as the key it swallowed, so editor, text-field and terminal routing stay the same. Plain
and Alt+character shortcuts stay egui-only (they would eat typing). Tests and snapshots never touch
`NSApp`: only `codecraft_ui_egui::run` enables the native bar.

File › Open with a project open asks This Window / New Window; a new window is an egui immediate
viewport with its own `Session` (`project_windows.rs`), settings are shared between windows, and
Window › Next/Previous Project Window cycle the focus. Without native viewports (kittest), project
windows keep running but are not painted.

## Re-observing RustRover

With RustRover running:

```sh
osascript -l JavaScript tools/observe/dump_menus.js rustrover 4 > /tmp/menus.jsonl
python3 tools/observe/menus_outline.py /tmp/menus.jsonl json > docs/observation/rustrover-<version>-menus.json
cargo xtask catalog
```

Read `docs/clean-room.md` first. Remove anything personal from the capture (open project
names appear in the Window menu).
