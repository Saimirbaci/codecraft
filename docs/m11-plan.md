# M11: from the rest of the suite (CLion, DataGrip, PyCharm) plus remote, SSH and Docker

Status: done (2026-10-13; all eight tracks landed; remaining depth is in `docs/gaps.md` items 28–36). After M10 the RustRover menus are at core parity; this milestone
borrows the features from JetBrains' other IDEs that help Rust developers too, and adds working
on other machines and in containers. Same rules and process as M4/M10: read `AGENTS.md` and
`docs/clean-room.md` (behaviour from public documentation and the running apps only; never their
code or assets).

Tracks run in two waves because disk is tight (one target dir per track).

## Wave 1

| Track | Scope | Owns |
|---|---|---|
| A Debugger depth (CLion) | Memory view (DAP `readMemory`/`writeMemory`: hex + ASCII, go to address or `&expr`, edit bytes); Disassembly (DAP `disassemble`, current instruction, step by instruction via `granularity: "instruction"`, source interleaved when the adapter gives locations); Registers (the adapter's register scopes, changed values highlighted); watchpoints (DAP `dataBreakpointInfo`/`setDataBreakpoints`: break on write/read of a variable from the Variables view); LLDB console (evaluate with `context: "repl"`, history); core-dump debugging (launch with `coreFile`), remote debugging (attach to `gdbserver`/`lldb-server` via `gdb-remote` in `attachCommands`) as Run configuration kinds; Show Assembly for a Rust function (`cargo asm` when cargo-show-asm is installed, else `rustc --emit asm` via `cargo rustc`), C/C++ (`clang -S`) | `crates/dap`, `engine/debug.rs`, `engine/debug_cmds.rs`, new `engine/debug_views.rs`, new `engine/asm.rs`, `ui-egui/src/debug_panel.rs` (+ new `ui-egui/src/debug_views_ui.rs`) |
| B Database depth (DataGrip) | Query history per data source (persisted under `$CODECRAFT_HOME`), saved queries; parameter prompts for `?`, `$1`, `:name`, `${name}`; Explain Plan (PostgreSQL `EXPLAIN (FORMAT JSON)`, optionally ANALYZE; SQLite `EXPLAIN QUERY PLAN`; MySQL `EXPLAIN FORMAT=JSON`) as a tree with cost/rows; grid extras: value viewer (JSON pretty, BLOB hex, image), transpose, filter row (WHERE / ORDER BY typed into the grid header), aggregate footer for the selection (count/sum/avg/min/max), copy/export as CSV/TSV/JSON/SQL INSERT/Markdown; import CSV/JSON into a table (column mapping preview); data source colour + read-only flag (production guard: confirm before writes), manual-commit transaction mode (Commit/Rollback); DuckDB data source if the crate is cheap (else note in gaps) | `crates/db`, `engine/database*.rs`, `engine/database/`, `engine/database_cmds*`, `ui-egui/src/database_panel.rs`, `ui-egui/src/data_grid.rs` |
| H HTTP Client | `.http`/`.rest` files: requests separated by `###`, method/URL/headers/body, `{{variables}}` from `http-client.env.json` + `http-client.private.env.json` (environment selector), `@name = value` file variables, `< ./file` bodies, `# @name` request names; gutter ▶ per request (Run, Run All), responses in Services › HTTP Requests (status, timing, headers, pretty JSON/XML/HTML, save to file) and history under `.codecraft/httpRequests/`; Tools › HTTP Client: Create Request, Open Requests Collection (built-in examples), Show History, Convert cURL to HTTP Request, Copy as cURL, Import from Postman Collection; highlighting and completion of methods/headers. Response handler scripts (`> {% %}`) are out of scope (shown, not run) | new `crates/http` (parser, env, client on `ureq`, cURL/Postman conversion), new `engine/http_client.rs`, `engine/http_cmds.rs`, `ui-egui/src/http_ui.rs`, `syntax` lexer for `.http` |
| D Docker | Services › Docker: connections from `docker context ls` (Tools › Services › Docker), containers/images/volumes/networks and Compose projects (by `com.docker.compose.project` label) via the `docker` CLI (`--format '{{json .}}'`); start/stop/restart/pause/remove, logs (streamed), inspect, exec into a Terminal tab (`docker exec -it … sh`), pull/remove image, prune; Dockerfile and docker-compose run configurations (build+run, compose up/down/one service) with gutter ▶ in `Dockerfile`/`compose.yaml`; File › New › Dockerfile; Docker as a run target (run any run configuration inside an image with the project mounted); Dev Containers: read `.devcontainer/devcontainer.json`, build/start it and open a Terminal in it (`devcontainer` CLI if installed, else `docker build`/`run` from the referenced Dockerfile/image); Docker Registry… (login status / `docker login` in a terminal) | new `engine/docker.rs`, `engine/docker_cmds.rs`, `engine/docker_target.rs`, `ui-egui/src/docker_ui.rs`, `syntax` Dockerfile lexer |
| S SSH and remote | SSH servers: hosts from `~/.ssh/config` (read-only) plus CodeCraft-defined ones (`$CODECRAFT_HOME/ssh.json`; keys/agent only, no stored passwords); system `ssh` with `BatchMode` and a ControlMaster socket per host; Tools › Start SSH Session (Terminal tab); Deployment: server + path mappings per project, Upload/Download/Sync (rsync over ssh, scp fallback), Compare with Deployed Version (diff view), Automatic Upload on save, File Transfer tool window (log); Browse Remote Host (Remote Host tool window: list, open = download to a cache and upload on save, rename/delete/new folder); SSH as a run target (sync, then run in the mapped remote dir); SSH tunnels for data sources (`ssh -N -L`, local port, closed on disconnect); File › Remote Development…: pick host + remote dir, mirror into a local cache with automatic upload, open it as a project with the SSH run target preselected | new `crates/remote` (ssh config parsing, command builders, rsync/ls output parsing), new `engine/remote.rs`, `engine/remote_cmds.rs`, `engine/ssh_target.rs`, `ui-egui/src/remote_ui.rs` |
| Y Python | Interpreters: detect `.venv`/`venv`, uv, poetry, conda, pyenv and system Pythons; status-bar interpreter widget + popup to switch; create a venv (`uv venv` or `python -m venv`); the selected interpreter drives run configs, debugpy, pytest and the language server; Python Packages tool window (list/outdated/install/upgrade/uninstall via `uv pip` or `pip`, in a job); quick-fix "Install package X" on unresolved imports; Python Console tool window (REPL subprocess with a small driver script, multi-line input, history, Variables view from the driver's JSON); mypy/pyright in Inspect Code when installed; doctest runs; Django/FastAPI/Flask run configuration kinds; Tools › Create setup.py / Run setup.py Task | new `engine/python.rs`, `engine/python_console.rs`, `engine/python_cmds.rs`, `ui-egui/src/python_ui.rs`; surgical in `inspections.rs`, `run_config_store.rs` (`RunKind`), `chrome.rs` (status widget) |

## Wave 2 (starts as wave-1 tracks finish)

| Track | Scope | Owns |
|---|---|---|
| P Profiler (CLion/PyCharm) | Run › Profile (samply on macOS/Linux, `perf` on Linux; py-spy for Python), Attach Profiler to Process…, Open Profiler Snapshot (Firefox-profiler/gecko JSON from samply, speedscope JSON, collapsed stacks); Profiler tool window: flame graph, call tree (top-down), method list (bottom-up, self/total), go to source; install hints | new `crates/profile`, new `engine/profiler.rs`, `engine/profiler_cmds.rs`, `ui-egui/src/profiler_ui.rs` |
| Q SQL in Rust and SQL editing | SQL language injection in Rust strings (`sqlx::query!`/`query_as!`/`query_scalar!`/`query`, diesel `sql_query`, rusqlite `execute`/`prepare`/`query_row`, tokio-postgres/postgres `query`/`execute`, `// language=SQL` comments): highlighting, completion of tables/columns from the project's chosen data source, unresolved table/column warnings; SQL formatter (consoles, `.sql` files, injected SQL); keyword completion and alias-aware column completion | `syntax/src/sql.rs` and new `syntax/src/sql_format.rs`, new `engine/sql_lang.rs`, `engine/sql_lang_cmds.rs`; surgical hooks in `decorations.rs`/`completion.rs` |

## Shared contracts (in the tree after the scaffolding commit)

- **Tool windows:** `ToolWindowId::{Profiler, PythonConsole, PythonPackages, FileTransfer,
  RemoteHost}` with toggles `view.toolWindows.{profiler,pythonConsole,pythonPackages,fileTransfer,remoteHost}`
  and stub panels `profiler_ui::show`, `python_ui::{show_console, show_packages}`,
  `remote_ui::{show_file_transfer, show_remote_host}`.
- **Services:** the Services tool window has sections (`engine::services::ServicesSection::{Database,
  Docker, Http}`); `services::show(s, section)` selects one and opens the window. Panels:
  `database_panel::services` (existing), `docker_ui::show_services`, `http_ui::show_services`.
- **Run targets:** `RunConfig::target` and `UserRunConfig::target` (`engine::run_targets::RunTarget`:
  `Local`, `Docker(DockerTarget)`, `Ssh(SshTarget)`); `runs::start_config` runs a non-local
  configuration through `run_targets::wrap`, which calls `docker_target::wrap` (track D) or
  `ssh_target::wrap` (track S). Each track owns its target struct and its `wrap`.
- **Terminal:** `terminal::open_command(s, title, program, args, cwd)` opens a Terminal tab
  running a program (tracks D and S).
- **Session fields:** `docker`, `remote`, `http`, `python`, `profiler`, `sql_lang`, `services`,
  each with `pump` called from `Session::pump`.
- **Command modules** registered in `commands.rs`: `docker_cmds` (D), `remote_cmds` (S),
  `http_cmds` (H), `python_cmds` (Y), `profiler_cmds` (P), `sql_lang_cmds` (Q). Use the catalog's
  exact ids (`tools.httpClient.*`, `tools.deployment.*`, `tools.startSshSession`,
  `tools.services.docker*`, `tools.services.devContainers.*`, `file.new.dockerfile`,
  `file.remoteDevelopment`, `run.profile*`, `run.attachProfilerToProcess`,
  `run.openProfilerSnapshot*`, `tools.pythonOrDebugConsole`, `tools.managePythonPackages`,
  `tools.createSetupPy`, `tools.runSetupPyTask`, …).
- **New L0 crates:** `codecraft-http`, `codecraft-remote`, `codecraft-profile` (registered in
  `xtask layer_of`).

## Rules

Same as M4/M10: never panic, no unsafe, nothing blocks the UI thread, own
`CARGO_TARGET_DIR=target-<track>` with `CARGO_INCREMENTAL=0`, surgical edits to shared files, no
`cargo update`, no git commits by track agents, tests never touch the real `~/.codecraft`,
settings, `~/.ssh` or Docker state (fixtures, fake `docker`/`ssh`/`rsync` scripts on `PATH`,
`CODECRAFT_HOME`/`CODECRAFT_CONFIG_DIR` overrides). Tests never open network connections except
to servers they start on 127.0.0.1; never run real profilers, `docker` or `ssh` against real hosts.
