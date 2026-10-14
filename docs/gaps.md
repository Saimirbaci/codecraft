# Gaps

Known shortfalls against RustRover, ranked. Update this list when you close one (with the date).

## High

1. ~~No debugger~~ (landed in M2: lldb-dap for Rust/C/C++/Zig, debugpy for Python). Remaining:
   no memory/disassembly views; js-debug for Node/TS only when the user installs it.
   (2026-10-13: Rust values read like RustRover's: `&str`/`String` show their text and `Vec`
   `size=N`, with the toolchain's LLDB formatters (or a sibling toolchain's when missing) and the
   pointer dropped from pointer+summary values; Run › Attach to Process… (Alt+Shift+F5) lists
   processes from `ps` (filter, all users) and attaches with lldb-dap (native/Rust) or debugpy
   (Python); Stop detaches.) Python attach depends on debugpy's process injection (not tested
   end to end); closing the window kills the adapter rather than detaching first.
2. ~~No VCS~~ (landed in M3). ~~3-way merge view, per-hunk staging, worktrees, GitHub~~ (X,
   2026-10-13: Stage/Unstage/Rollback in the gutter change popup (index-only, the work tree is
   untouched); a three-way merge window for conflicted files (yours/base/theirs from the index
   stages, non-conflicting changes applied up front, Accept Left/Right/Both/Ignore per change or
   for all, Apply writes the result and marks it resolved); Git › New Worktree… and Worktrees…
   (list, open as project, remove/force remove); GitHub through the `gh` CLI when installed: your
   repositories in the Clone dialog (also File › New › Project from Version Control, `owner/repo`
   shorthand), Create Pull Request… (pushes the branch first), install hint when `gh` is
   missing). Remaining: the merge view is per-chunk cards with a result preview, not an editable
   3-pane editor; the change popup doesn't show whether a hunk is already staged; no View Pull
   Requests, Sync Fork, Share on GitHub or GitLab API features (GitLab clone is plain git).
3. ~~No terminal tool window~~ (landed in M3, with `claude` launched already connected).
4. ~~Highlighting is lexical only~~ (M3: semantic tokens with mutable underline and unresolved
   references, inlay hints for types and parameters). ~~Parameter-name hints are not clickable;
   hints are not shown in non-active split views~~ (M10, 2026-10-13: semantic tokens and inlay
   hints are requested for the selected tab of every visible split group, with settling and
   in-flight tracking per document; hidden tabs never ask. Inlay label parts keep their tooltips
   and locations: hovering shows the tooltip, Cmd+click goes to the part's location (or, for a
   type hint without one, to the type's declaration); `editor.inlayHints` lists them for
   agents). Remaining: no `inlayHint/resolve` (rust-analyzer sends locations eagerly); hints are
   requested for the whole file, not the visible range.
5. ~~Full-file re-lex on every edit (and full-text LSP sync)~~ (M4, 2026-10-13: Rust highlighting
   is incremental (re-lex and re-classify a window around the edit, exactly equal to a full lex;
   ~17x faster per keystroke on a 10k-line file) and folds reuse its tokens; documents sync to
   servers that ask for it with ranged `didChange` from the document's change log). Remaining:
   other languages still re-lex the whole file per edit (their lexers use file-wide context, e.g.
   C `#define`s, Markdown setext headings); the editor still copies the text once per revision.
6. ~~macOS keymap only~~ (M8, 2026-10-13: Settings › Keymap selects `macOS` or `Windows/Linux`
   (RustRover's Ctrl-based defaults from JetBrains' public reference, `Cmd` → `Ctrl` otherwise;
   the platform's keymap by default); the keymap editor adds shortcuts by recording a key press,
   removes them, warns about conflicts (Remove Other Assignments / Leave) and resets; dispatch,
   menus, tooltips and Find Action follow the active keymap and the user's overrides).
   Remaining: no keymap copies/inheritance (overrides are per built-in keymap); no two-stroke
   (chord) shortcuts or mouse shortcuts; RustRover's other keymaps (Emacs, VS Code…) are not
   provided.

## Medium

7. ~~In-window menu bar; RustRover on macOS uses the global NSMenu bar~~ (M9, 2026-10-13: the menus
   live in the macOS global menu bar through `muda`, built from the same catalog as the in-window
   bar, with live enabled states, checkmarks for the View toggles, native key equivalents and the
   system's Services/Hide/Minimize/Zoom; Settings › Appearance switches back to the in-window bar.
   Project windows: File › Open asks This Window / New Window, Next/Previous Project Window,
   Help › Help; `cargo xtask bundle|dmg|package-linux|package-windows`). (2026-10-13: project
   windows open at exit are reopened on start with the main window's last project, each with
   its editors; Settings › Appearance & Behavior › System Settings › Reopen projects on startup,
   `codecraft --no-restore`.) Remaining: Merge All
   Project Windows (macOS window tabs) is not implemented; window positions and sizes of the extra
   project windows are not restored (they open at the main window's size); plain-key and Alt+character shortcuts (Backspace, Tab, Alt+/) are not native key
   equivalents, so the native menu shows no shortcut for them; enabled states refresh every 250 ms
   instead of when a menu opens; no Developer ID signing or notarization in CI; the Windows zip task
   is untested on Windows.
8. ~~No split editors, editor tab groups~~ (M4, 2026-10-13: Split Right/Down, Split and Move,
   Unsplit/Unsplit All, Next/Previous Splitter, Change Orientation, Maximize/Normalize, Stretch
   Editor, draggable splitters, a tab bar per group, tabs dragged between groups, several views
   of one file whose carets follow each other's edits). Remaining: no detachable tool windows or
   editor windows. ~~Keep Tab Open (no preview tab) and Show Hidden Tabs are not implemented~~
   (M10 track F, 2026-10-13: a single click in the Project view opens an italic preview tab
   that the next one replaces until it is edited, double-clicked or kept; Show Hidden Tabs lists
   the group's tabs; Settings › Editor › Editor Tabs: show tabs, preview tab, tab limit).
   (2026-10-13: the split layout, each group's tabs with carets, scroll, folds and pinned state,
   the focused group and the tool-window layout are saved per project in
   `.codecraft/workspace.json` (the config directory when the project is read-only) on close,
   project switch and every 30 s while they change, and restored when the project opens;
   `session.save`/`session.restore`/`session.state` for agents.) Remaining: unsaved buffers'
   text is not restored (Quit still prompts); untitled buffers and scratch files without a path
   are not restored; per-tab soft-wrap/column-mode state is not saved.
9. ~~Completion: no snippet placeholders (snippets are flattened), no completion item resolve,
   no auto-popup tuning (RustRover's delays and fuzzy ranking)~~ (M10, 2026-10-13: LSP snippets
   (`$1`, `${2:x}`, nesting, mirrors, `${3|a,b|}` choices in a chooser, `$0`, `TM_*`/`CLIPBOARD`
   variables with regex transforms, escapes) start a live template session: Tab/Shift+Tab walk the
   placeholders and mirrors are edited together; multi-line items take the line's indentation;
   the selected item is resolved after 80 ms (documentation beside the list, auto-import
   `additionalTextEdits` applied in the accept's undo step, also when the answer comes after the
   accept); ranking is case-insensitive prefix, then camel humps (`hm` → `HashMap`, `gm` →
   `get_mut`), then humps from a later word, each tier by the server's `sortText`, matched chars
   highlighted, `preselect` honoured; the auto-popup uses the server's trigger characters
   (`.`/`::` for Rust) after a delay (Settings › Editor › Code Completion: on/off, delay, argument
   placeholders), re-requests incomplete lists and drops stale answers; Type-Matching
   (Ctrl+Shift+Space) keeps items of the expected type from `let x: T =` or the parameter being
   passed (signature help), else ranks by relevance and says so). Remaining: no commit characters,
   no "smart" type inference beyond those two sources (return position, struct fields), no
   second-press (Ctrl+Space twice) widening, no per-language auto-popup settings, no transforms on
   tab stops (`${1/…/…/}` mirrors stay plain), nested snippet sessions replace the outer one.
10. ~~Rename has no preview or conflict dialog~~ (M7, 2026-10-13: multi-file renames, code actions and
    file moves open the refactoring preview with per-file checkboxes and diffs; `prepareRename` and
    rename errors show "Cannot Rename"). ~~Rename is a popup, not RustRover's in-place inline
    rename~~ (M10, 2026-10-13: Shift+F6 edits the name in the editor with the file's occurrences
    (`documentHighlight`) framed and mirrored; Enter (or Tab, or clicking elsewhere) renames
    through the server from the original text as one undo step, multi-file results open the
    preview; Escape restores; the popup remains when `prepareRename` gives no range or with
    Settings › Editor › General › Rename in the editor off; `refactor.rename {newName}` stays
    headless). Remaining: name conflicts are only those rust-analyzer reports; occurrences in other
    open files are not mirrored live; no "rename comments and strings" options.
11. ~~Run configurations are derived only~~ (M8, 2026-10-13: Run › Edit Configurations… lists the
    project's derived configurations and user ones (Cargo command or any command line, env vars,
    working directory), creates/copies/deletes/edits them with Apply/OK/Cancel and saves them in
    `.codecraft/run-configurations.json`; `runConfig.*` commands do the same headlessly).
    2026-10-13: + Add offers Cargo, Command, Python, C/C++ (compiled with cc/c++ then run), Zig
    and Npm configurations; every configuration can load a `.env` file (re-read on each run;
    explicit variables win); `codecraft-cli configs <project> list|add|update|copy|delete|run`
    lets agents manage them without JSON.
    Remaining: no before-launch tasks, no templates, no "store as project file" choice, no
    per-configuration debugger options.
12. ~~Test results show as a summary + console~~ (M6, 2026-10-13: test tree beside the Run
    console for cargo/libtest, pytest (live `-v` lines + JUnit report), CTest and `zig test`, with
    per-test output, failure points and navigation, Rerun Failed Tests, Show Passed/Ignored, sort by
    duration, previous/next failed, test history in `.codecraft/test-history.json`, export/import
    (JSON, JUnit XML), Run with Coverage via `cargo llvm-cov` / pytest-cov, Coverage tool window,
    gutter coverage marks, Manage Coverage Reports). Remaining: libtest prints no per-test
    durations on stable (only with `--report-time`, nightly), so cargo tests show suite durations
    only; Rust test locations come from a module-path walk, not rust-analyzer runnables; no
    jest/vitest parser; `zig build test` cannot rerun only the failed tests; no branch coverage
    view, and the coverage table has no sorting.
13. ~~Settings dialog covers a handful of options~~ (M8, 2026-10-13: searchable tree with
    Appearance (theme Dark/Light/High Contrast, UI font size), Keymap (editor), Editor
    (General, Font, Color Scheme with a token editor for user schemes, Code Style, Live
    Templates list), Languages & Frameworks › Rust, Version Control › Commit Message Generation
    and Tools › AI Agents; Apply/OK/Cancel over an engine-side draft; `settings.get/set/list/
    reset` validate keys and values; View › Quick Switch Scheme (Ctrl+`) switches scheme, keymap
    and theme). Remaining: Code Style has only the tab size; no font family choice; Live
    Templates are read-only in Settings; ~~show-whitespace is a setting the editor does not paint
    yet~~ (M10, 2026-10-13: spaces as dots and tabs as arrows in the `whitespace` colour token;
    `whitespace_mode` all / leading_trailing / trailing; View › Active Editor › Show
    Whitespaces); colour schemes cannot be imported/exported as files.
14. ~~No file watching~~ (M3: FSEvents/inotify watcher reloads unmodified files and refreshes the tree and Git).
15. ~~Reformat with rustfmt replaces the whole document~~ (M4, 2026-10-13: only changed lines are
    replaced and carets keep their token through whitespace changes).
16. ~~Lazily filled RustRover submenus (Tools › Rust, Edit › Find Usages, Code › Folding…) were not
    captured by the menu observation~~ (2026-10-13: captured, and every core item is live; see 27).
    Still empty: View › Active Editor, Git › Current File and Run › Test History (they need an
    open editor or test runs in RustRover), so their items stay in the shell's EXTRA table.

## Low

17. ~~No IME pre-edit (composition) rendering; committed text works~~ (M10, 2026-10-13: the
    pre-edit is drawn underlined at the caret without touching the document, the caret follows
    the IME's active range, the commit is typed as one edit, and the IME cursor area is the
    caret so the candidate window opens there). Remaining: `DeleteSurrounding` is ignored.
18. ~~Project view lacks drag and drop, delete and rename operations~~ (M7, 2026-10-13: Rename/Move/
    Copy/Safe Delete in the context menu and on F6/F5/Cmd+Backspace, drag and drop onto directories,
    `.rs` renames update `mod` declarations through rust-analyzer). Remaining: External Libraries;
    moving a `.rs` file to another directory does not move its `mod` declaration.
19. No macros. ~~Live templates~~ (M3b, 2026-10-13: Tab expansion, Cmd+J, Surround With, Save as
    Live Template, user templates in `~/.codecraft/templates.json`). ~~No bookmarks, TODO tool
    window or local history~~ (X, 2026-10-13: bookmarks with F3, mnemonics with Alt+F3 (0–9, A–Z,
    Ctrl+digit to jump), Cmd+F3 popup, next/previous in the file, descriptions, gutter marks, the
    Bookmarks tool window, following edits through the document's change log, saved in
    `.codecraft/bookmarks.json`; the TODO tool window (TODO/FIXME in comments by the language's
    lexer, ignore rules respected, scanned on a worker thread, open files live while it is shown,
    files changed on disk rescanned); File › Local History (Show History, for Selection, Project
    History, Recent Changes, Put Label…) with snapshots on save and on external change in
    `.codecraft/local-history` (5 days, 64 MB, 2 MB per file), a diff view and Revert (undoable
    when the file is open); scratch files (Shift+Cmd+N, language chooser, File › Scratches) in
    `$CODECRAFT_HOME/scratches`). Remaining: the TODO patterns and filters are not configurable;
    bookmarks are lines only (no file/folder bookmarks, groups or drag-and-drop ordering);
    bookmarks of files renamed outside CodeCraft are lost; local history does not track
    renames/deletions or directory-level history, and times show as relative or UTC; scratch
    files are not shown in the Project view (Scratches and Consoles).

## M2 follow-ups (2026-10-12)

20. Ask mode for Claude Code and Codex needs the `claude-code-acp` / `codex-acp` adapters (not
    installed here); Gemini and Grok speak ACP natively. Grok's `streaming-messages-json` stream
    format is parsed leniently (no recorded sample yet).
21. ~~Gutter ▶ run/debug icons exist only for Rust~~ (2026-10-13: C/C++ `main`, Python
    `if __name__ == "__main__":` and pytest tests in test files, Zig `pub fn main` and `test`
    blocks, jest/vitest `test`/`it`/`describe` in `*.test.*`/`*.spec.*`/`__tests__` files,
    running vitest/jest (`-t name`) or `node --test`). Remaining: the icon runs on click (no
    Run/Debug menu); JS/TS tests cannot be debugged from the gutter; C `main` detection is a line
    scan.
22. Python and TypeScript language servers are not installed on this machine
    (`pip install basedpyright`, `npm i -g typescript-language-server typescript`); CodeCraft
    shows install hints and keeps highlighting/run working.
23. ~~Database: no in-grid editing yet; TLS for PostgreSQL is not supported~~ (2026-10-13: tables
    with a primary key are editable in the grid: edit a cell in place (double-click/Enter/F2, Set
    NULL), add and delete rows, pending changes highlighted, Revert, Preview SQL, Submit as one
    transaction (a stale row rolls everything back) and reload, all through `db.editCell`/
    `db.addRow`/`db.deleteRows`/`db.revertChanges`/`db.previewChanges`/`db.submitChanges`/
    `db.getChanges`; PostgreSQL TLS via the platform TLS stack with `sslmode`
    disable/prefer/require/verify-ca/verify-full, `sslrootcert` and client certificates
    (`sslcert`/`sslkey`, PKCS#8 PEM)). Remaining: no PostgreSQL TLS on Linux (needs OpenSSL), no
    MySQL TLS; console query results are read-only (only Open Table results are editable);
    values are sent as escaped literals, not bind parameters.

## M3b editor depth (2026-10-13)

24. Soft wrap, column selection (Shift+Cmd+8, Alt+drag), breadcrumbs, the navigation bar
    (Cmd+Up), live templates, Unwrap/Remove, Complete Current Statement, Auto-Indent Lines,
    Move Element Left/Right, Next Edit Location and Cyclic Expand Word landed. ~~Wrapped rows do
    not keep the line's indentation; box selection has no virtual space~~ (M10, 2026-10-13:
    continuation rows are indented by the line's leading whitespace, at most half the row width,
    with caret motion, clicks and painting agreeing; box carets past short line ends are drawn
    at their virtual column and typing pads those lines with spaces in one undo step. Custom
    folding regions (`// region`/`// endregion`, `#region`, `<editor-fold>`, `#pragma region`)
    fold like other ranges, and Navigate › Custom Folding… (Alt+Cmd+.) lists them). Remaining:
    Backspace/Delete in virtual space and Shift+arrows past line ends without column mode; no
    "use original line's indent" setting (always on); live template variables have no expressions (`completeSmart()`,
    `suggestVariableName()`); breadcrumbs and Unwrap/Remove are lexical (no LSP document
    symbols); Auto-Indent Lines covers brace languages only; Optimize Imports for Rust
    (2026-10-13) sorts single-line `use` groups and applies rust-analyzer's "Remove all unused
    imports" over the file, but does not merge imports; no live template settings page (edit
    `templates.json`).

## M10 menu sweep (2026-10-13)

25. The cheap remaining menu items are live (track F): View › Tool Windows › Database Changes
    (pending grid edits of every open table with Submit/Revert per table and for all);
    Recently Changed Files (modified buffers, then files by their latest Local History
    snapshot) and Recent Changes (Alt+Shift+C); Compare With… (file chooser or Browse…, side-by-
    side diff) and Compare with Clipboard (selection or file); Quick Type Definition (LSP
    `typeDefinition` in a popup); Context Info (Ctrl+Shift+Q: header lines of the enclosing
    items); Run › Stop Background Processes… (Shift+Cmd+F2: runs, debug session, language
    servers, agents, database queries, Git operation), Show Running List, Window › Processes ›
    Show/Auto Show; File Properties › File Encoding (UTF-8, UTF-16 LE/BE, ISO-8859-1 detected
    on open, kept on save, Reload or Convert; the status bar shows it), Add/Remove BOM (the BOM
    survives saves), Make file read-only (toggles the file's write bits and the editor),
    Associate with File Type… (glob → language, in settings); Power Save Mode (no completion
    auto-popup, semantic tokens/inlay hints, check on save or live TODO scan; status bar badge);
    Export › Files or Selection to HTML… and Print… (highlighted HTML in the theme's colours,
    opened in the browser to print); New › Python Package; New Projects Setup (opens Settings /
    Edit Configurations); Edit › Fill Paragraph (comments and plain text, Settings › Code Style ›
    Hard wrap at, default 100); Git › GitHub/GitLab › Manage Accounts… (`gh`/`glab auth status`
    with a login hint; login is never run). Remaining: other save paths (agent and refactoring
    writes, the IDE server) still write UTF-8 without a BOM; UTF-16 without a BOM is detected
    only for mostly-ASCII text; no other encodings (Shift-JIS, Windows-1252…); a language
    association does not move an already-open file to a different language server until it is
    reopened; Print has no page setup (the browser prints); the tab limit closes the leftmost
    tab, not the least recently used; menus show no checkmarks for Power Save Mode / Auto Show;
    Compare with Clipboard reads the system clipboard on macOS only (elsewhere CodeCraft's own
    last copy); Settings for New Projects edits the global settings (there are no per-project
    defaults).

## M10 inspections (2026-10-13)

26. Code › Inspect Code…, Code Cleanup…, Analyze Stack Trace or Thread Dump… and Rearrange Code
    are live (track D). Inspect Code asks for a scope (whole project, uncommitted files from the
    Git status, the current file or its directory; `{scope, path, tools}` for agents) and runs
    on a worker thread (Stop kills the process group): Rust through `cargo clippy
    --message-format=json --all-targets` (cargo check when Settings › Rust turns Clippy off, or
    when Clippy is not installed, with a hint), plus ruff, ESLint (project `node_modules/.bin`
    first), clang-tidy (`-p` with a `compile_commands.json`) and `zig fmt --check` when installed
    and the scope has files for them. View › Tool Windows › Inspection Results: a tree by
    inspection (most severe first) → file → problem with severity filters, Rerun/Stop, a
    description panel (rustc's rendered diagnostic, the lint's docs link), double-click to jump.
    Quick-fixes come from rustc/Clippy suggestions (multi-span; maybe-incorrect ones marked),
    ruff fixes (safe/unsafe), ESLint fixes and suggestions, and `zig fmt` (whole-file reformat);
    they become an LSP `WorkspaceEdit` applied through the refactoring preview (single-file fixes
    at once, "Fix all 'lint' problems" and Fix All previewed; overlapping fixes are left for a
    rerun; files edited since the inspection refuse fixes; the other problems of a fixed file
    move with the edit). Suppress: `#[allow(lint)]` on the enclosing Rust item, `# noqa: CODE`,
    `// eslint-disable-next-line rule`, `// NOLINT(check)`. Code Cleanup applies every
    machine-applicable fix of the scope, then rustfmt / ruff format / zig fmt, as one preview.
    Analyze Stack Trace (paste box prefilled from the clipboard) recognizes Rust panics and
    backtraces, Python tracebacks, Zig and sanitizer traces and JavaScript stacks; file locations
    are links resolved against the project, library frames dimmed. Rearrange Code sorts a Rust
    file's top-level items (use, mod declarations, macro_rules!, consts, statics, types, traits,
    impls, fns, inline mods) in the preview. Remaining: RustRover's own inspections (its
    inspection profiles, severities editor, "Inspect Code" for a custom scope, offline/exported
    results) are not reproduced: only external linters run; clippy always checks the whole
    workspace (results are filtered to the scope); the Rust suppression is a line-based heuristic
    (innermost item by indentation); clang-tidy fixes (`--export-fixes`) are not read; findings
    are not shown as editor highlights; Code Cleanup has no profile of what to apply; Rearrange
    Code does not reorder items inside `impl`/`mod` bodies and refuses files with item-level
    macro invocations; Analyze Stack Trace has no thread-dump view (Java-style dumps are plain
    text) and opens in its own window instead of a Run tool window tab.

## M10 newly observed menus (2026-10-13)

27. The items of RustRover's lazily filled submenus are live (track G); core menu parity is
    192/192. Commands that existed under our own ids now use the catalog id and keep the old one
    as an alias (`commands::ALIASES`; `Registry::get`, `execute`, `enabled`, shortcuts and saved
    keymap overrides resolve it): `edit.paste` → `edit.paste.paste`, `edit.bookmarks.next/
    previous/show/gotoMnemonic` → `…nextLineBookmark/previousLineBookmark/showLineBookmarks/
    goToMnemonic`, `run.debuggingActions.resume/pause` → `resumeProgram/pauseAllPrograms`,
    `code.folding.toggle` → `toggleFolding`, `code.reformat{File,CargoProject}WithRustfmt` →
    `tools.rust.…` (the Code menu's items resolve to them), `view.hideAllToolWindows` →
    `window.activeToolWindow.hideAllWindows`; the menus' EXTRA entries they duplicated are gone.
    New: Edit › Paste › Paste from History… (a 100-entry ring of copies, cuts and outside pastes
    in a chooser at the caret) and Paste as Plain Text (never whole-line); Edit › Find Usages ›
    Show Usages (popup), Find Usages in File and Highlight Usages in File (`documentHighlight`,
    else the identifier's whole-word occurrences; highlights stay until Escape or an edit),
    Next/Previous Highlighted Usage, Recent Find Usages (the last 10 searches) and Find Usages
    Settings… (project or current file, include test code; applied to Find Usages);
    View › Appearance: Presentation Mode (editor font `presentation_font_size`, bars and tool
    windows hidden, full screen), Distraction Free Mode, Full Screen, Zen Mode, Compact Mode,
    Zoom IDE… (whole-UI zoom 50–300% with a −/+/Reset popup), Toolbar, Navigation Bar (Top, In
    Status Bar, Don't Show, Show Members), Tool Window Bars, Status Bar and the 17 Status Bar
    Widgets (persistent toggles; painted: status text, Rust external linter, line:column, language
    services, line separator, encoding, Power Save, column selection mode, indentation, Git branch,
    read-only, memory indicator); menus show checkmarks and "Exit …"/"Current: N%" labels in both
    menu bars; Code › Folding: Expand/Collapse Recursively, Expand (All) to Level 1–5, Expand/
    Collapse Doc Comments, Fold Selection / Remove Region, Fold Code Block; Run › Debugging
    Actions: Resume/Pause/Stop All Programs, Force Step Over, Smart Step Into, Force Run to
    Cursor, Show Execution Point; Window › Active Tool Window › Hide Active Tool Window, Hide Side
    Tool Windows, Jump to Last Tool Window; Window › Notifications › Close First/Close All.
    Window › Merge All Project Windows is out of scope (macOS window tabs; project windows are
    egui viewports). Remaining: no inline (single-line) folds for Fold Selection, and folds stay on
    their start lines when lines are inserted above; Force Step Over / Smart Step Into / Force Run
    to Cursor do the plain step (DAP cannot ignore breakpoints, lldb-dap has no step-in targets);
    File System Sync, Aggregator, Grid Position, JSON Schema and Rust Resolve Context widgets are
    toggles without a widget; Find Usages' test filter goes by path (not `#[cfg(test)]` modules);
    the "All Programs" actions act on the one debug session CodeCraft runs; Active Tool Window's
    tab, maximize and resize actions, Layouts, Tasks & Contexts, Reset Frame and Compile and
    Reload Modified Files are not implemented.

## M11 suite features, Docker, SSH and remote (2026-10-13)

28. Debugger depth (track A): Memory, Disassembly and Registers tabs in the Debug tool window,
    watchpoints from the Variables view, an LLDB console with history, core-dump and remote
    (`gdb-remote`) debugging from Run › Debug…, Code › Show Assembly. Xcode's lldb-dap (lldb-1600)
    lacks `readMemory`/`writeMemory`/`dataBreakpointInfo`/stepping granularity and misaligns
    `disassemble`, so CodeCraft checks the adapter's capabilities and falls back to LLDB console
    commands. Remaining: core-dump and remote debugging are not run configuration kinds; the
    console disassembly fallback shows only the current function; watchpoints last for the
    session (no conditions or hit counts); older consoles need `p`/`expr` for expressions; no
    search in the Memory view and bytes are edited through a hex field; Show Assembly has no
    source-to-asm mapping, LLVM IR/MIR views or target choice; on this machine only the console
    fallbacks run against a real adapter (native DAP paths are tested with a scripted adapter).
29. Database depth (track B): query history and saved queries, parameter prompts, Explain Plan /
    Explain Analyze trees, filter row, transposed view, value viewer, selection aggregates, Copy
    As / Export (CSV, TSV, JSON, SQL INSERT, Markdown), CSV/TSV/JSON import, data source colour,
    read-only, production confirmation, manual commit, SSH tunnels. Remaining: no DuckDB (needs a
    new dependency); the value viewer decodes PNG only; in Execute All, a positional `?1` shares
    one value across statements; plans are a tree only (no diagram, comparison or collapsing);
    MySQL/MariaDB explain parsing is tested on fixtures only; the transposed view shows the first
    1000 records read-only; import has no type conversion or "new table"; manual commit is per
    data source, not per console; the production guard classifies statements by keyword; in
    SQLite manual mode an open console transaction can make grid submits wait for the 5 s busy
    timeout.
30. HTTP Client (track H): `.http`/`.rest` files with environments, file and dynamic variables,
    gutter ▶ per request, responses and history in Services › HTTP Requests, cURL conversion both
    ways, Postman import, highlighting and completion. Remaining: response-handler and
    pre-request scripts (`> {% %}`, `< {% %}`) are shown but never run; no Auth Configuration or
    OAuth2 `$auth.token`; HTTP/1.1 only (no WebSocket, gRPC, GraphQL request types), no cookie
    jar, proxy or client certificates, and a running request can't be cancelled; the
    environment choice is global and lives in Services (no `.http` editor toolbar); `<>`
    previous-response references are skipped; Postman scripts, OAuth/digest and environment
    files aren't imported; no `.http` icon in the Project view.
31. Docker (track D): Services › Docker (contexts, containers, Compose projects, images, volumes,
    networks; start/stop/restart/pause/remove, streamed logs, inspect, exec in a Terminal tab,
    prune), Dockerfile and Docker Compose run configurations with gutter ▶, Docker as a run
    target, Dev Containers, Docker Registry login status, Dockerfile highlighting. Remaining: no
    dialogs for adding a TCP/SSH Docker connection or container options; pull/build/compose
    output appears when they finish; no per-service Compose actions or logs, no volume or
    container file browser; no registry browsing or push; Dev Containers ignore features, mounts,
    customizations and hooks other than `postCreateCommand`, and Compose-based ones need the
    `devcontainer` CLI; Dockerfile run configurations take one command field; no Dockerfile
    completion or inspections.
32. SSH and remote (track S): SSH servers from `~/.ssh/config` plus `$CODECRAFT_HOME/ssh.json`
    (keys/agent only), Start SSH Session, all of Tools › Deployment (upload, download, sync with
    preview, compare with deployed, automatic upload), File Transfer and Remote Host tool
    windows, SSH as a run target, SSH tunnels for data sources, File › Remote Development… (a
    mirrored local copy with automatic upload). Remaining: Automatic Upload shows no menu
    checkmark; the scp fallback ignores exclusions and has no sync preview; rsync/scp remote
    paths are limited to a safe character set; stopping an SSH run doesn't stop the remote
    process; tunnels connect through 127.0.0.1, so PostgreSQL `verify-full` fails its hostname
    check; the SSH run target's server is a text field; Remote Host has no drag and drop or
    multi-select; `Match` blocks in `~/.ssh/config` aren't evaluated; BatchMode means no password
    or passphrase prompts (accept host keys once via Start SSH Session); Remote Development opens
    the mirror in the current window, with no remote language server or file watching; no WSL.
33. Python (track Y): interpreter detection (venv, uv, poetry, conda, pyenv, system), a status-bar
    interpreter switcher and Create Virtualenv driving runs, debugpy, pytest and the language
    server; the Python Packages tool window; an "Install package" quick-fix; the Python Console
    with a Variables view; mypy/pyright in Inspect Code; Django/FastAPI/Flask run configurations;
    doctest runs; Create setup.py / Run setup.py Task. Remaining: doctest output isn't parsed into
    the test tree; console interrupt is Unix-only and children of console code survive Stop; no
    console completion or debug-console switching; the Variables view doesn't expand objects;
    package search filters installed packages only (no PyPI search); no pipenv, hatch, Windows
    registry, remote, WSL or Docker interpreters; the install quick-fix needs a running Python
    language server; mypy/pyright findings offer suppression only; Create setup.py has no dialog.
34. Profiler (track P): Run › Profile (samply, `perf` on Linux, py-spy for Python), Attach
    Profiler to Process…, Open Profiler Snapshot (samply/Firefox, Gecko, speedscope, collapsed
    stacks, `perf script`, DTrace; gzip), and a Profiler tool window with flame graph, call tree,
    method list and thread filter. Remaining: profiling a Cargo configuration records `cargo
    run`, including build time; the Run window's Stop sends SIGTERM, so samply may not save (the
    Profiler window's Stop works); no symbolication; snapshots are parsed whole into memory; no
    profiling on Docker/SSH targets; no timeline, memory profiling, snapshot diff or Chrome trace
    format; macOS attach needs `samply setup` and py-spy may need root; speedscope files with
    mixed units keep only the first unit's profiles.
35. SQL in Rust strings (track Q): SQL detected in sqlx/diesel/rusqlite/postgres calls and after
    `// language=SQL`, highlighted, completed from the chosen data source (alias-aware) and
    checked for unknown tables/columns; an SQL formatter for `.sql` files, consoles and injected
    strings. Remaining: no Settings page for SQL (`sql.settings`, `sql.chooseDataSource`,
    `.codecraft/sql.json`); detection is by call name, so SQL built with `format!` or
    concatenation isn't found; column checks skip subqueries, table functions and CTEs, and
    there are no quick-fixes; each edit re-scans the whole Rust file; the formatter doesn't wrap
    by width and keeps CASE and select lists inline; completion has no SQL functions or join
    conditions.
36. Tests and per-user files: `Session::isolate_home(dir)` keeps scratches, templates, profiler
    snapshots, query history and SSH state under `dir`; the panic-hunting test uses it (an early
    M11 run had written HTTP scratch files and a profiler list into `~/.codecraft`). New
    per-user files must be added there.

## Generated run configurations (2026-10-14)

37. Run › Edit Configurations › Generate proposes editable configurations for the project's
    entry points, per ecosystem and per sub-project (`backend/`, `web/`, …): Cargo binaries and
    tests; Django, FastAPI, Flask and Streamlit apps, `__main__` packages, `[project.scripts]`,
    `if __name__ == "__main__"` scripts with an entry-point name (`main.py`, `app.py`, `run.py`,
    `cli.py`, `server.py`) and pytest, never one per Python file; CMake/Meson executables, CTest,
    `make`/`make run`/`make test`, or the sources that define `main()`; `build.zig` steps or `zig
    run src/main.zig`; npm `dev`/`start`/`test`/`build`; Go `package main`; compose/Dockerfile.
    Proposals are ticked in a list (already configured ones are marked) and added to the draft.
    "Ask an agent" sends the detected list to Claude Code/Codex/Grok/Gemini (Ask mode when
    available, so anything beyond reading needs approval) and shows its JSON reply as proposals,
    after validation. The desktop app generates once when a project without
    `.codecraft/run-configurations.json` opens (Settings › Build, Execution, Deployment › Run
    Configurations). Remaining: detection is heuristic (three directory levels, file heads, no
    Python import graph); CMake binaries are assumed under `cmake-build-debug/<subdir>/` (no
    `RUNTIME_OUTPUT_DIRECTORY`); an agent in Auto mode (backends without Ask) is only told not
    to change files. Python and npm configurations that run in a sub-project use its own
    virtualenv and lockfile unless the project has a selected interpreter.

## M12 (2026-10-14)

38. Agent sessions (track AG): every agent job is saved per project under
    `$CODECRAFT_HOME/agent-sessions/<project key>/` (prompt, feed, status, cost, the backend's
    session id, review state), listed in the Agents tool window after a restart (running ones as
    Interrupted) and resumable (`claude --resume`, `codex exec resume`, ACP `session/load`, else a
    transcript seed). Remaining: two windows on one project don't see each other's live jobs (the
    other's shows as interrupted); a failed `claude --resume` (its own store cleared) isn't
    retried with the transcript; worktrees of interrupted reviews stay until resumed or deleted,
    and pruning removes old session files but not their worktrees; Gemini's `--resume` argument
    wasn't checked against a real CLI; sessions are per project root (feedback jobs on CodeCraft's
    own source appear only when that folder is open); opening/renaming reads the file
    synchronously (≤ 2 MB); no search, filter or export in History.
39. SQL editor and explorer (track DC): Go to Declaration, Quick Documentation, Find Usages,
    Rename and quick-fixes in SQL (files, consoles, Rust strings); inspections (UPDATE/DELETE
    without WHERE, INSERT count mismatch, SELECT * in views, unused alias/CTE, ambiguous column,
    unresolved names) with `-- noinspection`; `.sql` files attached to a data source and schema,
    run whole with stop/continue/ask and an execution log; Go to Database Object (⌥⇧⌘O);
    explorer filter, schemas to introspect, counts, routines, node refresh; pinned result tabs;
    SQL live templates. Remaining: analysis is lexical (subqueries flattened; the ambiguity check
    is single-level; unqualified usages may over-match; no SQL in Python/TypeScript strings);
    PostgreSQL/MySQL DDL is generated (no checks, sequences, triggers, partitions, grants) and
    DDL tabs are temp files never cleaned; routines come from information_schema without
    parameter lists; no metadata cache across disconnects; scripts take parameters only via
    `params`, keep logs in memory (last 20) and have no "run from caret"; no schema selector for
    SQLite; Go to Database Object has no menu item (RustRover has none); rename covers table
    aliases and CTEs, not column aliases.
