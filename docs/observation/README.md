# Observations of RustRover 2026.1

Captured on 2026-10-12 from a locally installed, running copy (macOS, arm64). Method and limits
follow `docs/clean-room.md`: visible UI and bundle metadata only.

| File | What | How |
|---|---|---|
| `rustrover-2026.1-menus.json` | 732 menu-bar entries (lazily filled submenus added 2026-10-13): path, title, shortcut (macOS keymap), submenu flag | `osascript -l JavaScript tools/observe/dump_menus.js rustrover 4`, decoded by `python3 tools/observe/menus_outline.py <dump> json` |
| `rustrover-2026.1-menus.txt` | The same as an indented outline | `menus_outline.py <dump>` |
| `rustrover-2026.1-rea-inventory.md` | REA 6.4.0 execution-free artifact inventory and Apple application projection: formats, sizes, native helpers | `rea inspect-artifact`, `rea project-apple-application-graph` |
| `rustrover-2026.1-bundled-plugins.txt` | Names of the plugin directories in the app bundle | `ls RustRover.app/Contents/plugins` |

## Bundle facts

From `Info.plist` and `Contents/Resources/product-info.json`:

- RustRover 2026.1, build RR-261.22158.331, bundle id `com.jetbrains.rustrover`, arm64.
- JVM application on the bundled JetBrains Runtime (Java 21+); an IntelliJ Platform product with
  Fleet kernel/RPC jars on the boot class path, LSP4J (LSP client), pty4j (terminal), skiko (Skia).
- Ships its own `lldb` and `gdb` (debugger backends), `fsnotifier` (file watching), a profiler, and
  `remote-dev-server`.
- URL scheme `rustrover://` (stack-trace navigation); opens all document types as an editor.
- 3.2 GB installed. Rust support is the `intellij-rust` plugin; native debugging is
  `nativeDebug-plugin` + `cidr-parallelStacks`; TOML, Markdown, YAML, JSON, Shell, Docker,
  Git/GitHub/GitLab, terminal, HTTP client, database tools and the web stack are bundled plugins.

## Lazily filled submenus

The IDE fills some submenus only when they open, so the first Accessibility walk saw them empty.
On 2026-10-13 they were captured by opening each one (`osascript -l JavaScript
tools/observe/dump_lazy_menus.js rustrover '<JSON list of paths>'`, which brings RustRover to the
front, opens each submenu, reads it, closes it with Escape and restores the previous front app),
adding 211 entries: Manage IDE Settings, Paste, Find Usages, Macros, Bookmarks, Appearance,
Analyze Code, Folding, Profile, Debugging Actions, Tasks & Contexts, Services, Markdown,
Tools › Rust, GitHub Copilot, Layouts, Active Tool Window, Notifications, Diagnostic Tools.
Still empty: File › Recent Projects (personal paths, skipped on purpose), View › Active Editor
and Git › Current File (they need an open editor in RustRover), Run › Test History and
Window › Editor Tabs › Split with Chooser Navigation (empty when observed). The shell's
`menus::EXTRA` table places CodeCraft's commands for those (and the bookmark toggles RustRover
adds for the caret's line); commands that predate the capture keep their old ids as aliases of
the catalog ids (`crates/engine/src/commands.rs`, `ALIASES`).

The captured menus include items contributed by third-party plugins installed in this copy
(GitHub Copilot, Python, Zig, Junie, AI Assistant, Educational). The catalog marks those
out of scope.
