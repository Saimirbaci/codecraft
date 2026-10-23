# Development

## Build and run

```sh
cargo run -p codecraft -- <file-or-project>          # desktop app
cargo run -p codecraft-cli -- check <project>          # headless cargo check through the engine
cargo run -p codecraft-cli -- run <project> <command-id> '<json params>'
cargo run -p codecraft-cli -- configs <project> add --name Api --kind python --command "main.py --port 80" --env-file .env
cargo run -p codecraft-cli -- configs <project> list    # also: update <i>, copy <i>, delete <i>, run <name|i>
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
cargo xtask bundle [--release] [--sign <identity>]   # target/bundle/CodeCraft.app, signed
cargo xtask dmg [--release] [--sign <identity>]      # target/bundle/CodeCraft-<version>.dmg (macOS, hdiutil), signed
cargo xtask package-deb [--release]                 # target/package/codecraft_<version>_<arch>.deb (Linux, dpkg-deb)
cargo xtask package-windows-installer [--release]   # zip + Inno Setup installer (Windows, ISCC)
cargo xtask version [patch|minor|major|<x.y.z>]     # print or bump the workspace version
./release.sh patch|minor|major [--dry-run]          # bump, commit, tag, push (no build; see docs/release.md)
cargo xtask notarize --profile <notarytool profile>  # submit the dmg, wait, staple, spctl
cargo xtask package-linux [--release]    # target/package/codecraft-<version>-linux-<arch>.tar.gz
cargo xtask package-windows [--release]  # target/package/codecraft-<version>-windows-<arch>.zip
```

`--layout-only` writes the package layout without building or signing; `--dry-run` prints every
command (build, codesign, hdiutil, notarytool, …) without running anything. Outputs go under the
Cargo target directory (`CARGO_TARGET_DIR` when set). Developer ID signing, notarization, the
entitlements and the CI/release workflows are described in [release.md](release.md).

- **macOS bundle.** `Contents/MacOS/codecraft-launcher` (background stable entry point),
  `Contents/Resources/factory/CodeCraft.app` (full foreground app), `Contents/Info.plist` (bundle id
  `dev.codecraft.CodeCraft`, version from the workspace, `LSMinimumSystemVersion` 11.0,
  `NSHighResolutionCapable`), `Contents/Resources/CodeCraft.icns` and `PkgInfo`. The icon is our
  own, drawn by `xtask/src/icon.rs` at every icon-set size and encoded as native ICNS in Rust. The
  bundle is signed inside-out with the hardened runtime and
  `apps/codecraft/macos/CodeCraft.entitlements`, ad hoc by default (`--timestamp=none`), or with
  `--sign <identity>` / `CODECRAFT_SIGN_IDENTITY` (a Developer ID, with a secure timestamp), then
  verified (`codesign --verify --deep --strict`, runtime flag checked). No document types are
  declared yet: the app takes its project from the command line or File › Open, not from Finder's
  open events.
- **dmg.** The signed bundle plus an `/Applications` link, compressed (UDZO), the image itself
  signed with the same identity and verified.
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

### Version payload development

`cargo xtask bundle --version-payload /tmp/codecraft-payload --layout-only` inspects a payload
layout; omit `--layout-only` to build and sign it ad hoc. The payload has no launcher and can be
passed to `codecraft_selfupdate::Store::stage` with version/source provenance. Never stage over
an existing version; promotion changes pointers atomically. Use an isolated `CODECRAFT_HOME`
and `CODECRAFT_FACTORY_DIR` for launcher development. `CODECRAFT_BOOT_TIMEOUT` and
`CODECRAFT_HEALTHY_AFTER` configure seconds (45 and 20 by default). App health is reported
from main-window UI progress, with the file write on a worker. Help › Versions & Rollback…
exposes commands whose disk work runs on engine workers. Rollback restores bounded configuration
snapshots; versions are selected for the next launch. See [m18-plan.md](m18-plan.md).
