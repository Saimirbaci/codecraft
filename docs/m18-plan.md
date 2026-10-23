# M18: self-update safety and feedback builds

## Decisions and layout

The installed entry point is a stable launcher, replaced only by releases. macOS packages
contain `Contents/MacOS/codecraft-launcher` and a complete factory app under
`Contents/Resources/factory/CodeCraft.app`. Version payloads contain only the real app.
Linux/Windows put the launcher at the usual `codecraft` entry point and the app in `factory/`.
Developer `cargo run -p codecraft` bypasses the launcher.

The launcher starts the bundle executable directly, rather than `open -n`: direct spawn gives
an owned child, reliable exit/timeout detection, inherited health environment, and redirected
output. The outer launcher bundle is background-only (`LSUIElement`), leaving the payload as
the sole foreground app. Dock identity and native menu behavior require GUI verification.
Shift detection is omitted: std has no safe, reliable modifier-key API. `--factory` is the
supported escape hatch, independent of pointer and metadata files.

`$CODECRAFT_HOME` (otherwise `$HOME/.codecraft`) contains `versions/<semver>-<sha>-<counter>/`,
immutable payloads and `meta.json`; `state.json` holds schema/current/last_good/previous/boot/
pending_health. Factory is an independent path, never a version-store entry. Atomic files use
same-directory unique temporary files, sync, rename, and directory sync on Unix. Invalid,
oversized, unknown-schema or truncated state is absent; unknown JSON fields are ignored.
IDs are validated before any path operation. An OS-backed file lock (std `File::try_lock`) serializes store
mutations and launcher boot supervision; the OS releases locks on owner death, so leftover lock files are harmless without PID/timeout
races. Running payload leases protect GC.

## State machine and failures

Promotion records previous without replacing any payload. Before its first boot, snapshot small
configuration into its state-backup. Each boot removes stale health, records a pending attempt,
and supplies version/health/launcher environment. The main window starts the health clock;
after 20 seconds of UI progress it reports healthy atomically. Default boot deadline is 45 s.

| Event | Decision |
|---|---|
| Current healthy or untried | Launch current |
| Failed pending boot | Fall back to last_good, then factory; queue reason/log notice |
| Two failures inside boot window | Mark bad, require explicit use to re-enable |
| Exit or hang before healthy | Reap/kill child, record failure, restore backup, select fallback |
| Healthy | Mark good and clear pending health; keep failure streak until clean process exit |
| Corrupt pointers/metadata or missing payload | Use valid last_good, then independent factory |
| Factory fails | Return an actionable error; do not loop forever or blacklist factory |
| Two launchers | One owns boot supervision; second returns busy |
| Crash after health | Supervisor continues waiting, records abnormal exit and rollback |
| Full disk / backup / atomic write error | Return error; preserve old payload and pointers |

One failed boot is quarantined with its reason; an explicit Use/forced launch can retry it.
Two abnormal exits within 300 seconds mark it bad. A healthy clean exit resets the consecutive
failure streak. Config created after a snapshot is moved into a version-local quarantine on
restore when that config was originally absent. The durable counter survives garbage collection.

Snapshots use a bounded configuration allowlist (including settings, keymaps, SSH server
configuration and small sessions); never follow symlinks, scan external homes, or copy worktrees,
build targets or caches. Restore overlays the captured configuration. Logs rotate at a fixed
cap. Pass 2 will run all stage/build/backup work on workers.

## Test strategy

All store tests use temporary roots; launcher integration tests use fake shell payloads and
explicit factory paths. No real user configuration, network, git, cargo, or GUI is used.
Cover pure selection, hostile IDs, corruption, atomic interruption, health, startup crash,
timeout, late crash, locks, GC leases and snapshot bounds. Packaging tests inspect generated
layouts. Run requested crate tests, strict clippy, layers and parity with `CARGO_TARGET_DIR=target-m18`,
`CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=3`. Record actual validation below after implementation.

## Pass 2

Pass 2 adds managed source, preflight, worker payload builds, Review acceptance and restart
handoff, described below. The L0 safety/source/build modules have no feedback dependencies.

## Validation record

Direct local smoke, on macOS in the restricted workspace (all version executables are shell
scripts and all state lives in a `tempfile.TemporaryDirectory`):

```sh
python3 /tmp/m18-smoke.py
```

The script invoked `target-m18/debug/codecraft-launcher` with explicit `CODECRAFT_HOME`,
`CODECRAFT_FACTORY_DIR`, `CODECRAFT_BOOT_TIMEOUT=3`, two fake installed versions (one exits 1,
one reports health after 1 s), and a fake factory. Exact child commands: launcher with no flags,
`--status`, `--factory`, `--rollback`. All exited 0. Assertions passed: startup crash falls back
to last_good, rollback notice exists, status JSON selects last_good, factory launch works,
manual rollback launches, original configuration survives. No real home/network/git/cargo used
by this smoke. The temporary tree was removed automatically.

Validation commands and full-gate results follow.

### Packaging and native smoke

Executed with `CARGO_TARGET_DIR=target-m18 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3`:

```sh
cargo xtask bundle --layout-only
cargo xtask bundle --version-payload target-m18/version-payload-smoke --layout-only
/usr/bin/iconutil -c iconset target-m18/bundle/CodeCraft.app/Contents/Resources/CodeCraft.icns -o /tmp/codecraft-m18-icon-verify.iconset
```

All exited 0. Python `plistlib` assertions passed: outer `CFBundleExecutable` is
`codecraft-launcher`, outer `LSUIElement` is true, factory `CodeCraft.app/Contents/Info.plist`
exists, standalone payload executable is `codecraft`, payload is foreground and contains no
nested factory. Layout-only intentionally contains no executables; the fake-binary layout test
checks the executable files and Unix executable bits. Native ICNS encoding replaces the old
iconutil encoding path, which returned “Invalid Iconset” on this machine; Apple's iconutil
successfully decoded the Rust-produced ICNS back to an iconset.

The macOS `stable_factory_signatures_keep_entitlements_and_runtime` test copies `/usr/bin/true`
into a temporary fake launcher and factory app, signs factory then outer bundle ad hoc, verifies
both with `codesign --verify --deep --strict`, checks hardened runtime and confirms the factory's
Apple Events entitlement survives outer sealing. This passes; no certificate or network is used.

### Test gates

All cargo commands used the three environment values above, no `cargo update`:

```sh
cargo test -p codecraft-engine -p codecraft-ui-egui -p codecraft
cargo test -p codecraft-engine --lib version_cmds::tests
cargo test -p codecraft-ui-egui --lib
cargo test -p codecraft-ui-egui
cargo test -p codecraft-selfupdate -p codecraft-launcher -p codecraft -p xtask
cargo clippy -p codecraft-selfupdate -p codecraft-launcher -p codecraft-engine -p codecraft-ui-egui -p codecraft -p xtask --all-targets -- -D warnings
cargo xtask layers
cargo xtask parity
```

- Broad engine gate: **867 passed, 10 failed**, including a passing new version-command test.
  Failures are in unchanged HTTP loopback tests (2), inline-completion loopback (1), notebook
  matplotlib display (1), process liveness (1), SSH tunnel loopback/process tests (3), Docker
  context relay loopback (1), and filesystem watcher (1). Local port allocation reports
  `Operation not permitted`; process listing is also denied in this sandbox. These existing
  environmental/harness failures are outside pass 1. The combined cargo command stops at engine
  failures, so UI was run separately.
- Focused engine version-command test: **1 passed**, using `Session::isolate_home`.
- UI unit tests: **62 passed**, including the new dialog/menu regression.
- Full UI run: passed unit tests and multiple integration suites, then
  `debug_sessions_ui::two_live_sessions_show_in_the_status_bar_and_stop_all` reported running
  over 60 seconds without completing. Interrupted with Ctrl-C (exit 130); full UI gate is
  **not green**. No production debugger/transport code was changed to hide this failure.
- Non-UTF-8 filesystem tests explicitly probe their temporary paths. This restricted macOS host
  rejects creation with `Operation not permitted`, including an independent Python `os.mkdir`
  probe. Those filesystem branches report the limitation and return here; they remain runnable
  on a host that permits such names. Parsing a non-UTF-8 developer project argument is separately
  tested without filesystem access. Space-containing paths run through the real launcher smoke.
- Layers: **passed**. Parity: **passed**, regenerated checklist (192/192 core, 407/407 in scope);
  no catalog commands were added, so checklist contents are unchanged.

Final reruns completed successfully:

- `cargo test -p codecraft-selfupdate -p codecraft-launcher -p xtask`: **16 selfupdate tests,
  7 launcher integration tests, 38 xtask unit tests and 3 release-script tests passed**. The two
  non-UTF-8 filesystem fixture guards are included in those totals and reported unsupported
  here, as described above. Health API tests also run a fresh child test process.
- `cargo test -p codecraft-selfupdate -p codecraft-launcher -p codecraft -p xtask`: **passed**,
  including both app argument tests. Further bounds/FIFO/escape-hatch regressions
  were then included in the final safety rerun above.
- Strict all-target Clippy for selfupdate, launcher, engine, ui-egui, app and xtask: **passed**
  on the final source. `git diff --check`: **passed**.

The definition-of-done full gate remains **non-green** because of the unchanged engine failures
and interrupted UI debugger integration. The new M18 safety/command/dialog/packaging checks are
green; no commit or push was made.

### Coverage and remaining verification

Automated fake-payload tests cover health promotion; startup failure/notice/config restore;
two failures and explicit re-enable; timeout kill, FIFO health marker, late abnormal exit;
factory-only install; corrupt/truncated/empty state and metadata; forced factory; concurrent
launchers; pure selection; bounded snapshots and absent-config quarantine; GC running leases;
atomic interruption/concurrent readers; monotonic counters after GC; clean-exit failure streak
reset; and persistent manual factory rollback. Read paths reject non-regular JSON files so a
FIFO cannot block the factory escape hatch. Metadata reads are capped at 64 KiB and version
listing at 1024 entries. Source provenance and failure reasons are bounded before allocation.
Forced factory supervision never reads or rewrites version pointers, even if `state.json` is a
directory. Payload copies reject symlinks and cap entries/depth/total bytes.

The real GUI health timing (first painted main window plus 20 seconds of UI progress), visible
rollback notification and next-launch version switching are only covered through the protocol,
engine and headless UI tests here. Manual macOS checks remain: one correct Dock icon, menu bar
name/activation, Finder/Dock launch behavior, session restoration across a real version change,
Gatekeeper/quarantine/TCC behavior on ad-hoc rebuilds, and native icon appearance. Shift-launch
is deliberately unsupported; use `--factory`. Linux/Windows layouts are unit-tested, not run
on those operating systems. At the end of pass 1, managed clone, preflight, build worker and Accept/restart integration
remained for pass 2; the implementation and validation are recorded below.

## Pass 2 implementation

Managed source is `<base>/source`. Resolution keeps explicit source location, environment override,
and the available compile-time tree ahead of the managed clone. Packaged feedback queues source
preparation on a worker and resumes submission after checkout; the panel reports setup. The default
repository is `https://github.com/Saimirbaci/codecraft.git`; settings and `CODECRAFT_REPO_URL` override
it. Git credentials stay with git. All git invocations disable terminal prompts and use argument
arrays. HTTPS, SSH and local file transports are accepted; external helpers/options are rejected.
A partial clone starts at the running build commit when available and creates `codecraft-user`.
An ownership marker and symlink guards prevent mutation of an arbitrary checkout. Developer source
is never checked out, rebased or committed by the update flow.

Upstream update refuses uncommitted and ignored files, requires a clean index and worktree, fetches without moving remote-tracking
branches, then rebases the user branch. Conflicts report paths and abort; user branch/worktree,
FETCH_HEAD and ORIG_HEAD are restored. Fetched objects can remain in git's object database.
Accept commits only accepted paths, using configured author identity or the CodeCraft fallback.
Revert creates a new commit and aborts conflicts. There is no push or hard reset.

Preflight returns serializable checks with ok/warn/fail, message and fix hint. It checks git,
cargo/rustc against the selected source's workspace Rust minimum, platform development tools, disk space
on the target volume, and repository reachability only when requested for a network operation.
First builds require 15 GiB by default; a warm selfupdate profile directory lowers this to 3 GiB.
Both thresholds are configurable. Rust installation instructions open rustup.rs in a browser;
no installation commands are executed.

Review queues preflight before enabling its build action; Accept only remains available. Cancel
revokes any queued source-setup submission, so reopening the overlay cannot start an old request.
The worker invokes the existing xtask payload bundler with `--profile selfupdate` and a unique
payload directory, atomically reserved to reject stale directories even after PID reuse. A
successful Cargo exit without an app executable is an error. `<base>/build-target` persists across builds. The profile inherits release,
disables LTO and debug info, uses 256 codegen units and enables incremental compilation. This trades
peak runtime optimization and extra build-cache space for short feedback rebuilds. Production builds
can download Cargo dependencies through Cargo; the smoke explicitly sets `CARGO_NET_OFFLINE=true`.
Build output is streamed through bounded channels, retained in a capped 1 MiB log, and saved as
`<base>/selfupdate-build.log`, with a successful build's log also kept in its version directory. Closing the Build tab does not cancel the job. Cargo compiler JSON
becomes existing Cargo Problems; the console displays rendered diagnostics rather than artifact JSON.
Both the console and durable raw build log are capped. Cancel terminates the build process group on Unix. Windows uses taskkill for child-tree cancellation and PowerShell DriveInfo for disk probing;
both still need native verification.

### Accept/build/restart state machine

```
Review CodeCraft source
  -> confirmation (file list, version naming, session restoration notice)
  -> Accept All (existing command)
  -> save accepted open buffers on worker (unrelated buffers untouched)
  -> managed source: commit accepted files
  -> preflight -> payload build -> source-change check
  -> stage + provenance -> promote pending -> garbage collection
  -> Build only / debug active / unsaved buffers: Ready, Restart now
  -> otherwise: save + hot exit -> spawn stable launcher waiting for app PID -> Quit
  -> launcher waits for old process and supervisor lock -> selected payload
  -> healthy: updated notification + Roll Back
  -> unhealthy: pass-1 fallback + rollback notice/log + agent follow-up / confirmed git revert
```

Review's primary action is `Accept, Build & Restart`; `Accept only` retains the existing action.
Other projects retain `Accept All`. Both primary and Auto job flows require the confirmation.
The initial confirmation describes the version naming scheme because a managed commit's hash is
assigned after acceptance; the final exact ID appears in Ready/status and the restart confirmation.
`selfupdate.build` may pin the current checkout with `rev`; a different revision is rejected instead
of checking out over user work. Metadata records source commit/branch, parent version, bounded feedback
summary, saved feedback session, managed commit and developer dirty hash.

`selfupdate.restartInto` uses `--wait-pid <app-pid> --version-id <id> --wait-lock`, detached into a
separate process group on Unix. The launcher bounds waiting to five minutes and refuses uncertain
process liveness. An explicit restart prompts to Save/Discard/Cancel when buffers are modified;
save failure cancels handoff. Existing hot exit and application exit persistence save editor/project,
terminal and agent-session state. A developer launch shows the built payload and offers Open rather
than quitting. Restored feedback uses the saved agent-session key, not a process-local job ID.

Settings › Tools › Self-update exposes repository/source location, retention, disk thresholds,
profile, always-on restart confirmation, checks and upstream update. All operations are commands,
including browser/payload opening. `selfupdate.*` is a distinct sensitive agent capability: Off or
Ask, never Allow, no Auto bypass, no session-wide approval, and denied to headless agents.

| Failure | Result |
|---|---|
| Git missing/private remote unreachable | Actionable worker error, no credential storage or prompt |
| Dirty managed tree / wrong branch / symlink | Refuse mutation, preserve files |
| Upstream conflict | Report conflicting paths, abort rebase, retain user branch |
| Preflight failure | Show checks/fix hints; disable Review build action until checks pass |
| Build failure or cancel | Old app remains open, capped log retained, no stage/promotion |
| Source changes during build | Refuse staging; ask for a fresh build |
| Stage/promote failure | Report error; existing immutable payload survives |
| Active debugger/dirty buffers | Keep Ready; explicit Restart now with save/discard confirmation |
| No stable launcher | Show payload path and Open action, keep developer app running |
| Old PID does not exit / liveness denied | New launcher returns an error, no concurrent payload |
| New payload broken | Real supervisor rolls back; notice and logs can be sent as a follow-up |

### Real offline smoke and build times

Executed `./tools/selfupdate-smoke.sh` on this macOS machine, using a temporary local bare remote,
a snapshot including uncommitted source, a managed user branch, temporary HOME/version store,
explicit existing Cargo/rustup caches, and no agent or network. The temporary tree was removed.
Output, with Cargo artifact JSON omitted, is preserved in [m18-smoke-output.txt](m18-smoke-output.txt).

```
Finished `selfupdate` profile [optimized] target(s) in 9m 57s
COLD_BUILD_SECONDS=617.18
Finished `selfupdate` profile [optimized] target(s) in 33.93s
WARM_BUILD_SECONDS=35.91
PAYLOAD_VERSION=Code Craft 0.3.0 (build dd8ab8fcbd)
SMOKE_OK real payload healthy; broken payload rolled back to 0.3.0-dd8ab8fcbd5d-1
```

Cold means an empty temporary build-target with cached Cargo dependencies; wall time includes
building xtask, the real desktop executable, payload layout and ad-hoc signing. Warm retains that
target, accepts another Rust source edit as a commit, and rebuilds with the new build commit embedded.
The diagnostic verifies `--version` reports that accepted commit before launcher supervision.
Other validation was compiling concurrently, so these measurements include contention. An earlier
run measured 426.57 s cold and 1.54 s for an unchanged warm build; these are not directly comparable
to the final warm edit/recommit measurement. This is a headless health-protocol check
(`--selfupdate-health-check`), not evidence of a painted GUI or restored live terminal processes.

### Pass 2 validation commands

All repository gates use `CARGO_TARGET_DIR=target-m18 CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=3`,
Cargo's offline mode, and no `cargo update`. New tests use temporary roots, local bare git remotes
and fake agent/cargo/rustc/launcher scripts. The real smoke uses the actual desktop executable
and stable launcher; it does not exercise a real agent or native GUI.

```
cargo test --offline --no-fail-fast -p codecraft-selfupdate -p codecraft-launcher -p codecraft-engine -p codecraft-ui-egui -p codecraft -p xtask
cargo clippy --offline --workspace --all-targets -- -D warnings
cargo xtask layers
cargo xtask parity
```

### Pass 2 gate results

- `cargo clippy --offline --workspace --all-targets -- -D warnings`: passed. Rerun after safety
  refinements; no new dependencies, unsafe code, or production panic sites were introduced.
- `cargo xtask layers`: passed. `cargo xtask parity`: passed, regenerated checklist remains
  192/192 core and 407/407 in scope.
- Selfupdate: 16 unit tests plus the store/health/atomic integration suites passed. Local bare git
  tests cover clone, commit/revert, successful upstream rebase, conflict abort, dirty/ignored-file
  refusal, inherited git-context isolation, URL/option/credential rejection and complete dirty fingerprints. Metadata reads are
  bounded and reject symlinks/non-regular files. No tests use a remote git service.
- Launcher: 8 integration tests passed, including real PID wait and invalid PID rejection. A final
  concurrent compilation/test run hit the fixtures’ three-second boot deadlines; the complete
  selfupdate/launcher/app rerun passed without code changes (all 8 launcher tests green).
- App: 2 argument tests passed. Xtask: 39 unit tests and 3 release-script tests passed, including
  custom/built-in profile output paths and the existing signing/layout checks.
- Engine flow: 2 tests passed, using local git, fake agent/cargo/rustc and launcher. They verify
  accepted open buffers reach disk, managed commits, compiler Problems and readable console output,
  close-tab survival, stage/promote, exact handoff argv, crash follow-up text, failure and cancel.
- UI: 62 unit tests and the new egui_kittest Review test passed. The latter checks CodeCraft vs
  other-project labels, confirmation, missing-toolchain instructions, and rejection of stale
  process-local job IDs when identifying feedback jobs. The final post-compilation rerun
  hit its five-second preflight deadline; a quiet direct rerun passed without code changes.
- Focused engine checks cover managed fallback resolution, policy Off/Ask behavior, always-on
  restart confirmation, hostile IDs/URLs, capped Unicode logs, and revocation of queued agent
  submission/automatic restart on Cancel. All live debugger sessions, including parked sessions,
  postpone restart; unsaved buffers also postpone it. Crash and build log tails have separate
  bounds, so a large build log cannot crowd the crash trace out of the follow-up or notification.

The complete no-fail-fast command **finished with exit 101: 25 failed targets**. This remains a
non-green definition-of-done gate. The engine unit result was 868 passed / 10 failed, matching
the pass-1 failures in loopback HTTP/inline completion, notebook display, process liveness,
SSH/Docker relays and filesystem watching. The broader integration run additionally encounters
permission-denied local sockets/process/PTY operations, debugger/server timeouts, and unavailable
GPU rendering (`No adapter found`). No affected integration test files were changed to suppress
these failures. Source/preflight/build/handoff tests passed in that run and were rerun separately
after final safety refinements.

A temporary Cargo test runner bounded each test binary to 180 seconds, so the no-fail-fast run
could continue rather than hang indefinitely. `agent_tools` and `qa_gallery_m15_ui` reached that
limit and returned 124; they count as failures, not skips. All tests ran under a temporary HOME,
with explicit existing Cargo/rustup caches, git global/system configuration disabled, SSH agent
socket removed, and Cargo offline. APFS non-UTF-8 fixture guards retain the EILSEQ (92) skip.

Failed engine targets: `--lib`, `agent_everywhere`, `agent_tools`, `cpp_clion`, `database_dx`,
`database_sources`, `debugger`, `debugger_dbg`, `debugger_dm`, `debugger_values`, `debugger_views`,
`ide_server`, `qa_m15`, `remote_targets`, `run_config_rc`, `rust_depth`, `terminal`,
`terminal_restore`, `vcs`. Failed UI targets: `debug_sessions_ui`, `feedback_ui`, `http_ui`,
`qa_gallery_m15_ui`, `qa_layout_ui`, `terminal_ui`.

### Coverage limits and manual checks

The real smoke proves local managed commits, actual optimized payload compilation, embedded commit,
real launcher health supervision and broken-payload rollback. Agent review, cancellation, failure
presentation, follow-up and approval policy use fakes/headless tests. Real GitHub/private credentials,
interactive GUI handoff and native Windows/Linux behavior have not been exercised here.

Manual macOS checks:

1. A real handoff leaves one correct Dock icon and the **Code Craft** menu bar name/activation.
2. Check Gatekeeper/quarantine and TCC prompts after ad-hoc rebuilt payloads.
3. Run feedback → real coding agent → Review → Accept, Build & Restart (and Build only); verify
   tabs, terminals and Claude sessions restore, unsaved/debugger postponement, and rollback actions.

No worktree commit or push was made; branch remains `codecraft/m18-self-update` at `23b3e2f`.

Launcher context is checked against the actual executable, so cargo/agent children cannot claim
their parent payload merely by inheriting its environment. Ephemeral test/snapshot apps never
report health or consume rollback notices, even when run from inside a launched IDE.
