# M18 pass 1: self-update safety core

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

Managed source clone, toolchain preflight, worker build job, review Accept integration,
session handoff and restart orchestration are intentionally deferred. No feedback dependencies
are introduced in the safety core.

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
on those operating systems. Managed clone, preflight, build worker and Accept/restart integration
remain pass 2, as planned.
