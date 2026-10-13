CodeCraft is a native Rust IDE with RustRover's menus, shortcuts and tool windows. Every action is a
command: find any of them with **Find Action** (⇧⌘A) or **Search Everywhere** (double ⇧).

## Getting around

- Open a project: File › Open… (a Cargo workspace or any directory). With a project already open,
  choose This Window or New Window; Window › Next Project Window switches between them.
- Go to File ⇧⌘O, Go to Symbol ⌥⌘O, Recent Files ⌘E, Back/Forward ⌘[ / ⌘].
- Quick Documentation F1, Parameter Info ⌘P, Show Context Actions ⌥↩, Rename ⇧F6.
- Run ⌃R, Debug ⌃D, Run Context Configuration ⌃⇧R, Stop ⌘F2.
- Tool windows: Project ⌘1, Problems ⌘6, Run ⌘4, Terminal ⌥F12, Git ⌘9.

## Language support

rust-analyzer provides completion, navigation, diagnostics and refactorings. Install it with
`rustup component add rust-analyzer`; CodeCraft finds it on PATH or through rustup. Without it,
editing, highlighting, Cargo build/run/test and search keep working.

## Settings and keymaps

CodeCraft › Settings… (⌘,) holds appearance, editor, keymap (macOS or Windows/Linux, with your own
bindings) and tool options. Settings live in `~/Library/Application Support/CodeCraft/state.json`
(`$XDG_CONFIG_HOME/codecraft` elsewhere).

## Agents and the command line

`codecraft-cli commands` lists every command with its parameters; `codecraft-cli run <project>
<command> '<json>'` drives the same engine headlessly.

## More

The project README and `docs/` (architecture, development, parity checklist) describe what works
and what is planned.
