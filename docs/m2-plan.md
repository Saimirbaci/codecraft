# M2: Polyglot, debugger, databases, agents, feedback

Status: in progress (started 2026-10-12). Owner of the integration: the lead. Tracks run in
parallel; each owns its files (see "File ownership") and talks to the rest only through the
contracts below.

## Why

CodeCraft is no longer Rust-only. Its users write **Rust, C/C++, Python, Zig and TypeScript**,
explore **databases** (DataGrip-like), and spend most of their editor time **debugging**. They
also work with **coding agents** (Claude Code, Codex, Grok, Gemini) and want to choose how much
an agent may do on its own. And CodeCraft itself should be improvable by drawing on it
(Evolvo-style feedback → agent → review).

## Tracks

| Track | Crate(s) | Engine | UI |
|---|---|---|---|
| A Language platform (lead) | `lsp` (discovery), `syntax` enum | `servers.rs` (multi-server LSP manager), run configs for every ecosystem, process supervision | status bar per-server state |
| B Debugger | `dap` (new, L0) | `debug.rs`, `debug_cmds.rs` | `debug_panel.rs`, gutter breakpoints + execution line in `editor.rs` |
| C Database | `db` (new, L0) | `database.rs`, `database_cmds.rs` | `database_panel.rs` |
| D Agents | `agent` (new, L0) | `agents.rs`, `agent_cmds.rs` | `agent_panel.rs`, `approvals_ui.rs`, `diff_view.rs` |
| E IDE server | `mcp` (new, L0) | `ide_server.rs`, `ide_cmds.rs` | (approvals via track D's cards) |
| F Feedback | — | `feedback_cmds.rs` | `feedback.rs` |
| G Syntax | `syntax` | — | — |

## Shared contracts (already in the tree)

- `codecraft_syntax::Language` has `C, Cpp, Python, Zig, TypeScript, JavaScript, Sql` (ids,
  extensions, comment tokens). Track G fills in their lexers.
- `ToolWindowId::{Debug, Database, Agent}` exist (Debug bottom; Database and Agent right) with
  toggles `view.toolWindows.{debug,database,agent}` and stub panels.
- `Session` has `approvals: Approvals`, `debug: DebugState`, `db: DbState`,
  `agents: AgentsState`, `ide: IdeServerState`; `Session::pump` calls each track's
  `pump(&mut Session) -> bool`, `Session::shutdown` each track's `shutdown`.
- Each track registers commands from its own `*_cmds.rs::specs()` (already listed in
  `commands.rs`). Command ids for observed menu items must be the catalog's ids.
- **Approvals** (`engine/src/approvals.rs`): background threads hold an `ApprovalSink` and call
  `ask(source, title, ApprovalKind)`; the request appears in `Session::approvals`; the UI shows
  it (track D draws the cards) and the user's `Decision` returns through a channel. `AgentMode`
  is `Auto | Ask | Review`. Every agent integration uses this; nobody blocks the UI thread.
- **Agent jobs contract** (track D implements, track F calls):
  `agent.start {backend: "claude"|"codex"|"grok"|"gemini", mode: "auto"|"ask"|"review",
  prompt: string, attachments?: [path], cwd?: path, title?: string}` → `{job: int}`.
- UI stubs: `debug_panel::show`, `database_panel::show`, `agent_panel::show`,
  `approvals_ui::show` (drawn every frame), `feedback::overlay` (drawn last every frame).

## Rules for every track

- Read `AGENTS.md` (never panic, no unsafe, tests, layering, clean-room).
- New L0 crates depend on nothing in the workspace; register them in `xtask/src/main.rs`
  `layer_of()` (lead does this when creating the crate) and add them to the workspace
  `[workspace.dependencies]`.
- Child processes: put them in their own process group (`CommandExt::process_group(0)`), kill
  the group on stop and in `shutdown`, never leave a process behind (the user has seen
  orphaned language servers in other editors).
- Tests that need an external tool (lldb-dap, debugpy, postgres, an agent CLI) skip cleanly when
  it is missing. Never call paid APIs from tests: agent tests use fake CLIs (shell scripts that
  print recorded stream-json).
- Use your own `CARGO_TARGET_DIR=target-<track>`; never run `cargo update`.
