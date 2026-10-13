//! Scratch files (File › New › Scratch File, Shift+Cmd+N): throwaway files outside the project,
//! one per language, kept in `$CODECRAFT_HOME/scratches` (default `~/.codecraft/scratches`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Session;
use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, ok, spec};
use crate::params::{opt_str, req_path};

/// Languages offered for new scratch files: (name, extension, initial text, caret offset).
pub const LANGUAGES: &[(&str, &str, &str, usize)] = &[
    ("Rust", "rs", "fn main() {\n    \n}\n", 16),
    ("Python", "py", "", 0),
    ("Plain text", "txt", "", 0),
    ("Markdown", "md", "", 0),
    ("JSON", "json", "", 0),
    ("TOML", "toml", "", 0),
    ("YAML", "yaml", "", 0),
    ("Shell Script", "sh", "#!/bin/sh\n", 10),
    ("SQL", "sql", "", 0),
    ("C", "c", "#include <stdio.h>\n\nint main(void) {\n    \n    return 0;\n}\n", 41),
    ("C++", "cpp", "#include <iostream>\n\nint main() {\n    \n    return 0;\n}\n", 38),
    ("Zig", "zig", "const std = @import(\"std\");\n\npub fn main() !void {\n    \n}\n", 55),
    ("TypeScript", "ts", "", 0),
    ("JavaScript", "js", "", 0),
];

/// Scratch files kept (numbering stops here rather than scanning forever).
const MAX_SCRATCHES: usize = 10_000;

/// The language chooser (Shift+Cmd+N without a language).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchChooser {
    pub query: String,
    pub selected: usize,
}

impl ScratchChooser {
    /// Indexes into [`LANGUAGES`] matching the query.
    pub fn visible(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        LANGUAGES
            .iter()
            .enumerate()
            .filter(|(_, (name, ext, ..))| q.is_empty() || name.to_lowercase().contains(&q) || ext.starts_with(&q))
            .map(|(i, _)| i)
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct ScratchState {
    /// Explicit directory (tests and embedders); otherwise `$CODECRAFT_HOME/scratches`.
    pub dir_override: Option<PathBuf>,
    pub chooser: Option<ScratchChooser>,
    /// The Scratches list popup is open.
    pub listing: bool,
}

impl ScratchState {
    pub fn dir(&self) -> Option<PathBuf> {
        if let Some(d) = &self.dir_override {
            return Some(d.clone());
        }
        if let Some(home) = std::env::var_os("CODECRAFT_HOME").filter(|h| !h.is_empty()) {
            return Some(PathBuf::from(home).join("scratches"));
        }
        let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).filter(|h| !h.is_empty())?;
        Some(PathBuf::from(home).join(".codecraft").join("scratches"))
    }
}

/// The language entry for a name or extension (`"rust"`, `"rs"`, `"C++"`).
pub fn language(key: &str) -> Option<&'static (&'static str, &'static str, &'static str, usize)> {
    let k = key.trim().to_lowercase();
    LANGUAGES.iter().find(|(name, ext, ..)| name.to_lowercase() == k || *ext == k)
}

/// The first free `scratch.ext`, `scratch_1.ext`, … in `dir`.
pub fn next_name(dir: &Path, ext: &str) -> Option<PathBuf> {
    (0..MAX_SCRATCHES)
        .map(|i| if i == 0 { format!("scratch.{ext}") } else { format!("scratch_{i}.{ext}") })
        .map(|n| dir.join(n))
        .find(|p| !p.exists())
}

/// Scratch files, most recently modified first.
pub fn list(dir: &Path) -> Vec<(PathBuf, std::time::SystemTime)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<_> = rd
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            m.is_file().then(|| (e.path(), m.modified().unwrap_or(std::time::UNIX_EPOCH)))
        })
        .take(MAX_SCRATCHES)
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// Create a scratch file for `lang` and open it.
pub(crate) fn create(s: &mut Session, lang: &str) -> CmdResult {
    let (_, ext, text, caret) =
        *language(lang).ok_or_else(|| CommandError::BadParams(format!("unknown scratch language '{lang}'")))?;
    let dir = s.scratch.dir().ok_or_else(|| CommandError::Failed("cannot find the home directory; set CODECRAFT_HOME".into()))?;
    std::fs::create_dir_all(&dir).map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", dir.display())))?;
    let path = next_name(&dir, ext).ok_or_else(|| CommandError::Failed("too many scratch files".into()))?;
    codecraft_fs::write_text_file_atomic(&path, text).map_err(|e| CommandError::Failed(e.to_string()))?;
    s.scratch.chooser = None;
    s.open_file(&path)?;
    let len = text.chars().count();
    s.select_active(|_, _| codecraft_text::SelectionSet::caret(caret.min(len)));
    Ok(json!({ "path": path }))
}

/// `path` if it is a file inside the scratches directory.
fn scratch_path(s: &Session, p: &Value) -> Result<PathBuf, CommandError> {
    let path = crate::normalize_path(&req_path(p, "path")?);
    let dir = s.scratch.dir().map(|d| crate::normalize_path(&d));
    if !dir.is_some_and(|d| path.starts_with(&d) && path != d) {
        return Err(CommandError::BadParams(format!("{} is not a scratch file", path.display())));
    }
    Ok(path)
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "file.new.scratchFile",
            "Scratch File",
            None,
            "{language?: \"Rust\" | \"rs\" | \"Python\" | …}: without a language, opens the chooser",
            Context::Global,
            always,
            |s, p| match opt_str(p, "language") {
                Some(lang) => create(s, lang),
                None => {
                    s.scratch.chooser = Some(ScratchChooser::default());
                    s.scratch.listing = false;
                    Ok(json!({ "languages": LANGUAGES.iter().map(|l| l.0).collect::<Vec<_>>() }))
                }
            },
        ),
        spec("scratch.closeChooser", "Close Scratch Language Chooser", None, "", Context::Global, always, |s, _| {
            s.scratch.chooser = None;
            ok()
        }),
        spec(
            "file.scratches",
            "Scratches",
            None,
            "Opens the scratch files list; returns {dir, files: [path]}",
            Context::Global,
            always,
            |s, _| {
                s.scratch.listing = true;
                s.scratch.chooser = None;
                let dir = s.scratch.dir();
                let files: Vec<PathBuf> = dir.as_deref().map(list).unwrap_or_default().into_iter().map(|(p, _)| p).collect();
                Ok(json!({ "dir": dir, "files": files }))
            },
        ),
        spec("scratch.closeList", "Close Scratches", None, "", Context::Global, always, |s, _| {
            s.scratch.listing = false;
            ok()
        }),
        spec("scratch.open", "Open Scratch File", None, "{path}", Context::Global, always, |s, p| {
            let path = scratch_path(s, p)?;
            s.open_file(&path)?;
            s.scratch.listing = false;
            ok()
        }),
        spec("scratch.delete", "Delete Scratch File", None, "{path}", Context::Global, always, |s, p| {
            let path = scratch_path(s, p)?;
            if let Some(id) = s.ws.doc_by_path(&path) {
                for e in s.ws.editors_for_doc(id) {
                    s.ws.close_editor(e);
                }
            }
            std::fs::remove_file(&path).map_err(|e| CommandError::Failed(format!("cannot delete {}: {e}", path.display())))?;
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(dir: &Path) -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.scratch.dir_override = Some(dir.join("scratches"));
        s
    }

    #[test]
    fn create_numbers_and_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let mut s = session(&root);
        let v = s.execute("file.new.scratchFile", &json!({})).unwrap();
        assert!(v["languages"].as_array().unwrap().len() >= 10);
        assert!(s.scratch.chooser.is_some());
        let a = s.execute("file.new.scratchFile", &json!({ "language": "Rust" })).unwrap();
        assert!(s.scratch.chooser.is_none());
        let b = s.execute("file.new.scratchFile", &json!({ "language": "rs" })).unwrap();
        assert!(a["path"].as_str().unwrap().ends_with("scratch.rs"));
        assert!(b["path"].as_str().unwrap().ends_with("scratch_1.rs"));
        let (e, d) = s.ws.active().unwrap();
        assert_eq!(d.doc.text(), "fn main() {\n    \n}\n");
        assert_eq!(e.selections.primary().head, 16, "caret inside main");
        assert_eq!(d.language, codecraft_syntax::Language::Rust);
        s.execute("file.new.scratchFile", &json!({ "language": "c++" })).unwrap();
        let v = s.execute("file.scratches", &json!({})).unwrap();
        assert_eq!(v["files"].as_array().unwrap().len(), 3);
        // Delete closes its editor.
        let p = b["path"].as_str().unwrap().to_string();
        s.execute("scratch.delete", &json!({ "path": p })).unwrap();
        assert!(!Path::new(&p).exists());
        assert!(s.ws.doc_by_path(Path::new(&p)).is_none());
    }

    #[test]
    fn caret_offsets_are_inside_templates() {
        for (name, _, text, caret) in LANGUAGES {
            assert!(*caret <= text.chars().count(), "{name}");
            if !text.is_empty() && text.contains("    \n") {
                let at: String = text.chars().take(*caret).collect();
                assert!(at.ends_with("    "), "{name}: caret after the indent");
            }
        }
    }

    #[test]
    fn hostile_params() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let mut s = session(&root);
        assert!(s.execute("file.new.scratchFile", &json!({ "language": "../../x" })).is_err());
        assert!(s.execute("file.new.scratchFile", &json!({ "language": "" })).is_err());
        std::fs::write(root.join("victim.txt"), "keep").unwrap();
        for p in [
            json!({ "path": root.join("victim.txt") }),
            json!({ "path": root.join("scratches/../victim.txt") }),
            json!({ "path": root.join("scratches") }),
            json!({}),
        ] {
            assert!(s.execute("scratch.delete", &p).is_err(), "{p}");
        }
        assert!(root.join("victim.txt").exists());
    }

    #[test]
    fn chooser_filter() {
        let c = ScratchChooser { query: "py".into(), selected: 0 };
        let names: Vec<&str> = c.visible().iter().map(|i| LANGUAGES[*i].0).collect();
        assert_eq!(names, vec!["Python"]);
        assert_eq!(ScratchChooser::default().visible().len(), LANGUAGES.len());
    }
}
