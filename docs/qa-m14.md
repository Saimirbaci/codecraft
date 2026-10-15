# M14 QA: a hands-on pass as a person and as an agent

Track QA of M14 (2026-10-15), after the AX (agent control plane), DBG (debugging), CA (code
analysis) and PF (profiling) tracks landed. Goal: find what breaks or confuses a PyCharm, CLion,
RustRover or DataGrip user, or an AI agent, in daily work with debugging, analysis and profiling,
and fix the important parts. Everything here ran on this machine with the real tools: cargo,
clippy, rust-analyzer, lldb-dap (Xcode), lldb, xctrace, clang/clangd, CMake 4.3, python 3.10 with
debugpy 1.8.17 and pytest 8, ruff, SQLite (bundled).

## What was checked

**Look (Part 1).** `crates/ui-egui/tests/qa_gallery_ui.rs` builds a realistic project (a Rust
library + binary, a Python script and pytest tests, SQL scripts, a SQLite database, a profiler
snapshot) and paints, at 1280×800 and 900×700 in the dark and light themes: the main window, every
tool window (Terminal, Python Console, Python Packages and Rust REPL excluded: they start processes,
Python Packages asks PyPI, and they have their
own tests), Inspection Results and Problems with findings, the Profiler (flame graph, method list,
call tree), the Database tool window and a query grid in Services, an agent's approval card,
Inspect Code / Run Configurations / Breakpoints / Search Everywhere / Find in Files / Find Action,
six settings pages (Agent Access, Inspections, Python, Rust, Appearance, Keymap), a stopped
debugpy session with inline values, and a pytest run with a failure: 204 PNGs. Set
`CODECRAFT_QA_RENDER_DIR` to write them (they are not committed); every one was looked at.

**Use it like a person (Part 2).** The same commands the UI dispatches, on four sample projects
in temporary directories: Rust (`shop`: a library with an off-by-one bug and a failing test, a
binary calling a slow `fib`), C++ (CMake, `std::vector`/`std::map`, a null dereference), Python
(a package with an unused import, a slow function, pytest tests, a WSGI app) and SQL (a SQLite
file and a report script): open, diagnostics, run, run tests, debug (breakpoint, step, values,
evaluation, a crash), Inspect Code (project scope, fix one, suppress one, SARIF export), CPU and
memory profiles, the SQL script and queries.

**Use it like an agent (Part 3).** `apps/codecraft-cli/tests/agent_workflows.rs` spawns the built
`codecraft-cli mcp <project> --allow …` over stdio and drives what an agent asks for:
"why does test X fail" (Python and Rust: `test_run` → `test_results` → `debug_breakpoint_set` →
`debug_start` → `debug_wait_for_stop` → `debug_evaluate` → `debug_control`), "what is slow / what
allocates" (`profile_start` with a configuration or a command line, CPU and memory →
`profile_hotspots`), "fix the lint problems" (`inspect` → `problems` → `analysis.fix` /
`analysis.suppress` / `analysis.export` SARIF), "what's in table T" (`db_sources` → `db_query`),
and a C++ crash (`debug_start` → exception stop → `debug_stack`; `inspect` with the Clang Static
Analyzer). Each result was judged as an agent reads it: compact, unambiguous, absolute paths and
1-based lines, a next step, errors that say what to do. Set `CODECRAFT_QA_DUMP=1` to print every
call and answer.

## Findings and fixes

Severity: **High** stops the workflow or gives a wrong answer; **Medium** is confusing or costs
time; **Low** is polish.

| # | Sev | Where | Finding | Status |
|---|---|---|---|---|
| 1 | High | Profiler, Python | A cProfile profile of any program that imports a module (and every pytest run) showed one function, `<method 'disable' of '_lsprof.Profiler' objects>`, at 100 %: the program's top-level `exec` is called again by the import machinery, so no function looked like a root. | Fixed: the outermost cumulative time is the root when the caller-less functions cover less than half of it; tiny frames whose callees took long (`<string>` → `run_module`) are kept. `crates/profile/src/python.rs` |
| 2 | High | Run/Debug, Rust | Running or debugging the test at a line in `src/lib.rs` (gutter, Debug Context Configuration, agent `debug_start {file, line}`) built `--bin <name>` when the package also has `src/main.rs`: "running 0 tests". | Fixed: the target whose root the file is, then the deepest source directory, preferring the library. `run_cmds::target_for_file` |
| 3 | High | Profiler, Rust | Instruments (xctrace) leaves v0-mangled Rust names: hotspots, flame graph and method list showed `_RNvCsczPlFL8jVDe_4shop3fib`. | Fixed: a v0 and legacy demangler applied to every profile frame (`shop::fib`, `std::rt::lang_start::<()>::{closure#0}`). `crates/profile/src/demangle.rs` |
| 4 | High | Agents | Right after `codecraft-cli mcp` started, `run_configurations` answered `[]` and `project_info` had no configurations while Cargo metadata loaded; `debug_start`/`profile_start`/`test_run` raced the same way. | Fixed: those tools wait for the project load (2 min cap). |
| 5 | High | Agents | `profile_hotspots` right after `profile_start` of a Cargo or CMake configuration failed with "no profiler snapshot is open" (the target was still building). Profiles that ended out of order answered from an older snapshot; a failed profile reported nothing useful. | Fixed: waits while any profile builds, records or loads; answers for the newest profile; a failed one is an error naming its Run tab. |
| 6 | High | Run configurations, C++ | A CMake project listed "CMake: Build" and "Run inventory" twice, and "CMake: Build" was selected, so Run › Debug said it "can't be debugged" and Profile profiled `cmake --build`. | Fixed: the CMake model, read before the project scan finished, had already set the derived configurations and the load appended them again; they are now rebuilt from scratch on load, and a CMake project starts with its executable selected (like CLion). |
| 7 | High | UI | A bottom tool window first shown empty ("Nothing has run yet") shrank to its 80 px minimum (egui sizes a panel's frame by its contents) and every bottom tool window opened that small afterwards: the Services grid showed a header and no rows, the Debug window three variables. | Fixed: tool window panels claim their full size. Regression test. |
| 8 | High | Agents, SQL | A project with a SQLite file but no data source: `db_sources` said `[]`, `db_query` said "no data source `shop.db`; have: " (an empty list), and `db.addDataSource` needed `db-write`. | Fixed: `db_sources` lists the project's SQLite files ("not added"); `db_query` takes a SQLite path and adds it as a data source (needs `write`); adding/editing a data source needs `write`, connecting/refreshing/row counts/history are reads. |
| 9 | Medium | Agents, SQL | SQL the database refused (`no such table: nope`) came back as a success with empty results. | Fixed: a tool error with the database's message; partial failures carry `errors`. |
| 10 | Medium | Agents | `problems` right after `inspect` said "no problems are known for this scope". | Fixed: the open findings of the last analysis are included (`source: "analysis"`, `analyzer`, `id` for `analysis.fix`, `fixable`). |
| 11 | Medium | Agents | `profile_start` took only `config`: a project without configurations, a command line or a memory profile meant searching `codecraft_commands` for `profiler.record`. | Fixed: `command`, `kind`, `tool`, `seconds`; errors name the configurations there are or suggest `command`. |
| 12 | Medium | Agents, profiler | Memory (tracemalloc) and some CPU hotspots were all standard library / `<frozen importlib._bootstrap>` frames; the note spoke of "time" for byte counts. | Fixed: `inProject` on the project's functions and `projectHotspots` when library code fills the top; relative script paths become absolute; the memory note speaks of bytes. |
| 13 | Medium | Inspect Code, C++ | A project scope included CMake's build directory: clang-tidy and clangd analysed `cmake-build-debug/CMakeFiles/…/CMakeCXXCompilerId.cpp`. | Fixed: directory and project scopes leave out `cmake-build-*`, `CMakeFiles`, `node_modules`, `__pycache__`, virtual environments, `.git`. |
| 14 | Medium | Inspect Code, Rust | rust-analyzer's "code is inactive due to #[cfg] directives" (every `#[cfg(test)] mod tests`) was an Info problem in every Rust project. | Fixed: not a problem (the editor fades such code). |
| 15 | Medium | UI | In a 700 px high window the left stripe's top and bottom button groups overlapped. | Fixed: buttons shrink to fit (30 → 22 px), and a stripe that still doesn't fit scrolls. Regression test. |
| 16 | Medium | UI | At 900 px the Debug toolbar's status ("app.py Paused (breakpoint)") was drawn over the step buttons; the Profiler toolbar's Compare/Open/✕ and snapshot summary ran past the panel, and the method list's Total column was cut. | Fixed: a toolbar's right end moves to its own row when it doesn't fit, combo boxes and fields wrap before overflowing; the empty Debug window no longer says "Not debugging" twice. Regression tests. |
| 17 | Medium | UI | Settings › Tools › Agent Access at 900 px squeezed each capability's label and description to a word per line. | Fixed: one row per capability with the text taking the width the buttons leave. Regression test. |
| 18 | Medium | Agents, C++ | Frame names in stop summaries were 500-character libc++ signatures (`lookup(std::__1::map<std::__1::basic_string<…`). | Fixed: `std::__1::` → `std::`, long signatures → `lookup(…)`. |
| 19 | High | Tests | Running the gate (`cargo test --workspace`), the engine's panic hunt fuzzed `python.packages.install/uninstall/upgrade` with `{"package": "x", "name": "y"}`: it ran `pip install y` against the machine's Python (PyPI has no `y`, nothing was installed; checked) and listed outdated packages from PyPI (network). | Fixed: the panic hunt skips `python.packages.*` (their own tests use fakes); the gallery leaves the Python Packages window out. |
| 20 | Low | Python Console | Stopping the Python Console kills the process and then waits for it in the command, on the UI thread; in the panic hunt each stop took seconds. | Fixed (lead): the kill no longer waits on the UI thread; a reaper thread collects the exit. |
| 21 | Low | Inspect Code, Rust | With cargo-deny installed, the default profile runs `cargo deny check advisories`, which fetches the advisory database over the network. | Fixed (M15 EA): advisories run only when the profile turns Security advisories on (a checkbox that says it uses the network; off in the built-in profiles) or the tool is asked for by name; otherwise cargo deny runs `--offline` on licenses, bans and sources with a `deny.toml`. `cargo_audit.rs`, `analysis/profiles.rs` |
| 22 | Low | Debugger, Rust | Each Rust session's output has LLDB's "This version of LLDB has no plugin for the language rust" warning (values still read well through the formatters). | Fixed in M15 (track DM, gaps item 58): the line is dropped from Rust sessions' output; tested with the real lldb-dap. |
| 23 | Low | Debugger, C++ | A C++ reference local (`const Item& it`) shows its address, not the referenced value, until expanded. | Fixed in M15 (track DM, gaps item 58): references read `{id=7, name="apple"}` / `3` and still expand to the members; tested with the real lldb-dap. |
| 24 | Low | Agents | `test_run`'s `commandLine` includes the internal JUnit XML temp path; `inspect`'s `tools[]` uses `command_line` while the rest is camelCase. | Open (docs show the real shape). |
| 25 | Low | Profiler | `profile_hotspots` follows the Method List's sort in the IDE (sorting by Total there changes the agent's order). | Fixed (M15 PP): a fixed, documented order (`sort: self` by default or `total`, ties by name, location, module) over all threads and the whole run, whatever the IDE shows; `profiler.hotspots`, `profiler.report`, `profile_hotspots` and `profile_stop` take `sort` and say which order they used. Regression tests in `profiler_cmds` and `tests/agent_tools.rs`. |
| 26 | Low | Settings, Python | Languages & Frameworks › Python has no interpreter picker (the status bar and `python.interpreter.select` have one). | Fixed (M15 EA): the page has the interpreter picker (detected interpreters, project default, Detect, Create Virtualenv…, Add Interpreter by path), the language server and type checking mode, Ruff options and the formatter. `ui-egui/src/python_settings_ui.rs` |
| 27 | Low | Run, Python | A failed test's location in the Run tool window is the absolute path, which wraps in a narrow panel. | Fixed (M15 EA): relative to the project, the full path on hover; agent results stay absolute. `testing::display_location` |

Looked at and fine: the Python debugging flow (stop summary with excerpt and locals, inline values
in the editor, evaluation, errors for bad expressions); Rust and C++ stop summaries (`Vec`
`size=3`, `HashMap` `size=2`, `std::vector<Item>` `size=2`, `EXC_BAD_ACCESS (code=1,
address=0x0)` at `main.cpp:23` with `p = 0x0`); test failures with file, 1-based line, excerpt
and message for cargo and pytest; ruff fix/suppress/SARIF; the Clang Static Analyzer finding the
null dereference; the approval card; the access policy's denials naming the `--allow` flag; empty
states with hints in every tool window (Run, Debug, Find, Problems, Profiler, Database, Services,
Git, Commit, Coverage, Agent); the light theme's contrast.

## Tests added

- `apps/codecraft-cli/tests/agent_workflows.rs` (6): the agent workflows above over the built
  binary, skipping without python3/debugpy/pytest, ruff, cargo, lldb-dap, xctrace, cmake or clang.
- `crates/ui-egui/tests/qa_layout_ui.rs` (5): bottom tool window height, stripe overlap at 700 px,
  Profiler toolbar and method list at 900 px, Agent Access labels at 900 px, Debug status vs.
  toolbar at 900 px (with debugpy).
- `crates/engine/tests/qa_m14.rs` (2): a CMake project's configurations listed once with the
  executable selected; the gutter Run of a test in `src/lib.rs` beside `src/main.rs` runs `--lib`.
- `crates/ui-egui/tests/qa_gallery_ui.rs` (1): the gallery (a smoke test without
  `CODECRAFT_QA_RENDER_DIR`).
- Unit tests: cProfile roots under `exec` cycles (`profile::python`), the demangler
  (`profile::demangle`, including hostile input), `run_cmds::target_for_file`,
  `inspections::in_build_output`, rust-analyzer's inactive code, agent capability of data source
  commands and SQLite detection (`agent_tools`), short C++ frame names (`agent_tools_cmds`).

## What remains

The Low rows above, and from the walk-through: Clippy is not part of the default Rust inspection
(an agent asking for lint fixes gets `cargo check` unless it passes `tools: ["clippy"]`, as in
RustRover); xctrace needs about half a second before it samples, so short programs give a handful
of samples; the tracemalloc snapshot at exit is dominated by module objects (the project's own
allocations are in `projectHotspots`); the gallery doesn't cover the Terminal, Python Console and
Rust REPL, the Git/VCS windows with a real repository, or remote/Docker windows.
