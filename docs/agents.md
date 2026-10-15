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

- **Built-in agent jobs** (the Agents tool window) get the tools automatically:
  - Claude Code: `--mcp-config` with a `codecraft` stdio server;
  - Codex: `-c mcp_servers.codecraft.command=… -c mcp_servers.codecraft.args=[…]`;
  - Ask mode (ACP: Claude, Codex, Gemini, Grok): `mcpServers` in `session/new`/`session/load`.

  The server is the IDE executable itself, `codecraft --mcp-bridge --port <port> --lock-dir <dir>
  --job <id>`: a stdio bridge to the IDE's server that reads the token from the lock file (never
  from the command line) and tags the connection with the job, so the job's mode applies (see
  below). Headless Gemini and Grok runs (Auto/Review) get no MCP server (their CLIs only take MCP
  servers from their settings files).
- **Any other agent**: bridge to the running IDE with
  ```sh
  claude mcp add codecraft-ide -- codecraft-cli mcp --connect <port>
  ```
  (the port is in `ide.status` or the lock file name). The IDE's policy and approval prompts apply.
- **Claude Code over `/ide`** receives the engine tools in `tools/list` too. Note that Claude Code
  decides which IDE-server tools it shows its model; if they don't appear, add the bridge above.

### Headless: `codecraft-cli mcp`

```sh
codecraft-cli mcp /path/to/project [--allow run,debug,profile,write,db-write] [--no-lsp]
```

An MCP server over stdio (one JSON-RPC message per line) with a long-lived session: the project
is opened once, language servers start as in the IDE, and debug sessions, profiles, test runs and
database connections live across calls until the client disconnects. Reading is always allowed;
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
the cards; **Review** jobs are never allowed `write` (their changes belong in the worktree).
Agents can't change the policy, answer approvals, start other agents or reconfigure the IDE
integration through `codecraft_execute` (`agentAccess.*` except `get`, `agent.*`, `ide.*`,
`feedback.*` are refused), nor publish or change things outside this machine: `git.push`/`vcs.push`,
sharing on GitHub/GitLab, gists, creating or merging pull requests, fork sync, deployment uploads
and syncs, creating/renaming/deleting files on remote hosts, and `docker.prune` are refused (the
user runs them). A data source marked *confirm writes* (production) still asks its own
confirmation in the IDE, and headless such writes are refused.

## Tools

Examples show the arguments an agent sends and the JSON text of the result (shortened with `…`).
Errors come back as MCP tool errors (`isError: true`) with a message saying what to do.

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

**`problems`** — what the IDE knows now (language servers, cargo, inspections), errors first.
```json
{ "path": "src", "severity": "warning" }
→ { "total": 1, "errors": 0, "problems": [{ "file": "/work/app/src/main.rs", "line": 2, "column": 9,
    "endLine": 2, "severity": "warning", "message": "unused variable: `unused`", "source": "language-server" }] }
```

**`inspect`** (run) — Inspect Code on a scope (`file`, `directory`, `project`, `uncommitted`) and
wait for the findings.
```json
{ "scope": "file", "path": "mod.py", "tools": ["ruff"] }
→ { "scope": "mod.py", "tools": ["ruff"], "counts": { "Warning": 1 }, "total": 1,
    "findings": [{ "id": 0, "severity": "Warning", "inspection": "F401 unused-import", "tool": "ruff",
    "code": "F401", "file": "/work/app/mod.py", "line": 1, "column": 8, "message": "`os` imported but unused", "fixes": 1 }] }
```

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

### Profiling

**`profile_start`** (profile) — `{ "config": "Run app" }` → `{ "tab": 5, "config": "Run app", "tool": "samply", "next": "…" }`

**`profile_stop`** (profile) — stops the recording, waits for the snapshot, returns the hotspots.
**`profile_hotspots`** — the hotspots of the open snapshot (waits while recording or loading).
```json
{ "limit": 3 }
→ { "snapshot": "/home/me/.codecraft/profiler/snapshots/app-….json", "unit": "samples", "samples": 4521, "total": 4521,
    "hotspots": [{ "name": "app::parse_line", "module": "app", "selfPercent": 41.2, "totalPercent": 55.0, "self": 1863,
                   "total": 2487, "file": "/work/app/src/parse.rs", "line": 88 }, …],
    "note": "selfPercent: time in the function itself; totalPercent: including what it calls" }
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

**`db_sources`** — `{}` → `[{ "id": "ds-1", "name": "Shop", "kind": "sqlite", "readOnly": false, "confirmWrites": false, "status": "connected", "schemas": [{ "name": "main", "tables": ["items"] }] }]`

**`db_query`** — read-only SQL needs `read`; SQL that modifies data needs `db-write`.
```json
{ "dataSource": "Shop", "sql": "select name from items order by id", "limit": 100 }
→ { "dataSource": "ds-1", "results": [{ "sql": "select name from items order by id", "columns": [{ "name": "name", … }],
    "rows": [["apple"], ["pear"], ["plum"]], "totalRows": 3, "statusLine": "3 rows", "hasMore": false }], "output": ["…"] }
```

## For developers

- Schemas: `crates/mcp/src/engine_tools.rs`; transports: `crates/mcp/src/{server,stdio,bridge}.rs`.
- Execution, capabilities, waits: `crates/engine/src/agent_tools.rs`; the commands they use that
  were new (`debug.stopSummary`, `debug.variablesOf`, `files.*`, `analysis.problems`,
  `lsp.query`/`lsp.queryResult`, `run.output`, `run.stopTab`, `run.configurations`,
  `test.summary`, `test.runFiltered`, `profiler.hotspots`, `commands.search`, `project.info`):
  `crates/engine/src/agent_tools_cmds.rs`; the policy: `crates/engine/src/agent_access.rs`; the
  stdio server, the bridge and the job's server entry: `crates/engine/src/agent_mcp.rs`.
- Tests: `crates/engine/tests/agent_tools.rs` (tools on temp projects, policy, IDE server and
  bridge, real debugpy/lldb-dap/rust-analyzer sessions when installed, a fake profiler),
  `apps/codecraft-cli/tests/agent_cli.rs` (the built binary over stdio, one-shot commands).
