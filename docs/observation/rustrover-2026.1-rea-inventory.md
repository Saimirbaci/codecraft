# REA inventory of RustRover 2026.1

Captured 2026-10-12 with REA 6.4.0 (`rea-agents`), execution-free: the artifact is read, never
run. Clean-room note: this records *what* ships (formats, sizes, executable names), not any file
contents.

```sh
rea inspect-artifact ~/Applications/RustRover.app --format json --integrity-policy record-and-continue > inspect.json
# then wrap substeps[0].evidence as {"inventory_evidence": [<evidence>]} into inventory.json:
rea project-apple-application-graph inventory.json --format json > apple-graph.json
```

Raw evidence (17 MB + 3 MB JSON with local paths) is not committed; regenerate it with the
commands above.

## Shape

- Artifact graph: 12,019 unique artifacts, 15,462 occurrences, 15,295 edges; inventory complete.
- Formats: 12,667 files, 1,331 directories, 941 zip/jar archives, 265 Mach-O + 8 universal
  binaries, 210 JavaScript bundles, 19 PE and 8 ELF binaries (remote-dev and cross-platform
  helpers), 13 property lists.
- Apple projection: 14 bundles (macOS deep layout), 273 executables, 3,165 framework files,
  218 native libraries, 15 code-signing records, no privileged helpers or launchd plists.
  Runtime families: JVM application (not detected as a family by REA) plus JavaScript.

## Where the bytes are

| Area | Size | What it is |
|---|---|---|
| `Contents/jbr` | 498 MB | JetBrains Runtime (Java) |
| `plugins/fullLine` | 278 MB | local full-line code completion model |
| `bin/gdb` | 248 MB | bundled GDB (+ Python) |
| `bin/lldb` | 176 MB | bundled LLDB.framework, debugserver, `LLDBFrontend` |
| `plugins/DatabaseTools` | 150 MB | database tools (DataGrip) |
| `plugins/intellij-rust` | 133 MB | the Rust plugin |
| `plugins/restClient` | 122 MB | HTTP client |
| `plugins/terminal` | 97 MB | terminal |
| `plugins/cwm-plugin` | 83 MB | Code With Me |
| `plugins/javascript-plugin` | 81 MB | JavaScript/TypeScript |

## Native helpers worth knowing (behaviour, for CodeCraft's design)

- **Debugger:** `LLDB.framework`, `debugserver` and a separate `LLDBFrontend` process per
  platform, plus a bundled `gdb`. RustRover drives LLDB through its own frontend process, not
  DAP. CodeCraft uses `lldb-dap` (same split: IDE ↔ frontend process ↔ LLDB) with an open
  protocol.
- **Terminal:** `pty4j-unix-spawn-helper`: a PTY-based terminal (CodeCraft's terminal milestone
  uses a PTY too).
- **File watching:** `fsnotifier`, a dedicated native watcher process (CodeCraft gap #14).
- **Rust plugin:** `intellij-rust-native-helper`.
- **Remote execution:** `ijent-{aarch64,x86_64}-unknown-linux-musl-release`, Rust-built agents for
  remote/WSL/container targets; `remote-dev-server` and `remote-dev-worker-*` for remote
  development.
- **Storage:** `libsqliteij` (SQLite for IDE caches); `async-profiler` for the profiler.

## REA's suggested next probe

`analyze_javascript_application` for the 210 JS bundles (JS/Node/Angular/Vue plugin services).
Not pursued: they belong to the web stack, which CodeCraft serves through language servers.
