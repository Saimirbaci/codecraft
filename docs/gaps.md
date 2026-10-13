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
   references, inlay hints for types and parameters). Remaining: parameter-name hints are not
   clickable; hints are not shown in non-active split views.
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
   Help › Help; `cargo xtask bundle|dmg|package-linux|package-windows`). Remaining: Merge All
   Project Windows (macOS window tabs) is not implemented; project windows are not restored on
   restart; plain-key and Alt+character shortcuts (Backspace, Tab, Alt+/) are not native key
   equivalents, so the native menu shows no shortcut for them; enabled states refresh every 250 ms
   instead of when a menu opens; no Developer ID signing or notarization in CI; the Windows zip task
   is untested on Windows.
8. ~~No split editors, editor tab groups~~ (M4, 2026-10-13: Split Right/Down, Split and Move,
   Unsplit/Unsplit All, Next/Previous Splitter, Change Orientation, Maximize/Normalize, Stretch
   Editor, draggable splitters, a tab bar per group, tabs dragged between groups, several views
   of one file whose carets follow each other's edits). Remaining: no detachable tool windows or
   editor windows; Keep Tab Open (no preview tab) and Show Hidden Tabs are not implemented; the
   split layout is not restored on restart (open tabs are not restored either).
9. Completion: no snippet placeholders (snippets are flattened), no completion item resolve,
   no auto-popup tuning (RustRover's delays and fuzzy ranking).
10. ~~Rename has no preview or conflict dialog~~ (M7, 2026-10-13: multi-file renames, code actions and
    file moves open the refactoring preview with per-file checkboxes and diffs; `prepareRename` and
    rename errors show "Cannot Rename"). Remaining: rename is a popup, not RustRover's in-place
    inline rename; name conflicts are only those rust-analyzer reports.
11. ~~Run configurations are derived only~~ (M8, 2026-10-13: Run › Edit Configurations… lists the
    project's derived configurations and user ones (Cargo command or any command line, env vars,
    working directory), creates/copies/deletes/edits them with Apply/OK/Cancel and saves them in
    `.codecraft/run-configurations.json`; `runConfig.*` commands do the same headlessly).
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
    Templates are read-only in Settings; show-whitespace is a setting the editor does not paint
    yet; colour schemes cannot be imported/exported as files.
14. ~~No file watching~~ (M3: FSEvents/inotify watcher reloads unmodified files and refreshes the tree and Git).
15. ~~Reformat with rustfmt replaces the whole document~~ (M4, 2026-10-13: only changed lines are
    replaced and carets keep their token through whitespace changes).
16. Lazily filled RustRover submenus (Tools › Rust, Edit › Find Usages, Code › Folding…) were not
    captured by the menu observation. Open each menu in RustRover and re-run the dump.

## Low

17. No IME pre-edit (composition) rendering; committed text works.
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
    Move Element Left/Right, Next Edit Location and Cyclic Expand Word landed. Remaining: wrapped
    rows do not keep the line's indentation; box selection has no virtual space (short lines get
    a caret at their end); live template variables have no expressions (`completeSmart()`,
    `suggestVariableName()`); breadcrumbs and Unwrap/Remove are lexical (no LSP document
    symbols); Auto-Indent Lines covers brace languages only; Optimize Imports for Rust
    (2026-10-13) sorts single-line `use` groups and applies rust-analyzer's "Remove all unused
    imports" over the file, but does not merge imports; no live template settings page (edit
    `templates.json`).
