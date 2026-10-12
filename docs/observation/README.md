# Observations of RustRover 2026.1

Captured on 2026-10-12 from a locally installed, running copy (macOS, arm64). Method and limits
follow `docs/clean-room.md`: visible UI and bundle metadata only.

| File | What | How |
|---|---|---|
| `rustrover-2026.1-menus.json` | 522 menu-bar entries: path, title, shortcut (macOS keymap), submenu flag | `osascript -l JavaScript tools/observe/dump_menus.js rustrover 4`, decoded by `python3 tools/observe/menus_outline.py <dump> json` |
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

## Gaps in the menu capture

The IDE fills some submenus lazily when they are opened, so the Accessibility walk saw them empty:
File › Recent Projects, Manage IDE Settings; Edit › Paste, Find Usages, Macros, Bookmarks;
View › Appearance, Active Editor; Code › Analyze Code, Folding; Run › Profile, Debugging Actions,
Test History; Tools › Rust, Tasks & Contexts, Services, Markdown, GitHub Copilot; Git › Current File;
Window › Layouts, Active Tool Window, Notifications. Capture them by opening each menu, then
re-running the dump (to do).

The captured menus include items contributed by third-party plugins installed in this copy
(GitHub Copilot, Python, Zig, Junie, AI Assistant, Educational). The catalog marks those
out of scope.
