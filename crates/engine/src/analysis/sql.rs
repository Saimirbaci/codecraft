//! CodeCraft's SQL inspections (`database::sql_nav::inspect`) over the `.sql` files of a scope,
//! without a data source (names are not resolved against a schema): UPDATE/DELETE without WHERE,
//! INSERT value counts, `SELECT *` in a view, unused aliases and CTEs, ambiguous columns. Their
//! quick-fixes become fixes; `-- noinspection Id` suppresses.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use codecraft_text::Document;

use crate::database::sql_nav::inspect;
use crate::inspections::{self, Applicability, Finding, Fix, TextChange, Tool, ToolRun};

/// `.sql` files inspected per run and their largest size.
const MAX_FILES: usize = 2000;
const MAX_BYTES: u64 = 2 << 20;

pub(crate) const EXTS: &[&str] = &["sql", "ddl", "dml"];

fn is_sql(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

pub(crate) fn has_sql(files: &[PathBuf]) -> bool {
    files.iter().any(|f| is_sql(f))
}

fn change(path: &Path, doc: &Document, e: &inspect::SqlEdit) -> Option<TextChange> {
    let len = doc.len_chars();
    if e.start > e.end || e.end > len {
        return None;
    }
    Some(TextChange {
        path: path.to_path_buf(),
        start: doc.offset_to_position(e.start),
        end: doc.offset_to_position(e.end),
        new_text: e.text.clone(),
    })
}

/// The findings of one SQL text.
pub fn inspect_text(path: &Path, text: &str) -> Vec<Finding> {
    let doc = Document::from_text(text);
    let text = doc.text();
    let mut out = Vec::new();
    for p in inspect::check(&text, None) {
        let start = doc.offset_to_position(p.start.min(doc.len_chars()));
        let end = doc.offset_to_position(p.end.min(doc.len_chars())).max(start);
        let mut f = Finding::new(Tool::Sql, p.code, p.severity, path.to_path_buf(), start);
        f.end = end;
        f.code = Some(p.code.to_string());
        f.message = p.message.clone();
        f.description = format!("{}\n\nInspection: {}", p.message, p.code);
        for q in inspect::fixes(&text, &p, None) {
            if q.title.starts_with("Suppress") {
                continue;
            }
            let changes: Option<Vec<TextChange>> = q.edits.iter().map(|e| change(path, &doc, e)).collect();
            if let Some(changes) = changes.filter(|c| !c.is_empty()) {
                f.fixes.push(Fix { title: q.title.clone(), applicability: Applicability::MaybeIncorrect, changes });
            }
        }
        out.push(f);
    }
    out
}

/// Inspect the scope's SQL files.
pub(crate) fn run(files: &[PathBuf], root: &Path, cancel: &AtomicBool, progress: &dyn Fn(&str)) -> (ToolRun, Vec<Finding>, bool) {
    let mut out = Vec::new();
    let mut n = 0usize;
    let mut skipped = 0usize;
    for f in files.iter().filter(|f| is_sql(f)).take(MAX_FILES) {
        if cancel.load(Ordering::Relaxed) {
            return (ToolRun { tool: Tool::Sql, command_line: String::new(), outcome: "cancelled".into(), ok: false }, out, true);
        }
        if std::fs::metadata(f).map(|m| m.len() > MAX_BYTES).unwrap_or(true) {
            skipped += 1;
            continue;
        }
        progress(&format!("Inspecting {}", f.strip_prefix(root).unwrap_or(f).display()));
        if let Some(text) = inspections::read_normalized(f) {
            out.extend(inspect_text(f, &text));
            n += 1;
        }
    }
    let mut outcome = format!("{} in {n} file(s)", inspections::count_label(out.len()));
    if skipped > 0 {
        outcome.push_str(&format!(", {skipped} too large"));
    }
    (ToolRun { tool: Tool::Sql, command_line: "built-in SQL inspections".into(), outcome, ok: true }, out, false)
}

/// `-- noinspection Code` above the statement (from the inspection's own suppress fix).
pub(crate) fn suppress_change(f: &Finding, text: &str) -> Result<TextChange, String> {
    let code = f.code.as_deref().ok_or("this problem cannot be suppressed")?;
    let doc = Document::from_text(text);
    let text = doc.text();
    let start = crate::inspections::fixes::offset_of(&doc, f.start).ok_or("the problem is outside the file")?;
    let problem = inspect::check(&text, None)
        .into_iter()
        .find(|p| p.code == code && p.start == start)
        .ok_or("the problem is no longer in the file; inspect again")?;
    let fix = inspect::fixes(&text, &problem, None)
        .into_iter()
        .find(|q| q.title == "Suppress for statement")
        .ok_or("this problem cannot be suppressed")?;
    let e = fix.edits.first().ok_or("this problem cannot be suppressed")?;
    change(&f.path, &doc, e).ok_or_else(|| "the suppression is outside the file".to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use codecraft_text::Position;

    #[test]
    fn sql_files_are_inspected_with_fixes_and_suppression() {
        let text = "SELECT 1;\nDELETE FROM users;\nWITH unused AS (SELECT 1) SELECT 2;\n";
        let fs = inspect_text(Path::new("/p/q.sql"), text);
        let codes: Vec<&str> = fs.iter().map(|f| f.rule()).collect();
        assert!(codes.contains(&"SqlWithoutWhere"), "{codes:?}");
        assert!(codes.contains(&"SqlUnusedCte"), "{codes:?}");
        let del = fs.iter().find(|f| f.rule() == "SqlWithoutWhere").unwrap();
        assert_eq!(del.start.line, 1);
        let cte = fs.iter().find(|f| f.rule() == "SqlUnusedCte").unwrap();
        assert!(!cte.fixes.is_empty(), "removing the CTE is a fix");
        let c = suppress_change(del, text).unwrap();
        assert_eq!((c.start, c.new_text.as_str()), (Position::new(1, 0), "-- noinspection SqlWithoutWhere\n"));
        // Once suppressed, the problem is gone.
        let suppressed = "SELECT 1;\n-- noinspection SqlWithoutWhere\nDELETE FROM users;\n";
        assert!(inspect_text(Path::new("/p/q.sql"), suppressed).iter().all(|f| f.rule() != "SqlWithoutWhere"));
        let mut moved = del.clone();
        moved.start = Position::new(0, 0);
        assert!(suppress_change(&moved, text).is_err());
    }
}
