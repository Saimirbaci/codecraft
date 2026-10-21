# M17 — SSH remote projects

Status: pass 2 implemented across sync, language services and execution; M17 remains partial.
Tracks 5 (remote debugging validation) and 6 (effective SSH configuration, askpass and robust
reconnect queues) belong to pass 3. See gaps item 69 for the remaining limits and test gate.

The hybrid model remains: a local mirror for engine filesystem operations, SSH for remote
processes. Keep `file.remoteDevelopment`, native project windows, deployment mappings and the
worker-thread transfer log. There is no workspace-wide virtual filesystem.

## Pass 1 foundation

Opening creates a new project window, remembers the mirror and its deployment marker, exposes
connection state, and shares a private persistent ControlMaster. Bounded, incremental rsync
snapshots honor deployment defaults and per-directory `.gitignore` filters. A three-way baseline
preserves local edits and deletions; checked saves compare the host before advancing the baseline.

## Pass 2

### A. Automatic sync and conflicts

- Optional `inotifywait -m -r` runs over the shared SSH connection. Events are coalesced to a
  three-second focused / fifteen-second unfocused refresh interval. Missing or failed inotify
  falls back to one batched rsync metadata probe per interval; a download happens on differences.
  This intentionally uses rsync rather than a separate find/stat implementation so polling has
  the download's exact filters, deletion handling and size exclusions. Metadata polling cannot
  detect an edit preserving both size and timestamp; an explicit refresh still uses checksums.
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
runs its configured cargo check/clippy on the host. Missing servers report their remote install
hint; no installation is performed automatically.

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

## Remaining before calling M17 complete

- Real-host authentication, network loss, ControlPersist, host-side inotify, actual remote language
  servers/cargo and terminal job control need manual verification. All new SSH transport tests use
  executables in temporary fixture directories, never a real host or user SSH/config directory.
- Remote project root transfer paths retain the existing safe-character restriction.
- Polling uses rsync metadata, not find-newer/stat; inotify filters default excluded directories,
  while per-directory `.gitignore` filtering is enforced by the actual download. Ignored events
  can cause an unnecessary probe/refresh. This is not a remote synchronization daemon.
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
