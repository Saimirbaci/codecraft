# CodeCraft for AI agents

CodeCraft exposes its engine (the debugger, code analysis, the profiler, the test runner, run
configurations, databases, files with unsaved editor text) to AI agents as MCP tools. An agent
can debug a Rust, C/C++ or Python program, find usages, run inspections, profile a
configuration or query a database with the same machinery the IDE's windows use, while the IDE is
open or without it.

Every tool is built on registered commands (see `codecraft_commands`), returns compact JSON with
absolute paths, **1-based lines and columns**, short source excerpts and stable ids, and cuts long
output with a `truncated` note that says how to page. What an agent may do is decided per call by
the [access policy](#access-policy).

## Connecting

### While the IDE is open

The IDE runs an MCP server for the open project (the one Claude Code's `/ide` connects to; the
lock file is `~/.claude/ide/<port>.lock`). It advertises Claude Code's IDE tools (`openFile`,
`openDiff`, `getDiagnostics`, …, unchanged) followed by the engine tools below. Calls are answered
from the IDE's event loop; tools that wait (a debugger stop, test results) never block the UI.

- **Built-in agent jobs** (the Agents tool window) get the tools automatically, each the way its
  CLI documents, never by editing the user's own settings:
  - Claude Code: `--mcp-config` with a `codecraft` stdio server;
  - Codex: `-c mcp_servers.codecraft.command=… -c mcp_servers.codecraft.args=[…]`;
  - Gemini CLI (Auto/Review): a per-job settings file, `$TMPDIR/codecraft-agent-…-job<N>/gemini-system-defaults.json`,
    named by `GEMINI_CLI_SYSTEM_DEFAULTS_PATH` (Gemini's documented "system defaults" layer, the
    lowest one). It holds the machine's own system defaults, if any, plus `mcpServers.codecraft`;
    Gemini merges `mcpServers` by name across its layers, so the servers in `~/.gemini/settings.json`
    stay (one the user also named `codecraft` wins). The directory is removed when the turn ends;
    CodeCraft never reads or writes `~/.gemini`;
  - Grok (Auto/Review): Grok's headless mode reads MCP servers only from its config files
    (`~/.grok/config.toml`, `.grok/config.toml`), and its `GROK_CONFIG`/`GROK_CONFIG_PATH`
    overlay is documented not to add servers, so CodeCraft gives it none and never edits those
    files. Add CodeCraft once (Agents › Connect an External Agent prints the command):
    ```sh
    grok mcp add codecraft-ide -- /path/to/codecraft-cli mcp --ide
    ```
    Every Grok job CodeCraft starts carries `CODECRAFT_IDE_PORT`, `CODECRAFT_IDE_LOCK_DIR`,
    `CODECRAFT_JOB` and `CODECRAFT_JOB_KEY` in its environment, so that server reaches this IDE as
    the job (if Grok passes its environment to MCP servers; not verified against a real `grok`).
    The job's feed says this when it starts;
  - Ask mode (ACP: Claude, Codex, Gemini, Grok): `mcpServers` in `session/new`/`session/load`.

  The server is the IDE executable itself, `codecraft --mcp-bridge --port <port> --lock-dir <dir>
  --job <id>` with `CODECRAFT_JOB_KEY` in its environment: a stdio bridge to the IDE's server that
  reads the token from the lock file (never from the command line) and tags the connection with
  the job and the job's key (a per-session secret, different per job). The IDE trusts a job tag
  only with the right key, so another client that has the lock file's token can't pass itself off
  as an Auto job. The job's mode applies (see below).
- **Review jobs work on their worktree.** A Review job's tool calls run in a second, headless
  session opened on its directory in the git worktree: `file_read`/`file_write`/`file_edit`,
  runs, tests, `inspect`, `problems`, debugging, profiling and SQLite files are the worktree's,
  never the open project's. Absolute paths of the open project are moved to the same place in
  the worktree; writes outside the worktree are refused. Writes there need no card (they are
  reviewed file by file afterwards; Off still denies); running, debugging, profiling and SQL that
  writes ask as for an Ask job, with the cards in the IDE. CodeCraft's own state in that session
  (`.codecraft/`) is git-ignored in the worktree, so the review lists only the agent's changes. The
  session closes when the job stops running.
- **Any other agent**: see [Connect an external agent](#connect-an-external-agent). The IDE's policy
  and approval prompts apply.
- **Claude Code over `/ide`** receives the engine tools in `tools/list` too. Note that Claude Code
  decides which IDE-server tools it shows its model; if they don't appear, add the bridge.

### Connect an external agent

Agents › ⇄ (or Settings › Tools › Agent Access › Connect an External Agent…, command
`agent.connectExternal`) shows, for Claude Code, Codex, Gemini CLI, Cursor, Grok and any MCP client,
the entry to paste into the client's configuration, with absolute paths; CodeCraft never edits
those files. Two kinds:

- **This IDE**: `codecraft-cli mcp --ide` (or `codecraft --mcp-bridge --ide`) finds the IDE by
  itself: from a job's environment (`CODECRAFT_IDE_PORT`, …), else the running CodeCraft window
  whose project holds the working directory (the lock files' `workspaceFolders`). One entry in the
  client's user configuration serves every project; the IDE's policy and cards apply.
- **Headless**: `codecraft-cli mcp <project> --allow run,debug,profile` (below).

The same from a terminal, printed with the real path of the binary:

```sh
codecraft-cli mcp --print-config claude [project] [--allow caps]   # claude mcp add codecraft -- /abs/codecraft-cli mcp /abs/project --allow run,debug,profile
codecraft-cli mcp --print-config codex                             # codex mcp add … and the ~/.codex/config.toml block
codecraft-cli mcp --print-config gemini                            # the mcpServers JSON for the project's .gemini/settings.json (~/.gemini/… with --ide)
codecraft-cli mcp --print-config cursor                            # the mcpServers JSON for .cursor/mcp.json (~/.cursor/mcp.json with --ide)
codecraft-cli mcp --print-config grok                              # grok mcp add …
codecraft-cli mcp --print-config json                              # generic {"mcpServers": {…}}
codecraft-cli mcp --print-config claude --ide                      # claude mcp add --scope user codecraft-ide -- /abs/codecraft-cli mcp --ide
```

`codecraft-cli mcp --connect <port>` still bridges to one IDE server by port (`ide.status`).

### Headless: `codecraft-cli mcp`

```sh
codecraft-cli mcp /path/to/project [--allow run,debug,profile,write,db-write] [--no-lsp]
```

An MCP server over stdio (one JSON-RPC message per line) with a long-lived session: the project
is opened once, language servers start as in the IDE, and debug sessions, profiles, test runs and
database connections live across calls until the client disconnects. Calls that need the
project's run configurations or Cargo targets (`project_info`, `run_configurations`, `debug_start`,
`profile_start`, `test_run`, `run_start`, `inspect`, `db_*`) wait while the project is still
loading right after the start (up to 2 minutes). Reading is always allowed;
the other capabilities must be listed in `--allow` (`all` allows everything): nobody is there to
approve them, so they are denied with a message naming the flag. `codecraft-cli mcp --help`
prints these snippets with the real path of the binary:

```sh
# Claude Code
claude mcp add codecraft -- /path/to/codecraft-cli mcp /path/to/project --allow run,debug,profile
```

```toml
# Codex (~/.codex/config.toml)
[mcp_servers.codecraft]
command = "/path/to/codecraft-cli"
args = ["mcp", "/path/to/project", "--allow", "run,debug,profile"]
```

```json
{ "mcpServers": { "codecraft": { "type": "stdio", "command": "/path/to/codecraft-cli",
  "args": ["mcp", "/path/to/project", "--allow", "run,debug,profile"] } } }
```

### One-shot commands

The same tools behind plain commands (everything allowed: you ran them):

```sh
codecraft-cli debug <project> <config|file> --break src/main.rs:12 [--eval expr]… [--json] [--timeout secs]
codecraft-cli analyze <project> [--scope path] [--format json|sarif|text] [--tools clippy,ruff]
codecraft-cli profile <project> <config> [--seconds N] [--json]
codecraft-cli test <project> [name-filter] [--file path] [--json]
```

`debug` runs to the first stop, prints the stop summary, evaluates each `--eval`, stops the
session (exit 1 if it never stopped). `analyze` exits 1 when there are errors; `--format sarif`
writes SARIF 2.1.0. `test` exits 1 when a test fails.

## Access policy

Settings › Tools › Agent Access (commands `agentAccess.get/set/reset/clearSession`, saved in
`$CODECRAFT_HOME/agent-access.json`):

| Capability | Covers | Default |
|---|---|---|
| `read` | files, problems, symbols, usages, state of the debugger/profiler/tests/runs, data source metadata, read-only SQL | Allow |
| `run` | run configurations, tests, inspections (they build and run code), stopping runs | Ask |
| `debug` | start/attach, breakpoints, stepping, evaluation | Ask |
| `profile` | start/stop the profiler | Ask |
| `write` | `file_write`, `file_edit`, any command that edits the project | Ask |
| `db-write` | SQL that is not a plain read: INSERT/UPDATE/DELETE/DDL, `SET`/`BEGIN`/`PREPARE`, `SELECT … INTO`, `EXPLAIN ANALYZE`, and reads that call a function known to change state (`nextval`, `pg_terminate_backend`, `dblink_exec`, `GET_LOCK`, `load_extension`, …) | Ask |

Each is Off, Ask or Allow. **Ask** shows an approval card in the IDE (the tool and its JSON input;
**Allow** once, **Always** for this session and this caller and capability, **Deny**); the call
waits for the answer (2 minutes, then it is denied). Headless, Ask is denied. Agent jobs: **Auto**
jobs get Ask capabilities without a card (their user chose Auto; Off stays off); **Ask** jobs see
the cards; **Review** jobs act on their worktree: `write` there needs no card (it is reviewed
afterwards), the rest is asked as for Ask jobs, and nothing reaches the open project.
Agents can't change the policy, answer approvals, start other agents or reconfigure the IDE
integration through `codecraft_execute` (`agentAccess.*` except `get`, `agent.*`, `ide.*`,
`feedback.*` are refused), nor publish or change things outside this machine: `git.push`/`vcs.push`,
sharing on GitHub/GitLab, gists, creating or merging pull requests, fork sync, deployment uploads
and syncs, creating/renaming/deleting files on remote hosts, and `docker.prune` are refused (the
user runs them). A data source marked *confirm writes* (production) still asks its own
confirmation in the IDE, and headless such writes are refused.

## Cancellation and disconnects

A client may cancel a call it is waiting for with MCP's `notifications/cancelled` (`requestId`):
a call waiting for its approval card has the card withdrawn; a call waiting for a debugger stop,
test results, a run's end, a profile or a language server stops waiting. Nothing is sent for the
cancelled request (as MCP asks). Work the call already started goes on (a test run, a debug
session; stop it with `run_stop` / `debug_control stop`). When a client disconnects, all its
waiting calls are dropped and their cards withdrawn at once. Both work over the IDE's server,
the bridge and `codecraft-cli mcp`.

## Tools

Examples show the arguments an agent sends and the JSON text of the result (shortened with `…`).
Errors come back as MCP tool errors (`isError: true`) with a message saying what to do. Result
fields are camelCase throughout (`commandLine`, `typeName`, …; `crates/engine/tests/agent_results_camel_case.rs`
walks every tool's result), and command lines show what the user's command was, without the
report files CodeCraft adds in the temp directory (pytest's JUnit XML, GoogleTest/Catch2/doctest/
Boost reports, jest/vitest JSON). `codecraft_execute` passes a command's own JSON through; a few
older commands (`debug.state`) still use snake_case fields there; the dedicated tools and the
agent commands (`debug.agentSummary`, `analysis.*`, `profiler.report`, …) don't.

### Generic

**`codecraft_commands`** — search the command registry. `capability` says what `codecraft_execute`
needs for it (`blocked` = agents may not run it).
```json
{ "query": "step over" }
→ { "total": 2, "offset": 0, "commands": [{ "id": "run.debuggingActions.stepOver", "label": "Step Over",
    "params": "", "enabled": false, "disabledReason": "the program is not paused",
    "menu": "Run › Debugging Actions", "capability": "debug" }, …] }
```

**`codecraft_execute`** — any command by id. The capability depends on the command (and, for
`db.execute` with `sql`, on whether the SQL writes).
```json
{ "id": "cargo.check" }                       → { "id": "cargo.check", "result": { "tab": 3 } }
{ "id": "debug.state" }                       → { "id": "debug.state", "result": { "active": false, … } }
```

**`project_info`**
```json
{} → { "project": { "root": "/work/app", "name": "app" }, "runConfigurations": [{ "index": 0,
       "name": "Run app", "kind": "cargo", "commandLine": "cargo run --bin app", "selected": true }],
       "languageServers": […], "debugger": "none", "breakpoints": 0,
       "profiler": { "recording": false, "snapshot": false }, "testRuns": 0, "runTabs": [], … }
```

### Files and editor

**`file_read`** — with the editor's unsaved text; numbered lines; 400 lines per call.
```json
{ "path": "src/main.rs", "startLine": 10, "endLine": 12 }
→ { "path": "/work/app/src/main.rs", "startLine": 10, "endLine": 12, "totalLines": 80, "unsaved": false,
    "content": "10\tfn main() {\n11\t    let x = parse();\n12\t    run(x);" }
```

**`file_write`** (write) — `{ "path": "src/new.rs", "content": "…" }` → `{ "path": "/work/app/src/new.rs", "created": true }`.
Open editors get one undo step and are saved.

**`file_edit`** (write) — `{ "path": "src/main.rs", "oldText": "run(x)", "newText": "run(&x)" }` →
`{ "path": "…", "replacements": 1 }`; `oldText` must occur once unless `replaceAll`.

**`editor_open`** — `{ "path": "src/main.rs", "line": 42 }` → `{ "opened": "/work/app/src/main.rs", "line": 42 }`.

### Analysis

**`problems`** — what the IDE knows now (language servers, cargo, the open findings of the last
`inspect`), errors first. Findings of the last analysis carry `source: "analysis"`, the
`analyzer`, and the `id` that `analysis.fix` / `analysis.suppress` take (`fixable` when a
quick-fix exists).
```json
{ "path": "src", "severity": "warning" }
→ { "total": 2, "errors": 0, "problems": [{ "file": "/work/app/src/main.rs", "line": 2, "column": 9,
    "endLine": 2, "severity": "warning", "message": "unused variable: `unused`", "source": "language-server" },
    { "file": "/work/app/src/util.py", "line": 1, "column": 8, "endLine": 1, "severity": "warning",
      "message": "`os` imported but unused", "source": "analysis", "analyzer": "ruff", "code": "F401", "id": 0, "fixable": true }] }
```

**`inspect`** (run) — Inspect Code on a scope (`file`, `directory`, `project`, `uncommitted`,
`glob` with `pattern`) and wait for the findings. Build output (`cmake-build-*`, `CMakeFiles`, `node_modules`, `__pycache__`,
virtual environments) is not part of a directory or project scope. Rust projects get `cargo
check` unless `tools` asks for `clippy` (or Settings › Rust turns Clippy on).
```json
{ "scope": "file", "path": "mod.py", "tools": ["ruff"] }
→ { "scope": "File mod.py", "counts": { "warning": 1 }, "total": 1,
    "tools": [{ "tool": "ruff", "ok": true, "outcome": "1 problem", "commandLine": "ruff check --output-format=json …" }],
    "findings": [{ "id": 0, "severity": "Warning", "inspection": "F401 unused-import", "tool": "ruff",
    "code": "F401", "file": "/work/app/mod.py", "line": 1, "column": 8, "message": "`os` imported but unused", "fixes": 1 }] }
```
Then `codecraft_execute` `analysis.fix {id}` (or `{rule}` for all of a kind), `analysis.suppress
{id}` (`# noqa: F401`, `#[allow(…)]`, `// NOLINT(…)`), `analysis.export {format: "sarif", path}`.

**`definition`**, **`references`**, **`hover`** — a position is `path` + `line` + either `column` or
`symbol` (an identifier on that line).
```json
{ "path": "src/main.rs", "line": 3, "symbol": "greet" }
→ { "total": 1, "locations": [{ "file": "/work/app/src/main.rs", "line": 8, "column": 4, "excerpt": "fn greet(name: &str) -> String {" }] }

hover { "path": "src/main.rs", "line": 8, "column": 4 } → { "text": "fn greet(name: &str) -> String\n\nSays hello." }
```

**`document_symbols`** — `{ "path": "src/main.rs" }` → `{ "total": 2, "symbols": [{ "name": "main", "kind": "function", "line": 1, "depth": 0, "endLine": 5 }, …] }`

**`workspace_symbols`** — `{ "query": "greet" }` → `{ "total": 1, "symbols": [{ "name": "greet", "kind": "function", "file": "/work/app/src/main.rs", "line": 8, "column": 4 }] }`

**`call_hierarchy`** — `{ "path": "src/main.rs", "line": 8, "symbol": "greet", "direction": "incoming" }` →
`{ "calls": [{ "name": "main", "kind": "function", "file": "/work/app/src/main.rs", "line": 1, "column": 4, "callSites": [3] }] }`

### Debugging

**`run_configurations`** — `{}` → `[{ "index": 0, "name": "Run app", "kind": "cargo", "commandLine": "cargo run --bin app", "selected": true }, …]`

**`debug_breakpoint_set`** (debug)
```json
{ "path": "src/main.rs", "line": 4, "condition": "i == 3", "hitCount": "2", "logMessage": "i={i}", "temporary": false }
→ { "id": 3, "file": "/work/app/src/main.rs", "line": 4, "enabled": true, "condition": "i == 3", "hitCount": "2",
    "logMessage": "i={i}", "verified": true }
```
A log message makes a logpoint (logs, doesn't stop). **`debug_breakpoint_remove`** takes `id`,
`path`+`line` or `all: true`; **`debug_breakpoints`** lists them.

**`debug_start`** (debug) — one of `config` (name or index), `file` (+ `line` for a Rust test or
`main`), `program` (+ `args`), `pid` (+ `kind`: native, rust, python).
```json
{ "config": "Run app" } → { "state": "building", "session": "Run app", "started": true, "breakpoints": 1, "next": "call debug_wait_for_stop" }
```

**`debug_wait_for_stop`** — waits for a settled stop (frames and locals loaded) or the end.
```json
{ "timeoutMs": 60000, "frames": 3 }
→ { "state": "stopped", "reason": "breakpoint", "thread": { "id": 1, "name": "main" }, "session": "app.py", "adapter": "debugpy",
    "frames": [
      { "index": 0, "name": "work", "file": "/work/app/app.py", "line": 4, "column": 9,
        "excerpt": "  3 |     for i in range(n):\n> 4 |         total += i\n  5 |     return total" },
      { "index": 1, "name": "<module>", "file": "/work/app/app.py", "line": 7, "column": 1, "excerpt": "…" } ],
    "scopes": [{ "name": "Locals", "ref": 4 }, { "name": "Globals", "ref": 5 }],
    "locals": [{ "name": "i", "value": "3", "type": "int" }, { "name": "n", "value": "5", "type": "int" },
               { "name": "total", "value": "3", "type": "int" }] }
```
Other states: `none`, `building`, `starting`, `running` (with `timedOut: true` after the timeout),
`exited` (`exitCode`, `output`: the last console lines), `failed` (`message`, `output`).
A crash stops with `reason: "exception"` and an `exception` object (`EXC_BAD_ACCESS (code=1,
address=0x0)`, a panic message, a Python exception). Long C++ signatures are shortened to
`name(…)` in frame names (`std::__1::` becomes `std::`); the frame's `file` and `line` locate it.

**`debug_control`** (debug) — `continue`, `stepOver`, `stepInto`, `stepOut`, `pause`, `stop`,
`runToLine` (+ `path`, `line`). Waits for the next stop by default and returns its summary.
```json
{ "action": "stepOver" } → { "state": "stopped", "reason": "step", "frames": [{ "index": 0, "name": "work", "line": 3, … }], "locals": […] }
{ "action": "continue" } → { "state": "exited", "exitCode": 0, "output": ["result 10", "Process exited with code 0"] }
```

**`debug_stack`** — `{ "threadId": 2 }` → `{ "thread": 2, "threads": [{ "id": 1, "name": "main", "current": false }, …], "frames": [{ "index": 0, "name": "worker", "file": "…", "line": 20 }, …], "selectedFrame": 0 }`

**`debug_variables`** — the scopes of a frame (`frame`, default the selected one), or the
children of a structured value (`ref` from locals, scopes or an evaluation).
```json
{ "ref": 1007 } → { "ref": 1007, "variables": [{ "name": "[0]", "value": "1", "type": "i32" }, { "name": "[1]", "value": "2", "type": "i32" }, …] }
{ "frame": 1 }  → { "frame": 1, "scopes": [{ "name": "Locals", "ref": 9, "count": 2, "variables": […] }], "pending": false }
```

**`debug_evaluate`** (debug) — `{ "expression": "total * 10", "frame": 0 }` → `{ "expression": "total * 10", "value": "30", "type": "int" }`.
A failing expression is a tool error with the debugger's message.

**Several sessions** (a client and a server, a Python program's subprocesses) — `debug_start`
leaves the other sessions running (debugging the same configuration again restarts it) and its
result's `sessionId` names the new session. `debug_control`, `debug_wait_for_stop`,
`debug_stack`, `debug_variables` and `debug_evaluate` take `session` (that id, or the session's
name); without it they act on the selected session (the one that stopped last).
`debug_wait_for_stop` without `session` returns the first session that stops; with `session` it
waits for that one (or one of its subprocesses). With more than one session every summary lists
them; a subprocess's summary has `process` and `parentSession`.
```json
{ "session": 2, "timeoutMs": 60000 }
→ { "state": "stopped", "reason": "breakpoint", "session": "client.py", "sessionId": 2, …,
    "sessions": [{ "id": 1, "name": "server.py", "state": "stopped", "reason": "breakpoint" },
                 { "id": 2, "name": "client.py", "state": "stopped", "reason": "breakpoint", "selected": true }] }
```
Through `codecraft_execute`: `debug.sessions`, `debug.selectSession`, `debug.closeSession`,
`debug.stopAll`, and a `session` param on every `debug.*` / `run.debuggingActions.*` command.

### Profiling

**`profile_start`** (profile) — a run configuration (`config`) or a command line (`command`:
`python3 main.py`, `cargo run --release`, `pytest -k slow`, `./build/app`), CPU by default or
`kind: "memory"`, optionally a `tool` and `seconds`. Cargo and CMake targets are built first.
`test: {path, line}` (1-based) profiles the test at a source line, as the gutter's Profile does
(Rust `cargo test --lib name -- --exact`, pytest `file::test`, GoogleTest/Catch2/doctest/Boost.Test
filters). Memory profiles with tracemalloc sample the memory every `interval` seconds (default
0.1, `0` = only the snapshot at the end) and keep the snapshot nearest the peak; the report then
has `memory` (`peak`, `peakMs`, `timeline`, `atPeak` allocation sites, `peakSnapshot`).
```json
{ "config": "Run app" }                         → { "tab": 5, "job": 1, "config": "Run app", "tool": "xctrace", "state": "building", "next": "…" }
{ "command": "python3 main.py", "kind": "memory" } → { "tab": 6, "job": 2, "tool": "tracemalloc", "state": "recording", "next": "…" }
{ "test": { "path": "tests/test_calc.py", "line": 12 } } → { "job": 3, "tool": "cProfile", "config": "pytest test_slow", "command": "… -m pytest …/test_calc.py::test_slow", … }
```

**`profile_stop`** (profile) — stops the recording, waits for the snapshot, returns the hotspots.
**`profile_hotspots`** — the hotspots of the newest profile (waits while its target builds, it
records or its snapshot loads; a profile that failed is a tool error naming the Run tab with its
output). Rust names are demangled. `inProject` marks the project's own functions; when library
code (the standard library, Python's import machinery) fills the top of the list,
`projectHotspots` lists the project's functions separately. For memory profiles the unit is
`bytes` and the percentages are shares of the allocated bytes. The order is fixed, whatever the
IDE's Method List shows (and over all threads and the whole run): `sort: "self"` (default; self
weight, then total) or `"total"` (total, then self), ties by name, location and module; the
answer says which (`sort`, `order`). `codecraft_execute` `profiler.report` / `profiler.hotspots` /
`profiler.lineHits` also take `thread` (an index or a name) and `range: {startMs, endMs}` (ms since
the first sample; xctrace and samply snapshots) to look at one thread or one phase of the run.
```json
{ "limit": 3 }
→ { "snapshot": "/home/me/.codecraft/profiler/snapshots/app-….json", "unit": "samples", "samples": 4521, "total": 4521,
    "sort": "self", "order": "selfPercent desc, then totalPercent desc, then name, location, module",
    "hotspots": [{ "name": "app::parse_line", "module": "app", "selfPercent": 41.2, "totalPercent": 55.0, "self": 1863,
                   "total": 2487, "file": "/work/app/src/parse.rs", "line": 88, "inProject": true }, …],
    "note": "selfPercent: time in the function itself; totalPercent: including what it calls",
    "report": { "hotLines": […], "topByTotal": […], "notes": ["A Cargo debug build was profiled: …"] } }
```

### Tests

**`test_run`** (run) — everything (`cargo test` / `pytest`), a file (`path`), the test at
`path`+`line`, or a name filter (`name`). Waits for the end.
```json
{ "name": "parse" }
→ { "tab": 7, "title": "Tests matching 'parse'", "commandLine": "cargo test parse", "finished": true, "status": "failed",
    "counts": { "passed": 4, "failed": 1, "ignored": 0, "running": 0 }, "durationMs": 1830,
    "failures": [{ "name": "tests::parses_empty", "suite": "app", "message": "assertion `left == right` failed: empty\n  left: 1\n right: 0",
                   "file": "/work/app/src/lib.rs", "line": 16, "excerpt": "> 16 |         assert_eq!(…)", "output": "…" }] }
```
**`test_results`** — the latest (or `tab`) run; `includePassed` lists every test.

### Run configurations

**`run_start`** (run) — `{ "config": "Run app" }` → `{ "tab": 8, "config": "Run app", "next": "…" }`

**`run_output`** — `{ "tab": 8, "tail": 50, "waitMs": 60000 }` →
`{ "tab": 8, "title": "Run app", "commandLine": "cargo run --bin app", "status": "succeeded", "running": false, "exitCode": 0, "totalLines": 12, "firstLine": 0, "lines": ["…", "sum 6"] }`
(stderr lines start with `[stderr]`; page with `offset`).

**`run_stop`** (run) — `{ "tab": 8 }` → `{ "tab": 8, "stopped": true }`

### Databases

**`db_sources`** — `{}` → `[{ "id": "ds-1", "name": "Shop", "kind": "sqlite", "readOnly": false, "confirmWrites": false, "status": "connected", "schemas": [{ "name": "main", "tables": ["items"] }] }]`.
SQLite files in the project that no data source points at are listed too, as
`{ "id": null, "name": "shop.db", "kind": "sqlite", "path": "/work/app/data/shop.db", "status": "not added", "hint": "…" }`.

**`db_query`** — read-only SQL needs `read`; SQL that modifies data needs `db-write`. `dataSource`
is a data source's id or name, or the path of a SQLite file in the project: a file no data source points at yet
is added as one first (like dropping it on the Database tool window; that changes the project's
settings, so it needs `write`), and the result says so in `added`. SQL the database refuses is a
tool error with the database's message.
```json
{ "dataSource": "Shop", "sql": "select name from items order by id", "limit": 100 }
→ { "dataSource": "ds-1", "results": [{ "sql": "select name from items order by id", "columns": [{ "name": "name", … }],
    "rows": [["apple"], ["pear"], ["plum"]], "totalRows": 3, "statusLine": "3 rows", "hasMore": false }], "output": ["…"] }
{ "dataSource": "data/shop.db", "sql": "select count(*) from items" }
→ { "dataSource": "shop-db", "added": { "id": "shop-db", "name": "shop.db", "path": "/work/app/data/shop.db" },
    "note": "the SQLite file is now data source `shop-db` …", "results": [{ "rows": [[3]], … }], … }
```
Through `codecraft_execute`, adding or editing a data source (`db.addDataSource`,
`db.editDataSource`) needs `write`; connecting, refreshing, row counts and the query history are
reads.

## Playbook

Complete call sequences for common tasks, as an agent sends them (any transport). Results are
shortened with `…`; paths are absolute and lines 1-based in the real answers.

### Debug a failing test

1. Find the failure:
   ```json
   test_run { "name": "parses_empty" }
   → { "tab": 7, "title": "Tests matching 'parses_empty'", "commandLine": "cargo test parses_empty", "finished": true,
       "status": "failed", "counts": { "passed": 0, "failed": 1, "ignored": 0, "running": 0 }, "durationMs": 1830,
       "failures": [{ "name": "tests::parses_empty", "suite": "shop", "file": "/work/shop/src/lib.rs", "line": 16,
                      "message": "assertion `left == right` failed\n  left: 1\n right: 0",
                      "excerpt": "  15 |         let v = parse(\"\");\n> 16 |         assert_eq!(v.len(), 0);\n  17 |     }" }] }
   ```
2. Stop in the code under test and start the test under the debugger (`file` + `line` debugs the
   test at that line; Python: `pytest file::test` under debugpy):
   ```json
   debug_breakpoint_set { "path": "src/lib.rs", "line": 4 }      → { "id": 1, "file": "/work/shop/src/lib.rs", "line": 4, "enabled": true, "verified": true }
   debug_start { "file": "src/lib.rs", "line": 14 }              → { "state": "building", "started": true, "breakpoints": 1, "next": "call debug_wait_for_stop" }
   debug_wait_for_stop { "timeoutMs": 120000 }
   → { "state": "stopped", "reason": "breakpoint", "thread": { "id": 1, "name": "tests::parses_empty" },
       "frames": [{ "index": 0, "name": "shop::parse", "file": "/work/shop/src/lib.rs", "line": 4, "excerpt": "…" }, …],
       "locals": [{ "name": "input", "value": "\"\"", "type": "&str" }, { "name": "parts", "value": "size=1", "type": "Vec<&str>", "ref": 1003 }] }
   ```
3. Look closer, step, and finish:
   ```json
   debug_variables { "ref": 1003 }                → { "ref": 1003, "variables": [{ "name": "[0]", "value": "\"\"", "type": "&str" }] }
   debug_evaluate { "expression": "input.is_empty()" } → { "expression": "input.is_empty()", "value": "true", "type": "bool" }
   debug_control { "action": "stepOver" }        → { "state": "stopped", "reason": "step", "frames": [{ "line": 5, … }], "locals": […] }
   debug_control { "action": "stop" }            → { "state": "none" }
   ```
4. Fix (`file_edit`), then `test_run { "name": "parses_empty" }` again → `"status": "passed"`.

### Find a hotspot and verify the fix

```json
profile_start { "command": "python3 main.py" }       → { "tab": 5, "job": 1, "tool": "cProfile", "state": "recording", "next": "…" }
profile_hotspots { "limit": 5 }
→ { "snapshot": "…/profiler/snapshots/main-….json", "unit": "…", "total": …,
    "hotspots": [{ "name": "slow_sum", "selfPercent": 81.3, "totalPercent": 81.3, "file": "/work/app/main.py", "line": 12, "inProject": true }, …],
    "report": { "hotLines": [{ "file": "/work/app/main.py", "line": 14, "percent": 64.0, "code": "total += sum(range(i))" }, …] } }
file_read { "path": "main.py", "startLine": 10, "endLine": 16 }
file_edit { "path": "main.py", "oldText": "total += sum(range(i))", "newText": "total += i * (i - 1) // 2" } → { "path": "/work/app/main.py", "replacements": 1 }
profile_start { "command": "python3 main.py" }       → { "tab": 6, "job": 2, … }
profile_hotspots { "limit": 5 }                       → { "hotspots": [{ "name": "main", … }, …] }   (slow_sum is gone from the top)
codecraft_execute { "id": "profiler.compare", "params": { "a": "<first snapshot>", "b": "<second snapshot>" } }
→ { "id": "profiler.compare", "result": { "faster": [{ "name": "slow_sum", … }], "slower": [], … } }
```
Cargo and CMake configurations (`profile_start { "config": "Run app" }`) are built first;
`profile_hotspots` waits for the build, the recording and the snapshot. Rust debug builds are
flagged in `report.notes` (profile `cargo run --release`).

### Fix lint problems in changed files

```json
inspect { "scope": "uncommitted", "tools": ["clippy", "ruff"] }
→ { "scope": "Uncommitted files", "counts": { "warning": 2 }, "total": 2,
    "tools": [{ "tool": "clippy", "ok": true, "outcome": "1 problem", "commandLine": "cargo clippy --message-format=json …" }, …],
    "findings": [{ "id": 0, "severity": "Warning", "inspection": "clippy::needless_return", "tool": "clippy", "file": "/work/app/src/lib.rs",
                   "line": 9, "column": 5, "message": "unneeded `return` statement", "fixes": 1 },
                 { "id": 1, "severity": "Warning", "inspection": "F401 unused-import", "tool": "ruff", "file": "/work/app/tools/gen.py",
                   "line": 1, "column": 8, "message": "`os` imported but unused", "fixes": 1 }] }
codecraft_execute { "id": "analysis.fix", "params": { "id": 0 } }
→ { "id": "analysis.fix", "result": { "accepted": [0], "conflicts": [], "invalid": [], "stale": 0, "files": 1, "remaining": 1 } }
codecraft_execute { "id": "analysis.fix", "params": { "rule": "F401" } }   (every problem of a rule)
codecraft_execute { "id": "analysis.suppress", "params": { "id": 1 } }     (`# noqa: F401`, `#[allow(…)]`, `// NOLINT(…)`)
problems { "path": "src" }                                                   → { "total": 0, "errors": 0, "problems": [] }
```
`scope: "glob"` with `pattern: "src/a.rs, src/b.rs"` inspects an explicit list of files.

### Inspect a database table safely

Reads need only `read`; anything that could change data needs `db-write` (asked in the IDE,
denied headless unless allowed), and a data source marked *confirm writes* still asks its own
confirmation.
```json
db_sources {}
→ [{ "id": "ds-1", "name": "Shop", "kind": "postgres", "readOnly": false, "confirmWrites": true, "status": "connected",
     "schemas": [{ "name": "public", "tables": ["orders", "items"] }] },
   { "id": null, "name": "local.db", "kind": "sqlite", "path": "/work/app/local.db", "status": "not added", "hint": "…" }]
db_query { "dataSource": "Shop", "sql": "select column_name, data_type from information_schema.columns where table_name = 'orders'" }
→ { "dataSource": "ds-1", "results": [{ "columns": [{ "name": "column_name", "typeName": "name" }, …], "rows": [["id", "integer"], ["total", "numeric"], …],
    "totalRows": 6, "statusLine": "6 rows", "hasMore": false }], "output": ["…"] }
db_query { "dataSource": "Shop", "sql": "select count(*), max(created_at) from orders" }
db_query { "dataSource": "Shop", "sql": "select * from orders order by id desc", "limit": 20 }   → 20 rows, "hasMore": true
```
Prefer `count(*)`, `limit` and explicit columns; `EXPLAIN` (not `EXPLAIN ANALYZE`, which runs
the statement) shows a plan as a read.

### Review a branch (analysis + tests)

Run as a **Review** job (its calls act on the job's worktree, the open project is untouched), or
on a checkout of the branch:
```json
codecraft_execute { "id": "vcs.status" }
→ { "id": "vcs.status", "result": { "branch": "feature/parser", "upstream": "origin/feature/parser", "ahead": 3, "behind": 0, "files": [] } }
```
List the files the branch changes with the agent's own `git diff --name-only main...HEAD`, then:
```json
inspect { "scope": "glob", "pattern": "src/parse.rs, src/lib.rs, tests/parse.rs" }   → { "total": 1, "findings": [{ "id": 0, "file": "/wt/…/src/parse.rs", "line": 41, … }] }
problems { "path": "src", "severity": "warning" }                                  → { "total": 1, "errors": 0, "problems": […] }
test_run {}                                                                          → { "status": "passed", "counts": { "passed": 42, "failed": 0, … }, "failures": [] }
test_run { "path": "tests/parse.rs" }                                                → the branch's own tests
```
Report the findings with their `file:line`; a failing test's `excerpt` and `message` go into the
review as they are.

## For developers

- Schemas: `crates/mcp/src/engine_tools.rs`; transports: `crates/mcp/src/{server,stdio,bridge}.rs`.
- Execution, capabilities, waits: `crates/engine/src/agent_tools.rs`; the commands they use that
  were new (`debug.agentSummary`, `debug.variablesOf`, `files.*`, `analysis.problems`,
  `lsp.query`/`lsp.queryResult`, `run.output`, `run.stopTab`, `run.configurations`,
  `test.summary`, `test.runFiltered`, `profiler.hotspots`, `commands.search`, `project.info`):
  `crates/engine/src/agent_tools_cmds.rs`; the policy: `crates/engine/src/agent_access.rs`; the
  stdio server, the bridge and the job's server entry: `crates/engine/src/agent_mcp.rs`.
- Review jobs' worktree sessions: `crates/engine/src/agent_worktree.rs`; per-CLI MCP delivery
  (Gemini's settings file, Grok's environment): `crates/agent/src/cli.rs`; client configuration
  and `--ide` discovery: `crates/engine/src/agent_mcp.rs`; cancellation:
  `agent_tools::{cancel, client_gone}` and the transports' `Inflight` (`crates/mcp/src/lib.rs`).
- Tests: `crates/engine/tests/agent_tools.rs` (tools on temp projects, policy, IDE server and
  bridge, real debugpy/lldb-dap/rust-analyzer sessions when installed, a fake profiler),
  `crates/engine/tests/agent_everywhere.rs` (fake Gemini and Grok jobs, job keys, cancellation and
  disconnects over WebSocket and stdio, a Review job's calls on its worktree),
  `crates/engine/tests/agent_results_camel_case.rs` (every tool's result keys),
  `apps/codecraft-cli/tests/agent_cli.rs` and `agent_connect.rs` (the built binary over stdio,
  one-shot commands, `--print-config`, `--ide`, cancellation).
