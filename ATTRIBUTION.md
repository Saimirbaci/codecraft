# Attribution

CodeCraft contains no third-party code, fonts, icons or other assets beyond its Cargo
dependencies (each under its own license, see `Cargo.lock`).

| What | Source | License | Use |
|---|---|---|---|
| Menu structure and shortcuts of RustRover 2026.1 | Observed from the running application (`docs/observation/`) | Functional facts, recorded clean-room | Command ids and default keymap |
| rust-analyzer | Run as an external process | MIT OR Apache-2.0 | Language intelligence |
| `ra-ap-rustc_lexer` | crates.io | MIT OR Apache-2.0 | Rust lexing for highlighting |
| fzf's v2 matching algorithm (idea only, no code) | github.com/junegunn/fzf | MIT | Scoring scheme of `codecraft_fs::fuzzy_match` |

The UI uses the platform's own fonts at runtime (or egui's bundled fonts); no font files are
committed. Colours are CodeCraft's own approximations of a dark IDE theme.
