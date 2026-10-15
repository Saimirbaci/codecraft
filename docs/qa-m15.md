# M15 QA: the second hands-on pass, as a person and as an agent

Track QB of M15 (2026-10-15), after the EA (analysis in the editor), DM (several debug sessions),
PP (profiling polish) and AI (agents everywhere) tracks landed. Same method as `docs/qa-m14.md`:
look at every window, walk the new features on sample projects with the real tools, drive the
built MCP server the way an agent does, and fix what breaks or confuses. Tools on this machine:
cargo/clippy, rust-analyzer, lldb-dap (Xcode), xctrace, clang/clangd, CMake 4.3 with Homebrew's
GoogleTest, python 3.10 with debugpy and pytest, ruff, git, SQLite (bundled) and a local
PostgreSQL (throwaway schema). Not installed: evcxr, samply, py-spy, basedpyright, js-debug.

## What was checked

**Look (Part 1).** `crates/ui-egui/tests/qa_gallery_m15_ui.rs` paints what the M14 gallery left
out, at 1280×800 and 900×700 in both themes: the Terminal (two tabs, colours), the Python Console
(results, a traceback, the Variables view), the Rust REPL (a stand-in for evcxr, which is not
installed here), a real temporary Git repository (the log with branches and a tag, the Commit
tool window with two changelists and a diff preview, the shelf, a real merge conflict and the
merge editor), Deployment › Configuration, Remote Host and File Transfer over a fake `ssh`,
Services › Docker over a fake `docker`, and the M15 additions: Ruff's underline after a save and
the Alt+Enter popup on it, Settings › Languages & Frameworks › Python, two paused debugpy
sessions with their tabs, the profiler's threads timeline and Method List with a time range
selected, the Memory view of a real tracemalloc run, Connect an External Agent: 72 PNGs (18 states);
every state was looked at, most at both sizes and in both themes, and again after the fixes.
Both galleries now also report every glyph drawn as an empty box
(`tests/support/glyphs.rs`), which found the font problems below in the M14 states too. Set
`CODECRAFT_QA_RENDER_DIR` to write the PNGs (inside the target dir; never committed).

**Use it like a person (Part 2).** `crates/engine/tests/qa_m15.rs`, the commands the UI
dispatches on sample projects in temp dirs: a Python file with an unused import opened, edited
and saved (Ruff), Alt+Enter's fix, Inspect Code's findings in another file's editor; a Git
repository with a changelist and a breakpoint; a TCP server and its client under debugpy at once
(the server stops while handling the request, the client waits; switch, continue, the client
stops with the answer); the GoogleTest test at the caret profiled with xctrace through its CMake
target, then the first third of the run as a time range; a Python program's memory peak with
tracemalloc; `db_query` paging on SQLite.

**Use it like an agent (Part 3).** `apps/codecraft-cli/tests/agent_playbook.rs` runs the five
playbooks of `docs/agents.md` call by call, as written, over the built `codecraft-cli mcp
<project> --allow …` (stdio), on projects shaped like the docs' examples (the `shop` crate with
`parse` and `tests::parses_empty`, a Python `main.py` with `slow_sum`, a Git repository with an
uncommitted needless `return` and unused import, a PostgreSQL `orders` table in a throwaway
schema, a `feature/parser` branch), checking that each documented call is accepted and each
documented result field is there; plus a client and a server debugged with `session` (by id and
by name), `notifications/cancelled` of a `debug_wait_for_stop` that would wait a minute, and
every `--print-config` output against the docs. Set `CODECRAFT_QA_DUMP=1` to print every call.

## Findings and fixes

Severity: **High** stops the workflow or gives a wrong answer; **Medium** is confusing or costs
time; **Low** is polish.

| # | Sev | Where | Finding | Status |
|---|---|---|---|---|
| 1 | High | Profiler, C++ | Profiling a C++ test at the caret (GoogleTest, Catch2, doctest, Boost.Test through CMake), and Run › Profile of the C++ test configurations, launched `exec` itself: the build-then-run wrapper `sh -c '<build> && exec "$@"' arg0 <exe> <args>` was split on its last step. xctrace: "Path not found 'exec'", then "Export failed: Unexpected internal error". | Fixed: the program and its filter come from the wrapper's positional arguments. `profiler_plan::plan_process`; unit test; real GoogleTest + xctrace in `qa_m15.rs` (only `Perf.Phases` runs; a time range over its first third shows `spin`, not `churn`). |
| 2 | High | Agents | Right after `codecraft-cli mcp` started, `inspect {scope: "uncommitted"}` said "the project is not under Git" and `codecraft_execute vcs.status` "not a Git repository" (the repository was still being found). | Fixed: they wait until the repository's status has been read (`VcsState::settling_for`, 2 min cap). |
| 3 | High | Agents | The Review playbook's `codecraft_execute vcs.status` was denied as a `write`; so were `analysis.fileFindings` (documented for agents) and `python.languageServer`, while `debug.sessions` and `profiler.report`/`memory`/`lineHits`/`compare`/`job` needed `debug`/`profile`. | Fixed: they are reads; `analysis.analyzeFile` is `run`. `agent_tools::documented_queries_are_reads`. |
| 4 | Medium | Agents | `db_query` with `limit: 20` on 30 rows answered 20 rows and `"hasMore": false` (it meant "the server has more than were fetched"); an agent believed it had the whole table. | Fixed: `hasMore` is true when rows are left out, with a `note` ("20 of 30 rows; …"). `qa_m15::db_query_says_when_rows_are_left_out`, and PostgreSQL in the playbook. |
| 5 | Medium | Agents | `test_run`'s `commandLine` was `cargo test --message-format=json-diagnostic-rendered-ansi -- parses_empty` (run as written, it prints JSON). | Fixed: CodeCraft's own message format is left out (`cargo test -- parses_empty`); a user's own `--message-format` stays. |
| 6 | Medium | Agents, Rust | The playbook's `debug_evaluate {"expression": "input.is_empty()"}` fails: LLDB evaluates C-like expressions over Rust values, not method calls ("no member named 'is_empty' in '&str'"). | Fixed: the docs use `input.length == 0` and say what works (`s.length`, `v.len`, `v[0]`, fields); the error in a Rust session adds that hint. |
| 7 | Medium | Docs | The playbook's results differed from the real ones: the Python hotspot example showed `slow_sum` at 81 % self time and a hot line (cProfile has no lines; the builtin `sum` it calls has the self time); `debug_control stop` answers `exited`, not `none`; `inspect`'s scope is "Uncommitted files (2)"; a test's `suite` is "shop: src/lib.rs"; a breakpoint is `verified` only once a session set it. | Fixed in `docs/agents.md` (and what cProfile can and can't tell, which reads are reads, `limit`/`hasMore`). Every playbook now runs as written in `agent_playbook.rs`. |
| 8 | Medium | Editor, Python | A Python file opened with a lint problem showed no underline until it was edited and saved; Cmd+S on an unmodified file saves nothing ("All files are saved"), so there was no way to see Ruff's findings short of an edit or Inspect Code (PyCharm shows them on open). | Fixed: a Python file without findings is analyzed with Ruff when it opens (same setting as on save; Rust and C/C++ still wait for a save). `qa_m15.rs` (real ruff; a stand-in counts runs). |
| 9 | Medium | Git | The first changelist, shelf, breakpoint or bookmark put `.codecraft/changelists.json`, `breakpoints.json`, … into the Commit tool window as unversioned files to commit. | Fixed: like IntelliJ's `.idea/.gitignore`, `.codecraft/.gitignore` is written (once a session) keeping the per-user state out; shared settings (run configurations, data sources, inspection profiles) stay versionable. `qa_m15::codecraft_state_stays_out_of_the_commit`. |
| 10 | Medium | UI | Glyphs drawn as empty boxes on macOS: the Database, Hierarchy and Rust REPL stripe icons, Agents' ⇄ (the Connect an External Agent button looked empty), Reset Frame's ⤺, the ✕ close buttons (Profiler, Inspection Results, refactoring preview, run configurations, database settings), ↶/↵ in the database windows, ◆ in Search Everywhere, ⊘ for ignored tests. | Fixed: the platform's monospace and symbol fonts (Apple Symbols, DejaVu Sans, Segoe UI Symbol) are fallbacks of the UI fonts; ✕ → ✖, 🛢 → 🗄, 🦀 → "Rs" (no font here has them). `qa_layout_m15_ui.rs` asserts no empty boxes on macOS; the galleries report them. |
| 11 | Medium | UI | The Python Console's Variables panel grew to its longest value: at 900 px the transcript got a few words per line. | Fixed: at most half the window, long values cut (the whole value in the tooltip). Regression test. |
| 12 | Medium | UI | Connect an External Agent from Find Action (or a menu) ran the command and showed nothing: only the ⇄ button opened the window. | Fixed: the command's result opens the window wherever it was run from (`AgentsState::connect_shown`). Regression test. |
| 13 | Medium | Profiler | The Memory view's allocations were one "size · count · function · path" line each under a four-column header that didn't line up, with absolute paths. | Fixed: name and location (relative to the project, full path in the tooltip) on the left, Size and Count in columns. Regression test. |
| 14 | Low | Agents, Python | Every Python stop summary's `locals` started with debugpy's `special variables` group (`"value": ""`). | Fixed: debugpy's grouping nodes are left out of `locals` (still in `debug_variables`). |
| 15 | Low | Tests | `Session::isolate_home` kept tests from the GitHub CLI but not from the user's Docker: a UI test showing Services › Docker would have listed the real daemon's containers. | Fixed: isolated sessions get a missing `docker` (tests point it at a fake). |

Looked at and fine: the Terminal (ANSI colours, tabs, the plain `/bin/sh` of isolated sessions),
the Python Console transcript and errors, the Rust REPL window, the Git log graph with branch and
tag labels, commit details, changelists with their own Include boxes, the diff preview, the shelf,
the conflict entry and the merge editor at both sizes, Deployment › Configuration, Remote Host,
File Transfer, Services › Docker, Settings › Python at both sizes, Ruff's underline, the Error
Description and the Alt+Enter popup (Ruff's fix first, then the suppression), session tabs and
the status bar's "2 debug sessions (2 paused)", the time-range chip and dimmed timeline, the
Method List of a range, Connect an External Agent. As a person: the TCP client/server pair
(the stopped server comes forward, the waiting client is `running`, switching tabs, the client
stops with `'PING'`), tracemalloc's peak at `build`'s line with 98 % of the peak, Inspect Code's
findings in the editor. As an agent: the Debug-a-failing-test, Find-a-hotspot, Fix-lint,
Inspect-a-table (PostgreSQL: column types, counts, paging, `EXPLAIN` as a read, `EXPLAIN ANALYZE`
and `DELETE` denied without `db-write`) and Review-a-branch playbooks; two sessions by id and by
name; cancellation (nothing answered for the cancelled call, the server goes on, the program
stops); `--print-config` for Claude Code, Codex, Gemini, Cursor, Grok and JSON exactly as the docs
print them.

## Tests added

- `crates/ui-egui/tests/qa_gallery_m15_ui.rs` (4): the second gallery (smoke tests without
  `CODECRAFT_QA_RENDER_DIR`; skips without git, python3, debugpy, ruff).
- `crates/ui-egui/tests/qa_layout_m15_ui.rs` (4): no empty-box glyphs in the stripes, the Agent,
  Debug and Profiler windows and Search Everywhere; the Console's Variables at 900 px; the Memory
  view's columns and relative locations; Connect an External Agent from a command.
- `crates/ui-egui/tests/support/glyphs.rs`: the empty-box detector (shared by the galleries).
- `crates/engine/tests/qa_m15.rs` (8): Ruff on open and save with Alt+Enter and Inspect Code;
  on-open only for Python and only with the setting; `.codecraft/.gitignore`; a debugpy client
  and server; a GoogleTest test profiled at the caret with a time range; tracemalloc's peak;
  isolated sessions and Docker; `db_query` paging.
- `apps/codecraft-cli/tests/agent_playbook.rs` (8): the five playbooks, two sessions, cancelling
  a waiting call, `--print-config` against the docs.
- Unit tests: the build-then-run wrapper in `profiler_plan`, CodeCraft's Cargo message format in
  `public_command_line`, the documented reads in `agent_tools`, debugpy's grouping nodes.

## What remains

- evcxr isn't installed: the Rust REPL was painted with a stand-in. samply, py-spy, js-debug and
  basedpyright aren't either (as for M15's tracks).
- Analysis on open is Python only; Rust and C/C++ files show the analyzers' findings after a save
  (their language servers report as you type).
- A C++ test profiled at the caret shows the test report flag (`--gtest_output=json:…`) on its
  build step's command line (the step is instrumented like the test run; harmless).
- `.codecraft/.gitignore` itself is listed as a new file to commit (as `.idea/.gitignore` is).
- Search Everywhere / Go to File list CodeCraft's own files (`.codecraft/consoles/…/console.sql`,
  `.codecraft/datasources.json`) among the project's, sometimes first; the project scan leaves
  out `.idea` but not `.codecraft` (changing that touches the Project view, Find in Files and
  the watchers, so it was left for a decision).
- Low polish seen in the PNGs: the Git log graph's lines have gaps between rows; the timeline's
  range marker is drawn over the axis label it starts at; a session tab's close × sits tight
  against the name; "1 threads"; the Memory chart of a short program has two samples and an empty
  start (the first sample is ~17 ms in); Cmd+F1's description shows `warning[F401]` where the
  hover says "Ruff F401"; Remote Host's folder icon is the emoji font's outline; profile results
  spell the snapshot path `/var/folders/…` where other paths are `/private/var/folders/…`.
- `xctrace` left one function of the GoogleTest profile (`spin`) without a line.
- Not exercised for real: a Docker daemon and remote hosts (fakes), GitHub (a fake `gh` in the
  existing tests).
