# M17 — SSH remote projects

Status: pass 2 implemented across sync, language services and execution; M17 remains partial.
Tracks 5 (remote debugging validation) and 6 (effective SSH configuration, askpass and robust
reconnect queues) belong to pass 3. See gaps item 69 for the remaining limits and test gate.

The hybrid model remains: a local mirror for engine filesystem operations, SSH for remote
processes. Keep `file.remoteDevelopment`, native project windows, deployment mappings and the
worker-thread transfer log. There is no workspace-wide virtual filesystem.

## Pass 1 foundation

Opening creates a new project window, remembers the mirror and its deployment marker, exposes
connection state, and maintains private persistent interactive and bulk ControlMasters. Bounded, incremental rsync
snapshots honor deployment defaults and per-directory `.gitignore` filters. A three-way baseline
preserves local edits and deletions; checked saves compare the host before advancing the baseline.

## Pass 2

### A. Automatic sync and conflicts

- Optional `inotifywait -m -r` runs over the shared SSH connection. Events are coalesced to a
  three-second focused / fifteen-second unfocused refresh interval. Missing or failed inotify
  falls back to one SSH-side metadata fingerprint (`find` with default directory pruning and
  `cksum`), transferring only a fixed-size result. Idle intervals grow 3 → 10 → 30 → 60 seconds,
  five times slower unfocused; changes, focus regain and editor/terminal/transfer activity reset
  the backoff. A detected change stays queued while an interactive operation finishes.
  Polls start only with no operations in flight. `remote.project.autoRefresh` accepts
  `pollIntervalSecs` (persistent minimum, 0 disables fallback polling). Per-directory gitignore
  and size limits remain enforced by the download; ignored metadata can trigger extra refreshes.
  Same-size/same-timestamp changes require inotify or explicit checksum refresh.
- `remote.project.autoRefresh {paused?}` persists the pause setting in deployment.json. The
  status menu and File Transfer expose it. Disconnect stops detection; Reconnect resumes it.
- Commands, pump-applied edits, saves, baseline updates and repository-changing Git workers
  coordinate with the mirror lock. Dirty buffers are protected at apply time, including edits
  made during a download. Clean editors reload and remote changes notify LSP watchers.
- New files/folders and refactoring resource operations queue checked remote operations. Rename
  and delete compare an uploaded baseline (including whole directories) before changing the
  remote tree. The baseline advances only on success; failures retain the local result and
  explain the refusal in File Transfer. Saved content edits from refactoring use checked uploads.
- `remote.project.conflicts` opens a conflict list in File Transfer. `remote.project.resolve`
  offers Compare, Keep Mine, Take Remote and Merge. Existing diff/three-way merge views are reused;
  conflicted editor tabs have a warning marker. A refused upload fetches the incoming copy and
  pauses uploads for resolution while keeping the connection usable; it does not force a
  reconnect/retry loop. Applying a merge refuses a changed incoming
  snapshot, and the resulting upload still checks the host against the reviewed baseline.

### B. Remote language services

Rust-analyzer, clangd and basedpyright (or pyright when basedpyright is absent) run through
`ssh -T`. Reconnect resolves the canonical remote root and restarts servers needed by open
buffers. A pure L0 `PathMapper` translates JSON-RPC before framing and before event dispatch,
including initialize roots/options, diagnostics, locations, workspace-edit URI keys, resource
operations, watcher patterns and progress titles. Buffer text, replacement text and symbol names
are opaque. Initialize sends a null processId because a local PID is meaningless on the host.

External toolchain/registry sources map into a separate read-only source cache and are fetched
on demand with a 50 MiB cap. Single-definition navigation converts UTF-16 columns after fetching.
Refactoring refuses writes to this cache. Unsaved buffers continue to use didChange; checked
upload completion repeats didSave so server checks see the new remote disk contents. Rust-analyzer
runs its configured cargo check/clippy on the host. Servers are version-probed on workers with
cargo/local/usr-local bins on PATH inside the remote project (honoring rust-toolchain.toml). Results are
cached per host/language until reconnect; a rustup proxy without its component is unavailable.
Missing servers notify once with install hints and `remote.lsp.install {language}` (explicit,
logged installation), or `remote.lsp.localFallback {language}` (local mirror, no remote toolchain).
Clangd installation uses noninteractive host package managers; `command` can override the install
command for a particular distribution. Unexpected exits back off 2/4/8 seconds, then stop after
three retries with visible status; reconnect clears the budget.

### C. Remote terminals and runs

New shell tabs in a remote project, including `terminal.newRemote`, use `ssh -tt` and the remote
project directory. Shell tab identity, name, selected tab, output and directory remain in the
existing session snapshot. Remote OSC-7 directories map back into the mirror; local process cwd
polling does not replace them. Existing explicit Claude Code tabs and saved shell tabs with known local Claude sessions retain
their local launch and resume path; a local session ID is never sent to the remote host.

Run/Test/Debug/Profile command defaults and new/rebuilt configurations use the project's SSH
mapping without bulk uploads. Runs refuse to start while checked saves/tree operations are
pending. Managed SSH runs require `setsid` on the host, record a private pgid, and kill that group
through SSH on Stop, Disconnect, project close and quit. Stop is idempotent so repeated shutdown
signals do not interrupt its cleanup trap. Console/diagnostic paths map into the mirror; outside
paths navigate through the read-only source cache.

### First real-host follow-up

Bulk uploads and rsync/scp use a second private ControlMaster socket; PTYs, listings and LSP
retain the primary socket. Listings and file opens can proceed during background/transfer work.
Master stderr is retained and surfaced as `multiplexing unavailable: …` in project status.
Socket paths remain hashed/short; oversized override paths are refused with a reason.
`remote.diagnostics` and the status menu's Connection diagnostics show both masters, uptime,
sockets, in-flight work and a bounded invocation history. Worker measurements compare shared
and fresh RTTs and run `ssh -O check`; results appear in File Transfer. Debug invocation logs
contain program/duration/result, never command arguments. Socket presence records a sharing
request, not proof that an individual OpenSSH client attached; the explicit check verifies the
master at measurement time. QoS remains controlled by the user's SSH configuration.

## Remaining before calling M17 complete

- Real-host authentication, network loss, ControlPersist, host-side inotify, actual remote language
  servers/cargo and terminal job control need manual verification. All new SSH transport tests use
  executables in temporary fixture directories, never a real host or user SSH/config directory.
- Remote project root transfer paths retain the existing safe-character restriction.
- Polling requires Linux/GNU `find -printf` and prunes default excluded directories; per-directory
  `.gitignore` filtering is enforced by the download. Ignored events can cause extra refreshes.
- Generic recursive copies and generated populated subtrees (Cargo New/Python-package creation)
  are not fully published by the new empty-folder operation. File/directory type conflicts
  require manual reconciliation; file conflict actions explicitly refuse them without changes. Directory rename/delete conservatively
  refuse when excluded remote contents differ from the mirrored baseline. Tree operation intents
  are currently an in-memory ordered queue; durable reconnect/reopen replay remains track 6.
- Local external processes cannot be locked out. Cross-process/window queue ownership, atomic
  server-side compare-and-replace, abandoned transfer/run-state cleanup and cancellation during
  a busy transfer remain incomplete. Engine Git operations coordinate writes, but the mirror
  does not contain the remote `.git` database and is not a remote VCS implementation.
- Run starts report pending saves rather than automatically waiting/retrying. Remote terminals
  use host shell history; remote per-tab history isolation and remote Claude session hooks are not
  implemented. Existing local Claude resume behavior remains covered by its existing tests.
- Remote Debug/Profile inherit target defaults; track 5 breakpoint/stop and profiler transport
  validation remain outstanding. Effective `ssh -G` / Match, interactive askpass, reconnect and
  retained tree-operation queues remain track 6.

## Validation

All checks use `CARGO_TARGET_DIR=target-m17 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3`; no cargo
update, commits, real hosts, user SSH directories or user CodeCraft directories in the fixtures.

### First real-host follow-up gate

- `cargo test -p codecraft-engine --lib remote`: **46 passed**. Includes missing rustup
  components, project-directory version probes, install output and local fallback, retry limits,
  no-rsync idle polling, interval/activity/focus backoff, browsing during a slow bulk worker,
  retained refresh intent during interactive work, distinct sockets, master stderr and shutdown.
- Remote UI: **7 passed**, including the Connection diagnostics status-menu action.
- Remote crate: **52 passed, 2 failed** in existing relay/listener tests with Operation not
  permitted. The new persistent polling setting round-trips and legacy settings keep the default.
- All-target clippy with `-D warnings` passes for engine, remote and UI. Layers pass. Parity is
  regenerated and kept (407/407 observed actions); auxiliary install/fallback/diagnostics commands
  remain engine actions rather than additional observed RustRover menu items.
- The broad touched-crate run reached **852 passing / 10 failing engine unit tests**. Failures
  cover denied loopback sockets/process inspection, notebook/font caches and filesystem watching.
  Agent/debugger integration failures followed; the agent bridge test stalled and the broad run
  was interrupted after over 360 seconds of test runtime. This is not a green global gate.
- The full UI run completed **111 passing / 3 failing tests** before interruption after over
  360 seconds of test runtime in `qa_gallery_m15_ui::gallery_m15` (three other gallery cases
  passed before the stall). Failures: `debug_sessions_ui`'s exited debugpy adapter,
  `feedback_ui`'s missing GPU adapter and `http_ui`'s denied loopback socket. The focused remote
  UI target separately passes all seven tests.
- All SSH is fake. No real-host authentication, actual socket reuse, aarch64 installation,
  terminal latency, WAN RTT or throughput measurement was attempted. Socket presence in history
  is explicitly distinct from a successful `ssh -O check`.

### Previous pass 2 baseline

- Focused remote engine suite: 30/30; LSP: 108/108 (including fake SSH round trip and rejected
  URI traversal); remote UI: 6/6; remote-mirror agent permissions: 2/2. The engine mock-server
  test covers all three languages, reconnect, pyright fallback and a missing clangd install hint.
- Terminal restore: the new local-Claude-in-remote-project regression and the zsh test pass.
  The existing combined history/Claude test fails to identify the running shell's Claude child
  (`runs_under` needs process inspection unavailable here); this prevents a green full target.
- Remote crate: 50 pass, two existing loopback relay/listener tests fail with Operation not
  permitted. xtask: 34 pass, existing iconutil conversion fails. These are not silently skipped.
- The full touched-crate test command was attempted, then interrupted after existing engine
  socket/process/watcher/notebook failures and repeated service/debugger timeouts. The broad UI
  run was bounded to 360 seconds and timed out in debug_sessions_ui; its completed tests passed.
- All-target clippy with `-D warnings` passes for engine, UI, remote, LSP and xtask. Layers pass;
  parity is regenerated and kept (407/407 observed in-scope actions). Menu coverage is not a claim
  that M17's remaining behavior is complete. No real-host authentication, LSP, cargo, inotify,
  terminal job-control or debugger/forwarding validation was attempted.
