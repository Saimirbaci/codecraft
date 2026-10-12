# Claude Code IDE protocol: the facts CodeCraft relies on

CodeCraft's IDE server (`codecraft-mcp` + `engine/src/ide_server.rs`) lets the `claude` CLI
connect to CodeCraft the way it connects to VS Code, JetBrains IDEs or Neovim. The protocol has
no official specification. These notes record what we implement and where each fact comes
from. Clean-room: we read the public documentation below and wrote our own code. We copied no
code. claudecode.nvim is MIT-licensed, and we used its documentation only as a description of
the wire format.

## Sources

- [S1] coder/claudecode.nvim, `PROTOCOL.md` (based on MCP spec 2025-03-26), "How Claude Code IDE Integration Works" (MIT):
  https://github.com/coder/claudecode.nvim/blob/main/PROTOCOL.md
- [S2] coder/claudecode.nvim README and tool documentation: https://github.com/coder/claudecode.nvim
- [S3] Anthropic, Claude Code docs, "Use Claude Code in VS Code", section "The built-in IDE MCP
  server": https://code.claude.com/docs/en/ide-integrations (redirected from
  docs.anthropic.com/en/docs/claude-code/ide-integrations)
- [S4] GitHub advisory GHSA-9f65-56v6-gxw7 / CVE-2025-52882 (Claude Code IDE extensions accepted
  unauthenticated WebSocket connections; fixed by requiring the lock file's auth token).
- [S5] The installed CLI (`claude --version` → 2.1.296; `claude --help` lists
  `--ide: Automatically connect to IDE on startup if exactly one valid IDE is available`).
- [S6] A lock file written by a JetBrains IDE on this machine (field set only; token not read).
- [S7] manzaltu/claude-code-ide.el (Emacs; GPL, read for behaviour only):
  https://github.com/manzaltu/claude-code-ide.el
- [S8] Observable behaviour of the installed client (claude 2.1.296): its expectations, as
  recorded by a research pass that read the binary's strings (facts only; no code used).

## Discovery: the lock file [S1][S3][S6]

- Path: `~/.claude/ide/<port>.lock`, or `$CLAUDE_CONFIG_DIR/ide/<port>.lock` when that variable
  is set.
- Content (JSON):
  ```json
  { "pid": 12345, "workspaceFolders": ["/abs/project"], "ideName": "CodeCraft",
    "transport": "ws", "runningInWindows": false, "authToken": "<uuid v4>" }
  ```
  The token is opaque to the client: claudecode.nvim uses 32 hex characters, and CodeCraft uses
  a random UUID v4. `runningInWindows` comes from [S6]; [S1] doesn't list it.
- Claude Code lists the IDEs whose `workspaceFolders` contain its working directory. Its
  working directory is symlink-resolved, so CodeCraft also writes the canonical project path
  when it differs. `/ide` lets the user pick one; `--ide` (or the env vars below) connects
  automatically.
- The IDE writes the file when it starts and deletes it when it stops. CodeCraft also writes it
  owner-only (0600 in a 0700 directory) because it holds the token. It removes stale
  CodeCraft lock files whose `pid` is dead (checked with `kill -0` on Unix and `tasklist` on
  Windows). It never removes other IDEs' files, because their pid may belong to another PID
  namespace that shares the directory, such as a container or WSL.

## Transport and authentication [S1][S3][S4]

[S3] says: "The server binds to `127.0.0.1` on a random port in the range 10000–65535 … writes
it to a lock file at `~/.claude/ide/<port>.lock`, and the CLI must present it as the
`X-Claude-Code-Ide-Authorization` header … The lock file has `0600` permissions in a `0700`
directory … If `CLAUDE_CONFIG_DIR` is set, the lock file is written to
`$CLAUDE_CONFIG_DIR/ide/` instead." The CLI names this MCP server `ide`. Only
`getDiagnostics` (and `executeCode`) are visible to the model, as `mcp__ide__getDiagnostics`.
The other tools are internal RPC the CLI uses for its own UI: diffs, selections, saving.


- WebSocket server on loopback only. [S1] says to bind `127.0.0.1` on a random port (10000–65535). CodeCraft binds `127.0.0.1:<OS-chosen port>` and, when
  possible, `[::1]` on the same port.
- The client sends the lock file's token in the `x-claude-code-ide-authorization` HTTP header of
  the upgrade request. A missing or wrong token is rejected with HTTP 401 before the upgrade.
  CodeCraft compares the token in constant time.
- Subprotocol: if the client offers `mcp` in `Sec-WebSocket-Protocol`, the server echoes it.
- Each WebSocket text message carries one JSON-RPC 2.0 message.

## MCP methods [S1]

| Method | Server behaviour |
|---|---|
| `initialize` | `{protocolVersion, capabilities: {tools: {listChanged: true}, logging: {}}, serverInfo: {name: "CodeCraft", version}}`. CodeCraft echoes the client's `protocolVersion` if it knows it (2024-11-05, 2025-03-26, 2025-06-18, 2025-11-25), else offers 2025-06-18. |
| `notifications/initialized` | Marks the client ready. |
| `tools/list` | `{tools: [{name, description, inputSchema}]}` |
| `tools/call` | `{name, arguments}` → `CallToolResult` `{content: [{type: "text", text}], isError?}`. An unknown tool gets JSON-RPC error -32602. |
| `ping` | `{}` |
| `prompts/list`, `resources/list`, `resources/templates/list` | Empty lists. |
| `notifications/cancelled` | Withdraws a pending `openDiff` (its card disappears). |

Tool data is returned as one text item that holds JSON, the way the reference IDEs do it.

## Tools [S1][S2]

| Tool | Arguments | Result |
|---|---|---|
| `openFile` | `filePath`; optional `preview`, `startText`, `endText`, `selectToEndOfLine`, `makeFrontmost` (default true). CodeCraft also accepts `startLine`/`endLine` (1-based, inclusive), which the VS Code schema doesn't have. | `"Opened file: <path>"`. With `makeFrontmost: false`, JSON `{success, filePath, languageId, lineCount}` and the user's tab keeps focus. |
| `openDiff` | `old_file_path`, `new_file_path`, `new_file_contents`, `tab_name` | **Blocking**: it answers only once the user decides. Accept → two text items `"FILE_SAVED"` and the final contents. Reject → `"DIFF_REJECTED"` and `tab_name`. After FILE_SAVED, Claude Code writes the file itself and then calls `close_tab` [S2: `diff.lua`, which blocks the editor's own write]. CodeCraft writes the same content first, through the open document so it is undoable, so Claude's write changes nothing. |
| `getCurrentSelection` | none | `{success: true, text, filePath, fileUrl, selection: {start: {line, character}, end: {…}, isEmpty}}`. With no editor, `{success: false, message: "No active editor found"}`. |
| `getLatestSelection` | none | The same shape, with the last known selection when no editor is active. With none, `{success: false, message: "No selection available"}`. |
| `getOpenEditors` | none | `{tabs: [{uri, isActive, label, languageId, isDirty, isPinned, fileName}]}` |
| `getWorkspaceFolders` | none | `{success: true, folders: [{name, uri, path}], rootPath}` |
| `getDiagnostics` | optional `uri` | `[{uri, diagnostics: [{message, severity, range: {start, end}, source, code?}]}]`. Severity is a string: `"Error"`, `"Warning"`, `"Info"` or `"Hint"` [S1][S2: `tools/get_diagnostics.lua`]. Positions are zero-based with UTF-16 columns. |
| `checkDocumentDirty` | `filePath` | `{success: true, filePath, isDirty, isUntitled}`, or `{success: false, message: "Document not open: …"}` |
| `saveDocument` | `filePath` | `{success, filePath, saved, message}` |
| `close_tab` | `tab_name` | `"TAB_CLOSED"`. This also withdraws a pending diff with that tab name: the user answered in the terminal instead. |
| `closeAllDiffTabs` | none | `"CLOSED_<n>_DIFF_TABS"` |

CodeCraft does not offer `executeCode`, which runs code in a Jupyter kernel.

All positions are zero-based. `character` counts UTF-16 code units, as in LSP and VS Code.

## Notifications from the IDE [S1]

- `selection_changed`: `{text, filePath, fileUrl, selection: {start, end, isEmpty}}`. It is sent
  when the active editor's primary selection changes and has been stable for 100 ms. A newly
  initialized client also receives it.
- `at_mentioned`: `{filePath, lineStart, lineEnd}` (zero-based lines). It is sent by the
  `ide.sendSelection` command and adds the file or range to Claude's prompt.

## What the client does with the tools [S8]

- At the start of each query the CLI calls `closeAllDiffTabs` and ignores errors.
- Before an edit it calls `getDiagnostics {uri}` with a 500 ms timeout and checks that `[0].uri`
  is the URI it sent. CodeCraft therefore echoes the requested URI verbatim and returns an
  empty `diagnostics` list for a file without problems; it never errors there. After the
  edit, the CLI calls `getDiagnostics {}` with a 2 s timeout.
- `openDiff` has no client timeout. The CLI reads `content[1].text` after `FILE_SAVED`, applies
  the proposed contents unchanged when `content[0]` is `TAB_CLOSED`, drops the edit on
  `DIFF_REJECTED`, and then always calls `close_tab {tab_name}`.
- Only `getDiagnostics` and `executeCode` reach the model. The CLI never calls `openFile`,
  `getOpenEditors` and the other tools; CodeCraft offers them for parity with VS Code.
- `selection_changed`: the client reads `selection.start` and `selection.end`, `text` and
  `filePath`.
- `at_mentioned`: `lineStart`/`lineEnd` are optional and zero-based.
- After `initialize`, the client sends an `ide_connected {pid}` notification, which CodeCraft
  ignores.
- The client sends `protocolVersion` 2025-11-25 and accepts 2025-11-25, 2025-06-18,
  2025-03-26, 2024-11-05 and 2024-10-07.
- The client connects to the literal `ws://127.0.0.1:<port>`; `CLAUDE_CODE_IDE_HOST_OVERRIDE`
  changes the host.
- A lock file is valid when the port equals `CLAUDE_CODE_SSE_PORT` or the working directory is
  inside a workspace folder. In an IDE terminal (`FORCE_CODE_TERMINAL`, or a known
  `TERM_PROGRAM`), the lock's pid must also be an ancestor of `claude`, unless the port matches
  `CLAUDE_CODE_SSE_PORT`. The client deletes lock files whose pid is dead.

## Terminals the IDE launches [S1][S3][S7][S8]

- `CLAUDE_CODE_SSE_PORT=<port>`: `claude` connects to this port, finding the token in
  `<port>.lock`.
- `FORCE_CODE_TERMINAL=true`: the terminal counts as an IDE terminal, so `claude` connects
  automatically (claudecode.nvim and claude-code-ide.el set it).
- `ENABLE_IDE_INTEGRATION=true`: the older switch from [S1]. Current clients ignore it, but it
  is harmless.
- `NO_PROXY`/`no_proxy` with `127.0.0.1,localhost` appended, so a configured HTTP proxy
  doesn't intercept the WebSocket.
- [S3]: in the IDE's integrated terminal, `claude` "automatically integrates with your IDE";
  in an external terminal, run `/ide`. `claude --ide` connects at startup when exactly one valid
  IDE is available [S5].

`IdeServerState::env_for_child()` returns all of these while the server runs.
