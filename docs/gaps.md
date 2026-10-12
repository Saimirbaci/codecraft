# Gaps

Known shortfalls against RustRover, ranked. Update this list when you close one (with the date).

## High

1. ~~No debugger~~ (landed in M2: lldb-dap for Rust/C/C++/Zig, debugpy for Python). Remaining:
   `&str` values show the data pointer before the text; no memory/disassembly views; js-debug
   for Node/TS only when the user installs it; no attach-to-process UI yet.
2. ~~No VCS~~ (landed in M3). Remaining: 3-way merge view (conflicts open with markers
   highlighted), per-hunk staging, worktrees, GitHub/GitLab integration.
3. ~~No terminal tool window~~ (landed in M3, with `claude` launched already connected).
4. ~~Highlighting is lexical only~~ (M3: semantic tokens with mutable underline and unresolved
   references, inlay hints for types and parameters). Remaining: parameter-name hints are not
   clickable; hints are not shown in non-active split views.
5. **Full-file re-lex on every edit** (and full-text LSP sync). Fine at a few thousand lines;
   large files will feel it. Needs incremental lexing and incremental LSP sync.
6. **macOS keymap only.** Windows/Linux keymaps (Ctrl-based) are not mapped yet.

## Medium

7. In-window menu bar; RustRover on macOS uses the global NSMenu bar.
8. No split editors, editor tab groups or detachable tool windows.
9. Completion: no snippet placeholders (snippets are flattened), no completion item resolve,
   no auto-popup tuning (RustRover's delays and fuzzy ranking).
10. Rename has no preview or conflict dialog; it applies rust-analyzer's edit directly.
11. Run configurations are derived only (no Edit Configurations dialog, no env/args editing UI).
12. Test results show as a summary + console; there is no test tree tool window yet.
13. Settings dialog covers a handful of options; no color scheme editor, no keymap editor.
14. ~~No file watching~~ (M3: FSEvents/inotify watcher reloads unmodified files and refreshes the tree and Git).
15. Reformat with rustfmt replaces the whole document (the caret is clamped, not mapped).
16. Lazily filled RustRover submenus (Tools › Rust, Edit › Find Usages, Code › Folding…) were not
    captured by the menu observation. Open each menu in RustRover and re-run the dump.

## Low

17. No IME pre-edit (composition) rendering; committed text works.
18. Project view lacks drag and drop, delete and rename operations, and External Libraries.
19. No bookmarks, TODO tool window, local history, macros or live templates.

## M2 follow-ups (2026-10-12)

20. Ask mode for Claude Code and Codex needs the `claude-code-acp` / `codex-acp` adapters (not
    installed here); Gemini and Grok speak ACP natively. Grok's `streaming-messages-json` stream
    format is parsed leniently (no recorded sample yet).
21. Gutter ▶ run/debug icons exist only for Rust; other languages run via Ctrl+Shift+R / Ctrl+Shift+D.
22. Python and TypeScript language servers are not installed on this machine
    (`pip install basedpyright`, `npm i -g typescript-language-server typescript`); CodeCraft
    shows install hints and keeps highlighting/run working.
23. Database: no in-grid editing yet; TLS for PostgreSQL is not supported.
