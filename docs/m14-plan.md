# M14: debug, analyze and profile, for people and agents

Status: in progress (2026-10-15). Goal: CodeCraft replaces PyCharm, CLion and RustRover (plus
DataGrip, M12/M13) for daily work. Not every feature of those IDEs matters; what must be
first-class, for **both humans and AI agents**, is:

- **Debugging** Rust, C/C++ and Python: launch, attach, tests, breakpoints of every kind,
  stepping, variables that read well, evaluation, remote and core dumps.
- **Code analysis**: project-wide problems from the right analyzers per language, with scopes,
  profiles, quick fixes, suppressions and machine-readable results.
- **Profiling**: CPU and memory for native code and Python, with views a person can read and
  summaries an agent can act on.

"For agents" means: every capability is a command with JSON params and a JSON result that is
compact and self-explanatory (file paths, 1-based lines, short source excerpts, stable ids), and
agents can reach it while the IDE is open (the IDE's MCP server) and without it (a headless MCP
server over stdio, and `codecraft-cli` subcommands). Same rules as M10–M13: read `AGENTS.md` and
`docs/clean-room.md` (behaviour from public documentation and the running apps only).

Tracks work in their own git worktrees; the lead merges them. Disk is tight: at most four tracks
build at once, each with `CARGO_TARGET_DIR=<repo>/target-<track>`, `CARGO_INCREMENTAL=0`,
`CARGO_BUILD_JOBS=3`.

Tools on this machine (others are detected at run time; tests skip cleanly without them):
lldb-dap (Xcode), lldb, debugpy 1.8, clangd (Xcode), rust-analyzer, ruff, uv, node, xctrace.
Not installed: samply, py-spy, memray, basedpyright/pyright, mypy, clang-tidy, cppcheck, gdb,
valgrind.

## Wave 1

| Track | Scope | Owns |
|---|---|---|
| AX Agent control plane | MCP tools for the whole engine: `codecraft_commands` (search the registry) and `codecraft_execute` (any command by id) plus curated tools for debugging (start a configuration/test/attach, breakpoints with conditions, continue/step/pause, `wait_for_stop` with a timeout returning a stop summary: reason, thread, top frames with source excerpts, locals), analysis (problems for a scope, inspect, find usages, definition, hover/type, symbols, call hierarchy), profiling (record, stop, hotspots), tests (run, results), run configurations and database queries (read-only unless allowed); served by the IDE's MCP server (Claude Code connected to the IDE gets them) and by a headless `codecraft-cli mcp <project>` stdio server with a long-lived Session; an access policy (read-only / ask / allow per capability: run, debug, profile, write files, database writes) in Settings › Tools › Agent Access, with an approval prompt in the IDE and `--allow` flags headless; built-in agent jobs get the CodeCraft MCP server automatically; `codecraft-cli debug|analyze|profile|test` one-shot subcommands; `docs/agents.md` | `crates/mcp/src/{tools,engine_tools}.rs`, `engine/src/agent_access.rs`, `engine/src/ide_server.rs` (surgical), `apps/codecraft-cli`, `docs/agents.md` |
| DBG Debugging depth | Verified end to end with the real lldb-dap and debugpy here (tests skip without them): breakpoint conditions, hit counts, logpoints, temporary breakpoints, exception breakpoints per language (Rust panics, C++ throw/catch, Python raised/uncaught/user-uncaught); Python: debug pytest tests and Django/Flask/FastAPI configurations, subprocesses, justMyCode toggle, attach to a process verified, remote attach (debugpy listen host:port); native: remote debugging (lldb-server/gdbserver `gdb-remote`), core dumps (Run › Open Core Dump), Run to Cursor, Jump to Line/Set Next Statement, Set Value, step-into targets; inline variable values in the editor while stopped; return values after step out; Rust (HashMap, Option, enums, Rc/Arc, slices) and C++ libc++ STL values read well; debug console completion | `engine/src/debug*`, `crates/dap`, `ui-egui` debugger views |
| CA Code analysis | Inspect Code with scopes (file, directory, changed files, project, custom glob), inspection profiles with per-rule severity and on/off, a results tool window (by severity/analyzer/file), Fix All of a kind, suppressions (`#[allow]`, `# noqa`, `// NOLINT`, `-- noinspection`), baselines (only new problems), JSON/SARIF/HTML export, "Analyze code" commit check; analyzers: Rust (clippy with lint groups, rust-analyzer diagnostics project-wide), Python (ruff, basedpyright/pyright or mypy when present, managed install of basedpyright into `$CODECRAFT_HOME/tools` via uv on request), C/C++ (clang-tidy when present, `clang --analyze` static analyzer from the toolchain, clangd diagnostics project-wide, `compile_commands.json` from CMake), SQL inspections; the Problems tool window's project tab | `engine/src/analysis/*`, `engine/src/inspections*` (surgical), `ui-egui/src/inspect_ui.rs` |
| PF Profiling | Native CPU: xctrace Time Profiler on macOS (no install), samply when present, perf on Linux, with symbolication; Python CPU: py-spy when present (managed install via uv on request), cProfile always; memory: Python tracemalloc/memray, native allocations via xctrace Allocations on macOS and heaptrack/massif on Linux when present; views: flame graph, icicle, call tree, method list (self/total), callers/callees, per-line samples in the editor gutter, threads timeline, snapshot comparison (diff flame graph); profile tests and benchmarks (`cargo bench`, pytest); an agent report: top functions with self/total %, file:line and the hottest lines | `engine/src/profiler*`, `crates/profile`, `ui-egui` profiler views |

## Wave 2

| Track | Scope |
|---|---|
| QA Hands-on pass | Render every tool window and main dialog headlessly to PNG (egui_kittest with wgpu where available) at 1280×800 and 900×700 in dark and light, review them, and fix layout bugs (clipped toolbars, overlapping text, unreadable colours); walk the daily flows of PyCharm, CLion, RustRover and DataGrip users on sample projects with the real tools here (Rust, C++ with CMake, Python with pytest/Django-like app, SQLite/PostgreSQL), as a person through the UI and as an agent through the MCP tools; fix what breaks; `docs/qa-m14.md` with findings |

## Rules

Never panic, no unsafe, nothing blocks the UI thread, every action is a command, tests are the
gate. Tests never touch the real `~/.codecraft`, `~/.claude`, Keychain, `~/.ssh`, Docker, GitHub
or databases other than ones they start; never run a real agent CLI with a prompt, never the
network, never `sudo`. Nothing is installed globally; tools can be tried manually in a throwaway
`uv venv` inside the track's target dir. No `cargo update`. Track agents don't commit; the lead
merges.
