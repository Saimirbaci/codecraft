# Gaps

Known shortfalls against RustRover, ranked. Update this list when you close one (with the date).

## High

1. **No debugger.** RustRover's Debug run, breakpoints, variables, and LLDB/GDB integration (M5).
2. **No VCS.** The Git menu is unimplemented: commit, diff, log, branches, annotate (M4).
3. **No terminal tool window** (M3).
4. **Highlighting is lexical only.** No semantic tokens yet: locals, parameters, mutable
   bindings, unresolved references and trait methods all look the same. No inlay hints.
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
14. No file watching: external changes are picked up on window focus and by Reload All from Disk.
15. Reformat with rustfmt replaces the whole document (the caret is clamped, not mapped).
16. Lazily filled RustRover submenus (Tools › Rust, Edit › Find Usages, Code › Folding…) were not
    captured by the menu observation. Open each menu in RustRover and re-run the dump.

## Low

17. No IME pre-edit (composition) rendering; committed text works.
18. Project view lacks drag and drop, delete and rename operations, and External Libraries.
19. No bookmarks, TODO tool window, local history, macros or live templates.
