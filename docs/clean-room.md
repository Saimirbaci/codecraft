# Clean-room policy

CodeCraft reimplements RustRover's *behaviour*, not its code. This page is the boundary.

## Why

RustRover is proprietary JetBrains software. Its license forbids decompiling or reverse
engineering it, and copying its code or assets would make CodeCraft a derivative work. The REA
toolkit we use for observation ([morluto/rea](https://github.com/morluto/rea)) draws the same line
in its `DISCLAIMER.md`: it does not support "copying proprietary software or reproducing its
protected code and assets". PhotoCraft, whose structure this project follows, is clean-room in the
same way.

## Allowed sources

| Source | Use |
|---|---|
| The running RustRover app | Observe menus, shortcuts, dialogs, tool windows, behaviour, layout. Screenshots for *our own reference* (not committed as assets). |
| macOS Accessibility (AX) API | Read the visible menu tree and shortcuts (`tools/observe/dump_menus.js`). |
| Bundle metadata | `Info.plist`, `product-info.json`, directory listing of bundled plugins: execution-free inventory, like REA's `inspect_artifact`. |
| JetBrains public documentation | Feature descriptions, keymap reference cards. |
| IntelliJ Community (Apache-2.0) | Behaviour reference for platform features; adapted code needs `ATTRIBUTION.md`. |
| intellij-rust (MIT, archived 2023) | Behaviour reference for Rust features; adapted code needs `ATTRIBUTION.md`. |
| rust-analyzer, rustc, Cargo (MIT/Apache-2.0) | Used as libraries/processes. |

## Not allowed

- Decompiling, disassembling or extracting classes or bytecode from RustRover's JARs or native
  binaries (no `javap`, no JADX/CFR/Fernflower, no Ghidra/Hopper on JetBrains binaries).
- Copying JetBrains icons, color-scheme XML, fonts shipped inside the bundle, inspection
  descriptions, templates or any other files from the bundle.
- Reading proprietary resource files out of the bundle's JARs (keymaps, action XML,
  plugin descriptors). Use the visible UI or the open-source equivalents instead.

## How observations are recorded

Raw observations live in `docs/observation/`, named `rustrover-<version>-<what>`, together with
the script that produced them. `cargo xtask catalog` turns the menu observation into
`crates/engine/src/catalog.rs` (menu paths, labels, shortcuts and our command ids).
