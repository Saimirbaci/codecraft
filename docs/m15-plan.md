# M15: the daily loop, polished

Status: done (2026-10-15; all five tracks landed; remaining depth is in `docs/gaps.md` items 57–61 and `docs/qa-m15.md`). Goal: same as M14 (CodeCraft replaces PyCharm, CLion, RustRover
and DataGrip; debugging, code analysis and profiling first-class for people and AI agents), now
closing the "Remaining" lists of gaps 52–56 and `docs/qa-m14.md` that a daily user or an agent
hits: analysis visible while editing, several debug sessions at once, profiling a single test,
and agents getting the same tools whatever CLI they are. Same rules as M10–M14: read `AGENTS.md`
and `docs/clean-room.md`.

Tracks work in their own git worktrees; the lead merges them. At most four build at once, each
with `CARGO_TARGET_DIR=<repo>/target-<track>`, `CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=3`.

## Wave 1

| Track | Scope |
|---|---|
| EA Analysis in the editor (PyCharm/CLion feel) | Inspection findings shown in the editor as they are for language-server diagnostics (underlines by severity, gutter marks, the error stripe, hover with the rule and fixes, Alt+Enter fixes and suppressions from the analyzer); on-save analysis of the changed file (ruff/clippy flycheck/clang-tidy per the active profile, debounced, cancelled by the next save); the managed `basedpyright` (`$CODECRAFT_HOME/tools`) used as the Python language server when installed; Settings › Languages & Frameworks › Python with an interpreter picker (QA #26) and pyright/ruff options; cargo-deny's advisories only when the user turns it on (QA #21); relative test locations in the Run window (QA #27) |
| DM Debugger, multi-session | Several debug sessions at once (two processes, a Python parent and its subprocesses, a client and a server): a session picker in the Debug tool window, per-session threads/frames/variables/console, Stop/Stop All, breakpoints shared; agents choose a session (`session` param on debug commands and MCP tools, sessions listed in stop summaries); C++ references show the referenced value (QA #23); LLDB's "no plugin for rust" line filtered (QA #22); nested Rust enums summarised one level deeper, `Box<u8>` as a number; watches saved per project; Debug at caret for any test (Rust, Python, C++ frameworks, jest/vitest via js-debug when present) |
| PP Profiling polish | Profile the test at the caret from the gutter (Rust, Python, C++ frameworks); samply snapshots symbolicated; per-thread line statistics and gutter marks for the selected thread; a time-range selection on the threads timeline that filters every view; tracemalloc periodic snapshots with the peak and a memory-over-time chart; agent hotspots in a fixed order independent of the IDE's sort (QA #25) with an explicit `sort` param; Run window Stop also stops the profile's build step |
| AI Agents everywhere | Gemini and Grok jobs get the CodeCraft MCP tools (per-job settings files in a temp dir pointed to by the CLIs' documented env/flags; never the user's real settings); Review jobs' tool calls act on their worktree (files, runs, analysis rooted there); `notifications/cancelled` cancels a waiting tool and closes its approval card; cards from disconnected clients are withdrawn; consistent camelCase in tool results (QA #24) and no internal temp paths; a `codecraft-cli mcp --print-config claude|codex|gemini|cursor` helper; docs/agents.md updated with an agent "playbook" (debug a failing test, find a hotspot, fix lint, inspect a table) |

## Wave 2

| Track | Scope |
|---|---|
| QB QA pass 2 | Gallery coverage for the Terminal, Python Console, Rust REPL, a real Git repository (log, commit, changelists, merge editor) and the remote/Docker windows with fakes; walk the M15 features as a person and as an agent; fix what breaks; `docs/qa-m15.md` |

## Rules

As M14: never panic, no unsafe, nothing blocks the UI thread, every action is a command, tests
are the gate. Tests never touch the real `~/.codecraft`, `~/.claude`, `~/.gemini`, Keychain,
`~/.ssh`, Docker, GitHub or databases other than ones they start; never run a real agent CLI
with a prompt, never the network, never `sudo`, nothing installed globally. No `cargo update`.
Track agents don't commit; the lead merges.
