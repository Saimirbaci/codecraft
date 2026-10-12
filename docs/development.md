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

## Re-observing RustRover

With RustRover running:

```sh
osascript -l JavaScript tools/observe/dump_menus.js rustrover 4 > /tmp/menus.jsonl
python3 tools/observe/menus_outline.py /tmp/menus.jsonl json > docs/observation/rustrover-<version>-menus.json
cargo xtask catalog
```

Read `docs/clean-room.md` first. Remove anything personal from the capture (open project
names appear in the Window menu).
