# M16: remote targets, live analysis, safer SQL, shippable builds

Status: in progress (2026-10-16). Goal: same as M14/M15 (CodeCraft replaces PyCharm, CLion,
RustRover and DataGrip; debugging, analysis and profiling first-class for people and AI agents),
now with the four items the user chose: debugging and profiling on SSH and Docker targets,
analysis while typing in every language, bind parameters and editable console results in the
database tool, and signed, notarizable builds with CI. Same rules as M10–M15: read `AGENTS.md`
and `docs/clean-room.md`.

Tracks work in their own git worktrees; the lead merges them. At most four build at once, each
with `CARGO_TARGET_DIR=<repo>/target-<track>`, `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=3`.

## Tracks

| Track | Scope |
|---|---|
| RT Remote targets | Run, debug, test and profile on an SSH host or a Docker container, as PyCharm's remote interpreters and CLion's remote toolchains do: a configuration's target (Local / SSH server / Docker container or image / Compose service, from M11's run targets) applies to Run, Debug, Test and Profile; Python: debugpy started on the target (`python -m debugpy --listen` inside the container/host, port forwarded through `ssh -L` or `docker` port mapping or `docker exec` + a local socket relay), path mappings derived from the deployment mapping or the container mount; native: `lldb-server gdbserver`/`gdbserver` on the target with the port forwarded, the local lldb-dap attaching with `gdb-remote`, sources mapped (`target.source-map`), the binary built remotely (remote CMake/Cargo) or uploaded; profiling: py-spy/cProfile/tracemalloc on the target, perf/samply on Linux targets, the snapshot downloaded and opened; remote interpreters listed in Settings › Python (SSH/Docker) with their packages; agents get the same through the existing tools (`target` on debug_start/profile_start/test_run). Tests use fake `ssh`/`docker` executables that run the commands locally (so the real debugpy/lldb-dap/cProfile run end to end through the remote code path) — never a real host or the Docker daemon |
| LT Live analysis while typing | Analyzers re-run on the unsaved buffer as the user types (debounced ~500 ms after the last edit, cancelled by the next one, never on the UI thread): Python ruff via stdin (`--stdin-filename`), basedpyright/pyright live through the language server; Rust: rust-analyzer's native diagnostics are live already, clippy findings stay on save, plus a light syntax pass; C/C++: clangd live (open documents), clang-tidy on a temp copy of the buffer with the real compile command; SQL inspections live in `.sql` files and consoles; analysis on open for every language (QA M15); findings from a stale run never overwrite newer ones (document revision checks); a per-language "on the fly" toggle and a Power Save mode pause; agents: `analysis.fileFindings` reflects the live state with the revision it was computed for |
| DQ DataGrip queries | Bind parameters everywhere values are sent (grid edits, inserts, deletes, data import, agent `db_query` params, Data Tools): typed parameters per driver (PostgreSQL, MySQL, SQLite, SQL Server, ClickHouse where supported) instead of escaped literals, with literal fallback only for drivers that can't bind, and tests that values with quotes/backslashes/NUL/unicode/binary round-trip; console query results editable when the result maps to one table with a primary or unique key (detected from the query and metadata), with the same pending-changes/savepoint submit as Open Table; otherwise read-only with the reason shown; agents: `db_query {sql, params}` with bound params |
| SN Signed builds and CI | `cargo xtask bundle --sign "Developer ID Application: …"` with hardened runtime and entitlements, `cargo xtask notarize` with `xcrun notarytool submit --keychain-profile <profile>` + `stapler staple` (credentials only from the user's keychain profile; never stored in the repo), ad-hoc signing (`-s -`) for local builds; `.dmg` signed; verification (`codesign --verify --deep --strict`, `spctl -a -vv`); GitHub Actions workflows: CI (fmt/clippy/test/layers/parity on macOS and Linux, build check on Windows) and release (tag → macOS signed/notarized dmg when secrets exist, Linux tar/deb, Windows zip, as workflow artifacts); docs/release.md; Linux and Windows build fixes found by `cargo check --target` where toolchains are available (cross-check with `cargo check --target x86_64-unknown-linux-gnu` / `x86_64-pc-windows-msvc` if those targets are installed, else document) |

## Rules

As M14/M15: never panic, no unsafe, nothing blocks the UI thread, every action is a command,
tests are the gate. Tests never touch the real `~/.codecraft`, `~/.claude`, Keychain identities,
`~/.ssh`, the Docker daemon, GitHub, Apple's notary service, or databases other than ones they
start; never run a real agent CLI with a prompt, never the network, never `sudo`, nothing
installed globally. No `cargo update`. Track agents don't commit or push; the lead merges.
