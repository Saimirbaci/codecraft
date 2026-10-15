# Keymaps

Settings › Keymap offers five built-in keymaps and any number of user keymaps. Every keymap is
data in the engine (`crates/engine/src/keymaps.rs`, `crates/engine/src/settings/keymap_tables.rs`);
menus, tooltips, Find Action and dispatch all read the active one.

| Keymap | Built from | Parent |
|---|---|---|
| macOS | RustRover's macOS keymap, observed from the running app's menus (`docs/observation/`) | — |
| Windows/Linux | JetBrains' public RustRover keymap reference for Windows/Linux, plus `Cmd` → `Ctrl` | — |
| VS Code (macOS) | Microsoft's public "Keyboard shortcuts for macOS" reference for Visual Studio Code | macOS |
| VS Code | Microsoft's public "Keyboard shortcuts for Windows" / "… for Linux" references | Windows/Linux |
| Emacs | The GNU Emacs Reference Card (Meta = `Alt`) | Windows/Linux |

The VS Code and Emacs keymaps list only bindings for actions CodeCraft has (each id is checked
against the command registry by a test). Every other command keeps its parent keymap's shortcuts,
except those that clash with a key the derived keymap takes (the same keys, or the first stroke
of one of its chords: in Emacs `Ctrl+X` is a prefix, so the parent's Cut on `Ctrl+X` is dropped
and Cut is `Ctrl+W`). No JetBrains keymap files were read for any of them (see
`docs/clean-room.md`).

## Shortcut notation

`Shift+Cmd+F`, `Ctrl+Alt+L`, `F2`; `Shift Shift` for the double press; two keystrokes separated by
a space for a chord, e.g. `Cmd+K Cmd+C` (shown as `⌘K, ⌘C`). While the first stroke of a chord is
pending the status bar says so; the next key completes it (Escape cancels).

## User keymaps

Duplicate… makes a user keymap whose parent is the keymap shown; it inherits the parent's
shortcuts, the parent's own changes included, and keeps its changes under its name in
`keymap_overrides`. Rename… and Delete work on user keymaps only; deleting one re-parents the
keymaps based on it. Agents use `keymap.duplicate`, `keymap.rename`, `keymap.delete`,
`keymap.select`, `keymap.addShortcut` (chords accepted) and `keymap.list`.
