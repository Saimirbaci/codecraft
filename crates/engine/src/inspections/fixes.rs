//! Quick-fixes, suppressions and Code Cleanup: [`Fix`]es (char coordinates) → non-overlapping edits
//! → an LSP `WorkspaceEdit` reviewed and applied through the refactoring preview.
//!
//! A fix applies whole or not at all: when "fix all" meets two fixes that touch the same text, the
//! first wins and the other is left for a rerun. After a fix is applied, the other findings of the
//! file are moved through the edit so their positions and fixes stay valid.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use regex::Regex;
use serde_json::{Value, json};

use codecraft_lsp::lsp_types as lt;
use codecraft_text::{Document, Position};

use super::{Finding, Fix, InspectionResults, JobSpec, Rewrite, TextChange, Tool, text_hash};
use crate::{CommandError, Session};

/// Char range in a file's text and its replacement.
pub type Splice = (Range<usize>, String);

/// The outcome of choosing which fixes to apply together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Finding ids whose fix was accepted.
    pub accepted: Vec<usize>,
    /// Accepted replacements per file, sorted, non-overlapping (offsets in the current text).
    pub splices: BTreeMap<PathBuf, Vec<Splice>>,
    /// Fixes left out because they overlap an accepted one.
    pub conflicts: usize,
    /// Fixes whose positions are outside their file (or whose file cannot be read).
    pub invalid: usize,
}

/// A char position → offset, refusing positions outside the text.
pub fn offset_of(doc: &Document, pos: Position) -> Option<usize> {
    if pos.line >= doc.len_lines() || pos.column > doc.line_len(pos.line) {
        return None;
    }
    Some(doc.line_start(pos.line).saturating_add(pos.column))
}

fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start.max(b.start) < a.end.min(b.end) || (a.start == b.start && (a.is_empty() || b.is_empty()))
}

/// Choose fixes to apply together. `texts` holds the current text of every file the fixes touch
/// (missing files make their fixes invalid).
pub fn select(fixes: &[(usize, Fix)], texts: &BTreeMap<PathBuf, String>) -> Selection {
    let docs: BTreeMap<&PathBuf, Document> = texts.iter().map(|(p, t)| (p, Document::from_text(t))).collect();
    let mut sel = Selection::default();
    'fixes: for (id, fix) in fixes {
        let mut mine: Vec<(PathBuf, Splice)> = Vec::new();
        for c in &fix.changes {
            let Some(doc) = docs.get(&c.path) else {
                sel.invalid += 1;
                continue 'fixes;
            };
            let (Some(a), Some(b)) = (offset_of(doc, c.start), offset_of(doc, c.end)) else {
                sel.invalid += 1;
                continue 'fixes;
            };
            if b < a {
                sel.invalid += 1;
                continue 'fixes;
            }
            mine.push((c.path.clone(), (a..b, c.new_text.clone())));
        }
        if mine.is_empty() {
            sel.invalid += 1;
            continue;
        }
        // Within the fix itself the changes must not overlap either.
        for (i, (p, (r, _))) in mine.iter().enumerate() {
            if mine.iter().skip(i + 1).any(|(q, (r2, _))| p == q && overlaps(r, r2)) {
                sel.invalid += 1;
                continue 'fixes;
            }
        }
        // Identical to what is already accepted: nothing more to do (two lints, one suggestion).
        let all_dup =
            mine.iter().all(|(p, s)| sel.splices.get(p).is_some_and(|v| v.iter().any(|x| x == s)));
        if all_dup {
            sel.accepted.push(*id);
            continue;
        }
        let clash = mine.iter().any(|(p, (r, _))| sel.splices.get(p).is_some_and(|v| v.iter().any(|(r2, _)| overlaps(r, r2))));
        if clash {
            sel.conflicts += 1;
            continue;
        }
        for (p, s) in mine {
            sel.splices.entry(p).or_default().push(s);
        }
        sel.accepted.push(*id);
    }
    for v in sel.splices.values_mut() {
        v.sort_by_key(|(r, _)| (r.start, r.end));
    }
    sel
}

/// Apply sorted, non-overlapping splices (char offsets) to a text.
pub fn splice_text(text: &str, splices: &[Splice]) -> Result<String, String> {
    let bytes: Vec<usize> = text.char_indices().map(|(b, _)| b).chain(std::iter::once(text.len())).collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0usize;
    for (r, new) in splices {
        let (Some(&a), Some(&b)) = (bytes.get(r.start), bytes.get(r.end)) else {
            return Err("an edit is outside the file".into());
        };
        if a < at || b < a {
            return Err("overlapping edits".into());
        }
        out.push_str(text.get(at..a).unwrap_or(""));
        out.push_str(new);
        at = b;
    }
    out.push_str(text.get(at..).unwrap_or(""));
    Ok(out)
}

/// The LSP workspace edit for a selection (one `TextDocumentEdit` per file, UTF-16 positions).
pub fn workspace_edit(sel: &Selection, texts: &BTreeMap<PathBuf, String>) -> Result<lt::WorkspaceEdit, String> {
    let mut edits = Vec::new();
    for (path, splices) in &sel.splices {
        let text = texts.get(path).ok_or_else(|| format!("{} cannot be read", path.display()))?;
        let doc = Document::from_text(text);
        let uri = codecraft_lsp::path_to_uri(path).map_err(|e| e.to_string())?;
        edits.push(lt::TextDocumentEdit {
            text_document: lt::OptionalVersionedTextDocumentIdentifier { uri, version: None },
            edits: splices
                .iter()
                .map(|(r, new)| {
                    lt::OneOf::Left(lt::TextEdit {
                        range: crate::convert::lsp_range(&doc, r.clone()),
                        new_text: new.clone(),
                    })
                })
                .collect(),
        });
    }
    Ok(lt::WorkspaceEdit { document_changes: Some(lt::DocumentChanges::Edits(edits)), ..Default::default() })
}

/// Fixes → a workspace edit against `read` (current texts), with the selection made.
pub fn fixes_to_workspace_edit(
    fixes: &[(usize, Fix)],
    read: &dyn Fn(&Path) -> Option<String>,
) -> Result<(lt::WorkspaceEdit, Selection, BTreeMap<PathBuf, String>), String> {
    let paths: BTreeSet<&PathBuf> = fixes.iter().flat_map(|(_, f)| f.changes.iter().map(|c| &c.path)).collect();
    let texts: BTreeMap<PathBuf, String> = paths.into_iter().filter_map(|p| read(p).map(|t| (p.clone(), t))).collect();
    let sel = select(fixes, &texts);
    let edit = workspace_edit(&sel, &texts)?;
    Ok((edit, sel, texts))
}

// ---------------------------------------------------------------- moving findings through an edit

/// Map a char offset of the old text through sorted splices. Offsets inside a replaced range move
/// to its start; `None` when the offset was inside a replaced range (the caller drops fixes).
fn map_offset(splices: &[Splice], o: usize) -> (usize, bool) {
    let mut delta: isize = 0;
    for (r, new) in splices {
        if r.end <= o {
            let added = isize::try_from(new.chars().count()).unwrap_or(isize::MAX);
            let removed = isize::try_from(r.len()).unwrap_or(isize::MAX);
            delta = delta.saturating_add(added.saturating_sub(removed));
        } else if r.start < o {
            return (r.start.saturating_add_signed(delta), true);
        } else {
            break;
        }
    }
    (o.saturating_add_signed(delta), false)
}

/// After `path` changed from `before` to `after` by `splices`, move the findings and fixes that
/// point into it; fixes overlapping the applied edits are dropped.
pub fn remap(results: &mut InspectionResults, path: &Path, before: &str, after: &str, splices: &[Splice]) {
    let old = Document::from_text(before);
    let new = Document::from_text(after);
    let map = |p: Position| -> (Position, bool) {
        let o = old.position_to_offset(p);
        let (n, inside) = map_offset(splices, o);
        (new.offset_to_position(n), inside)
    };
    for f in results.findings.iter_mut() {
        if f.path == path {
            f.start = map(f.start).0;
            f.end = map(f.end).0.max(f.start);
        }
        f.fixes.retain_mut(|fix| {
            for c in fix.changes.iter_mut().filter(|c| c.path == path) {
                let (o_s, o_e) = (old.position_to_offset(c.start), old.position_to_offset(c.end));
                let hit = splices.iter().any(|(r, _)| overlaps(r, &(o_s..o_e)));
                if hit {
                    return false;
                }
                c.start = map(c.start).0;
                c.end = map(c.end).0;
            }
            true
        });
    }
    results.hashes.insert(path.to_path_buf(), text_hash(after));
}

// ---------------------------------------------------------------- applying through the preview

/// Fixes waiting in the refactoring preview: settled once the preview closes.
#[derive(Clone, Debug)]
pub struct Pending {
    pub title: String,
    pub ids: Vec<usize>,
    /// Per file: text before, after, and the splices between them.
    pub files: BTreeMap<PathBuf, (String, String, Vec<Splice>)>,
}

/// Apply (or preview) fixes for findings. `preview`: always open the refactoring preview;
/// otherwise single-file fixes apply at once (RustRover's behaviour) and the rest are previewed.
pub(crate) fn apply(s: &mut Session, title: &str, fixes: Vec<(usize, Fix)>, preview: bool) -> Result<Value, CommandError> {
    let results = s.inspections.results.as_ref().ok_or_else(|| CommandError::Failed("no inspection results".into()))?;
    if fixes.is_empty() {
        return Err(CommandError::Failed("nothing to fix".into()));
    }
    // Refuse files edited since the inspection (positions would be wrong).
    let mut stale = BTreeSet::new();
    let fresh: Vec<(usize, Fix)> = fixes
        .into_iter()
        .filter(|(_, fix)| {
            fix.changes.iter().all(|c| {
                let ok = super::is_fresh(s, results, &c.path);
                if !ok {
                    stale.insert(c.path.clone());
                }
                ok
            })
        })
        .collect();
    if fresh.is_empty() {
        let files: Vec<String> = stale.iter().map(|p| p.display().to_string()).collect();
        return Err(CommandError::Failed(format!(
            "{} changed since the inspection; run Inspect Code again",
            files.join(", ")
        )));
    }
    let (edit, sel, texts) = fixes_to_workspace_edit(&fresh, &|p| s.current_text(p)).map_err(CommandError::Failed)?;
    if sel.accepted.is_empty() {
        return Err(CommandError::Failed("the fixes no longer match the files; run Inspect Code again".into()));
    }
    let mut files = BTreeMap::new();
    for (path, splices) in &sel.splices {
        let before = texts.get(path).cloned().unwrap_or_default();
        let after = splice_text(&before, splices).map_err(CommandError::Failed)?;
        files.insert(path.clone(), (before, after, splices.clone()));
    }
    let pending = Pending { title: title.to_string(), ids: sel.accepted.clone(), files };
    let summary = json!({
        "accepted": sel.accepted,
        "conflicts": sel.conflicts,
        "invalid": sel.invalid,
        "stale": stale.len(),
        "files": sel.splices.len(),
    });
    if sel.splices.is_empty() {
        // Everything was a duplicate of nothing (cannot happen with accepted ids, but be safe).
        return Ok(summary);
    }
    let pv = s.build_preview(title, &edit)?;
    if preview || pv.needs_preview() {
        s.refactor.preview = Some(pv);
        s.inspections.pending = Some(pending);
        let mut v = summary;
        v["preview"] = Value::Bool(true);
        return Ok(v);
    }
    let report = s.apply_preview(&pv);
    s.refactor.last_report = Some(report.clone());
    settle(s, &pending);
    s.status = format!("{title}: {} fix(es) applied", pending.ids.len());
    let mut v = summary;
    v["preview"] = Value::Bool(false);
    v["applied"] = json!(report.applied);
    v["errors"] = json!(report.errors);
    Ok(v)
}

/// Mark what a closed preview applied: files whose text is now the expected result get their
/// findings moved and the fixed ones resolved.
pub(crate) fn settle(s: &mut Session, pending: &Pending) -> bool {
    let applied: Vec<&PathBuf> = pending
        .files
        .iter()
        .filter(|(p, (_, after, _))| s.current_text(p).is_some_and(|t| t == *after))
        .map(|(p, _)| p)
        .collect();
    if applied.is_empty() {
        return false;
    }
    let Some(results) = s.inspections.results.as_mut() else { return false };
    let applied_set: BTreeSet<&PathBuf> = applied.iter().copied().collect();
    // A finding is resolved when every file its fix touched was applied.
    for id in &pending.ids {
        let Some(f) = results.findings.get(*id) else { continue };
        let touched: BTreeSet<&PathBuf> =
            pending.files.keys().filter(|p| f.path == **p || f.fixes.iter().any(|x| x.changes.iter().any(|c| c.path == **p))).collect();
        if touched.iter().all(|p| applied_set.contains(p)) {
            results.resolved.insert(*id);
        }
    }
    for p in applied {
        if let Some((before, after, splices)) = pending.files.get(p) {
            remap(results, p, before, after, splices);
        }
    }
    if s.inspections.selected.is_some_and(|id| s.inspections.results.as_ref().is_some_and(|r| r.resolved.contains(&id))) {
        s.inspections.selected = None;
    }
    true
}

/// Settle pending fixes once their preview is closed (applied or cancelled).
pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(p) = &s.inspections.pending else { return false };
    if s.refactor.preview.as_ref().is_some_and(|pv| pv.title == p.title) {
        return false;
    }
    let Some(p) = s.inspections.pending.take() else { return false };
    settle(s, &p);
    true
}

/// Code Cleanup's whole-file rewrites → the refactoring preview.
pub(crate) fn open_rewrites(s: &mut Session, title: &str, rewrites: &[Rewrite]) -> Result<(), CommandError> {
    let mut edits = Vec::new();
    let mut pending = Pending { title: title.to_string(), ids: Vec::new(), files: BTreeMap::new() };
    for r in rewrites {
        let current = s.current_text(&r.path);
        if current.as_deref() != Some(r.before.as_str()) {
            s.ws.notify(
                codecraft_workspace::NotificationLevel::Warning,
                title,
                format!("{} changed during the cleanup; skipped", r.path.display()),
            );
            continue;
        }
        let doc = Document::from_text(&r.before);
        let uri = codecraft_lsp::path_to_uri(&r.path).map_err(|e| CommandError::Failed(e.to_string()))?;
        edits.push(lt::TextDocumentEdit {
            text_document: lt::OptionalVersionedTextDocumentIdentifier { uri, version: None },
            edits: vec![lt::OneOf::Left(lt::TextEdit {
                range: crate::convert::lsp_range(&doc, 0..doc.len_chars()),
                new_text: r.after.clone(),
            })],
        });
        let all = vec![(0..doc.len_chars(), r.after.clone())];
        pending.files.insert(r.path.clone(), (r.before.clone(), r.after.clone(), all));
    }
    if edits.is_empty() {
        return Ok(());
    }
    // Findings with a safe fix in a rewritten file were fixed by the cleanup.
    if let Some(results) = &s.inspections.results {
        pending.ids = results
            .open()
            .filter(|f| pending.files.contains_key(&f.path) && f.safe_fix().is_some())
            .map(|f| f.id)
            .collect();
    }
    let edit = lt::WorkspaceEdit { document_changes: Some(lt::DocumentChanges::Edits(edits)), ..Default::default() };
    let pv = s.build_preview(title, &edit)?;
    s.refactor.preview = Some(pv);
    s.inspections.pending = Some(pending);
    Ok(())
}

// ---------------------------------------------------------------- Code Cleanup (worker side)

fn edition_for<'a>(spec: &'a JobSpec, path: &Path) -> &'a str {
    spec.editions
        .iter()
        .filter(|(dir, _)| path.starts_with(dir))
        .max_by_key(|(dir, _)| dir.components().count())
        .map(|(_, e)| e.as_str())
        .unwrap_or("2021")
}

/// Apply every machine-applicable fix of the findings, then format the scope's files. Runs on
/// the inspection worker; returns the files whose text changes and the errors met.
pub(crate) fn cleanup_rewrites(
    spec: &JobSpec,
    findings: &[Finding],
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
) -> (Vec<Rewrite>, Vec<String>) {
    let mut errors: Vec<String> = Vec::new();
    let mut fixes_by_file: BTreeMap<PathBuf, Vec<(usize, Fix)>> = BTreeMap::new();
    for f in findings {
        let Some(fix) = f.safe_fix() else { continue };
        // Cleanup rewrites file by file: fixes spanning files are left to the quick-fix.
        let paths: BTreeSet<&PathBuf> = fix.changes.iter().map(|c| &c.path).collect();
        if paths.len() == 1
            && let Some(p) = paths.first()
        {
            fixes_by_file.entry((*p).clone()).or_default().push((f.id, fix.clone()));
        }
    }
    let wanted = |t: Tool| super::wanted(spec, t);
    let ruff = if wanted(Tool::Ruff) { super::find_program(spec, Tool::Ruff) } else { None };
    let zig = if wanted(Tool::ZigFmt) { super::find_program(spec, Tool::ZigFmt) } else { None };
    let mut files: BTreeSet<PathBuf> = fixes_by_file.keys().cloned().collect();
    if spec.format {
        let ext_ok = |p: &Path| match p.extension().and_then(|e| e.to_str()) {
            Some("rs") => wanted(Tool::Clippy),
            Some("py" | "pyi") => ruff.is_some(),
            Some("zig") => zig.is_some(),
            _ => false,
        };
        files.extend(spec.scope.files.iter().filter(|p| ext_ok(p)).take(super::MAX_FILE_ARGS).cloned());
    }
    let mut format_failed: BTreeSet<&'static str> = BTreeSet::new();
    let mut out = Vec::new();
    for path in files {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let rel = path.strip_prefix(&spec.root).unwrap_or(&path).display().to_string();
        progress(&format!("Cleaning up {rel}"));
        let Some(before) = super::read_normalized(&path) else {
            errors.push(format!("{rel}: cannot be read"));
            continue;
        };
        let mut text = before.clone();
        if let Some(fixes) = fixes_by_file.get(&path) {
            let texts = BTreeMap::from([(path.clone(), before.clone())]);
            let sel = select(fixes, &texts);
            if let Some(splices) = sel.splices.get(&path) {
                match splice_text(&before, splices) {
                    Ok(t) => text = t,
                    Err(e) => errors.push(format!("{rel}: {e}")),
                }
            }
        }
        if spec.format {
            let formatted = match path.extension().and_then(|e| e.to_str()) {
                Some("rs") if wanted(Tool::Clippy) => {
                    let dir = path.parent().unwrap_or(&spec.root);
                    Some(("rustfmt", codecraft_cargo::rustfmt(&text, edition_for(spec, &path), dir).map_err(|e| e.to_string())))
                }
                Some("py" | "pyi") => ruff.as_ref().map(|r| {
                    let name = path.display().to_string();
                    ("ruff format", super::linters::pipe_through(r, &["format", "--stdin-filename", &name, "-"], &spec.root, &text))
                }),
                Some("zig") => zig.as_ref().map(|z| ("zig fmt", super::linters::pipe_through(z, &["fmt", "--stdin"], &spec.root, &text))),
                _ => None,
            };
            match formatted {
                Some((_, Ok(t))) => text = Document::from_text(&t).text(),
                // One message per formatter: a syntax error elsewhere should not flood.
                Some((tool, Err(e))) if format_failed.insert(tool) => {
                    errors.push(format!("{tool} {rel}: {}", e.lines().next().unwrap_or("")));
                }
                Some((_, Err(_))) | None => {}
            }
        }
        if text != before {
            out.push(Rewrite { path, before, after: text });
        }
    }
    (out, errors)
}

// ---------------------------------------------------------------- suppression

fn item_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?(?:(?:default|async|const|unsafe|extern(?:\s+"[^"]*")?)\s+)*(?:fn|impl|struct|enum|trait|mod|static|const|type|union|macro_rules!)\b"#,
        )
        .ok()
    })
    .as_ref()
}

fn indent_width(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ' || *c == '\t').map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// The line of the innermost item enclosing `line` (the item line itself counts).
pub fn enclosing_item_line(text: &str, line: usize) -> Option<usize> {
    let re = item_re()?;
    let lines: Vec<&str> = text.split('\n').collect();
    let here = lines.get(line)?;
    let indent = if here.trim().is_empty() { usize::MAX } else { indent_width(here) };
    if re.is_match(here) {
        return Some(line);
    }
    (0..line).rev().find(|&l| {
        lines.get(l).is_some_and(|t| re.is_match(t) && indent_width(t) < indent)
    })
}

/// The change that suppresses a finding: `#[allow(lint)]` on the enclosing Rust item,
/// `# noqa: CODE` (ruff), `// eslint-disable-next-line rule`, `// NOLINT(check)` (clang-tidy).
pub fn suppress_change(f: &Finding, text: &str) -> Result<TextChange, String> {
    let code = f.code.as_deref().ok_or_else(|| format!("'{}' cannot be suppressed", f.inspection))?;
    let lines: Vec<&str> = text.split('\n').collect();
    let line_text = |l: usize| lines.get(l).copied().ok_or_else(|| "the problem is outside the file".to_string());
    let at = |line: usize, column: usize, new_text: String| TextChange {
        path: f.path.clone(),
        start: Position::new(line, column),
        end: Position::new(line, column),
        new_text,
    };
    let indent_of = |l: &str| l.chars().take_while(|c| *c == ' ' || *c == '\t').collect::<String>();
    match f.tool {
        Tool::Clippy | Tool::CargoCheck => {
            let item = enclosing_item_line(text, f.start.line).ok_or("no enclosing item to put #[allow] on")?;
            let l = line_text(item)?;
            Ok(at(item, 0, format!("{}#[allow({code})]\n", indent_of(l))))
        }
        Tool::Ruff => {
            let l = line_text(f.start.line)?;
            let end = l.chars().count();
            if let Some(i) = l.find("# noqa:") {
                let existing = l.get(i + 7..).unwrap_or("");
                if existing.split([',', ' ']).any(|c| c.trim() == code) {
                    return Err(format!("{code} is already suppressed on this line"));
                }
                Ok(at(f.start.line, end, format!(", {code}")))
            } else {
                Ok(at(f.start.line, end, format!("  # noqa: {code}")))
            }
        }
        Tool::Eslint => {
            let l = line_text(f.start.line)?;
            Ok(at(f.start.line, 0, format!("{}// eslint-disable-next-line {code}\n", indent_of(l))))
        }
        Tool::ClangTidy => {
            let l = line_text(f.start.line)?;
            Ok(at(f.start.line, l.chars().count(), format!("  // NOLINT({code})")))
        }
        Tool::ZigFmt => Err("zig fmt problems cannot be suppressed".into()),
        Tool::Mypy | Tool::Pyright => {
            let l = line_text(f.start.line)?;
            if l.contains("# type: ignore") || l.contains("# pyright: ignore") {
                return Err("this line already has an ignore comment".into());
            }
            let comment = crate::python_lint::suppress_comment(f.tool, code).ok_or("cannot be suppressed")?;
            Ok(at(f.start.line, l.chars().count(), comment))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::inspections::{Applicability, clippy};
    use codecraft_workspace::Severity;

    const MAIN: &str = "fn add(a: i32, b: i32) -> i32 {\n    return a + b;\n}\n\nfn main() {\n    let s = \"héllo wörld\"; let unused = 1;\n    let v: Vec<i32> = vec![1, 2, 3];\n    if v.len() == 0 { println!(\"{}\", s); }\n    let x = add(1, 2);\n    println!(\"{}\", x);\n}\n";

    fn fixture_findings() -> Vec<Finding> {
        let fs: Vec<Finding> = include_str!("fixtures/clippy.jsonl")
            .lines()
            .filter_map(|l| clippy::parse_line(l, Path::new("/work/app"), Tool::Clippy))
            .collect();
        crate::inspections::finalize(fs, |_| true).0
    }

    fn read_main(p: &Path) -> Option<String> {
        (p == Path::new("/work/app/src/main.rs")).then(|| MAIN.to_string())
    }

    #[test]
    fn clippy_suggestions_become_a_workspace_edit_in_utf16() {
        let fs = fixture_findings();
        let fixes: Vec<(usize, Fix)> = fs.iter().map(|f| (f.id, f.fixes[0].clone())).collect();
        let (edit, sel, _) = fixes_to_workspace_edit(&fixes, &read_main).unwrap();
        assert_eq!(sel.accepted, [0, 1, 2]);
        assert_eq!((sel.conflicts, sel.invalid), (0, 0));
        let Some(lt::DocumentChanges::Edits(edits)) = edit.document_changes else { panic!("edits") };
        assert_eq!(edits.len(), 1);
        assert!(edits[0].text_document.uri.as_str().ends_with("/work/app/src/main.rs"));
        let te: Vec<(u32, u32, u32, &str)> = edits[0]
            .edits
            .iter()
            .map(|e| match e {
                lt::OneOf::Left(t) => (t.range.start.line, t.range.start.character, t.range.end.character, t.new_text.as_str()),
                lt::OneOf::Right(a) => (a.text_edit.range.start.line, 0, 0, a.text_edit.new_text.as_str()),
            })
            .collect();
        // The needless_return pair, `_unused` (char column 31 = UTF-16 31: é and ö are BMP), is_empty.
        assert_eq!(te, [(1, 4, 16, "a + b"), (1, 16, 17, ""), (5, 31, 37, "_unused"), (7, 7, 19, "v.is_empty()")]);
        let after = splice_text(MAIN, &sel.splices[&PathBuf::from("/work/app/src/main.rs")]).unwrap();
        assert!(after.contains("    a + b\n}"));
        assert!(after.contains("let _unused = 1;"));
        assert!(after.contains("if v.is_empty() {"));
    }

    #[test]
    fn astral_chars_shift_utf16_but_not_char_columns() {
        let text = "let s = \"😀\"; let unused = 1;\n";
        let fix = Fix {
            title: "t".into(),
            applicability: Applicability::MachineApplicable,
            changes: vec![TextChange {
                path: "/a.rs".into(),
                start: Position::new(0, 17),
                end: Position::new(0, 23),
                new_text: "_unused".into(),
            }],
        };
        let (edit, _, _) = fixes_to_workspace_edit(&[(0, fix)], &|_| Some(text.to_string())).unwrap();
        let Some(lt::DocumentChanges::Edits(edits)) = edit.document_changes else { panic!() };
        let lt::OneOf::Left(t) = &edits[0].edits[0] else { panic!() };
        assert_eq!((t.range.start.character, t.range.end.character), (18, 24));
    }

    fn fix(id: usize, changes: &[(usize, usize, usize, usize, &str)]) -> (usize, Fix) {
        (id, Fix {
            title: format!("fix {id}"),
            applicability: Applicability::MachineApplicable,
            changes: changes
                .iter()
                .map(|&(l0, c0, l1, c1, t)| TextChange {
                    path: "/a.rs".into(),
                    start: Position::new(l0, c0),
                    end: Position::new(l1, c1),
                    new_text: t.into(),
                })
                .collect(),
        })
    }

    #[test]
    fn overlapping_duplicate_and_invalid_fixes() {
        let texts = BTreeMap::from([(PathBuf::from("/a.rs"), "abcdef\nghij\n".to_string())]);
        let sel = select(
            &[
                fix(0, &[(0, 1, 0, 3, "X")]),
                fix(1, &[(0, 2, 0, 4, "Y")]),            // overlaps 0
                fix(2, &[(0, 1, 0, 3, "X")]),            // same as 0
                fix(3, &[(1, 0, 1, 1, "G"), (0, 5, 0, 6, "F")]),
                fix(4, &[(5, 0, 5, 1, "?")]),            // outside
                fix(5, &[(0, 2, 0, 9, "?")]),            // column past the line end
                fix(6, &[(1, 3, 1, 1, "?")]),            // backwards
                fix(7, &[(1, 2, 1, 2, "+"), (1, 2, 1, 2, "-")]), // conflicts with itself
            ],
            &texts,
        );
        assert_eq!(sel.accepted, [0, 2, 3]);
        assert_eq!(sel.conflicts, 1);
        assert_eq!(sel.invalid, 4);
        let out = splice_text("abcdef\nghij\n", &sel.splices[&PathBuf::from("/a.rs")]).unwrap();
        assert_eq!(out, "aXdeF\nGhij\n");
        assert!(splice_text("ab", &[(0..9, String::new())]).is_err());
        assert!(splice_text("abc", &[(1..2, String::new()), (0..1, String::new())]).is_err());
    }

    #[test]
    fn remap_moves_findings_and_drops_hit_fixes() {
        let before = "a\nbb\ncc\n";
        let splices = vec![(0..0, "#[allow(x)]\n".to_string())];
        let after = splice_text(before, &splices).unwrap();
        let mut f1 = Finding::new(Tool::Clippy, "x", Severity::Warning, "/a.rs".into(), Position::new(1, 1));
        f1.end = Position::new(1, 2);
        f1.fixes.push(fix(0, &[(2, 0, 2, 2, "dd")]).1);
        let mut f2 = Finding::new(Tool::Clippy, "y", Severity::Warning, "/a.rs".into(), Position::new(0, 0));
        f2.id = 1;
        f2.fixes.push(fix(1, &[(0, 0, 0, 1, "z")]).1);
        let mut r = InspectionResults {
            kind: super::super::JobKind::Inspect,
            scope: super::super::Scope { kind: super::super::ScopeKind::Project, path: None },
            scope_label: String::new(),
            findings: vec![f1, f2],
            tools: Vec::new(),
            truncated: false,
            hashes: BTreeMap::new(),
            resolved: BTreeSet::new(),
        };
        remap(&mut r, Path::new("/a.rs"), before, &after, &splices);
        assert_eq!(r.findings[0].start, Position::new(2, 1));
        assert_eq!(r.findings[0].fixes[0].changes[0].start, Position::new(3, 0));
        // The insertion at 0 moves f2's start; its fix starting exactly there is dropped.
        assert_eq!(r.findings[1].start, Position::new(1, 0));
        assert!(r.findings[1].fixes.is_empty());
        assert_eq!(r.hashes.get(Path::new("/a.rs")), Some(&text_hash(&after)));
        // Replacements: offsets inside a replaced range move to its start.
        assert_eq!(map_offset(&[(2..5, "x".into())], 4), (2, true));
        assert_eq!(map_offset(&[(2..5, "x".into())], 6), (4, false));
    }

    #[test]
    fn rust_suppression_goes_on_the_enclosing_item() {
        let text = "/// Docs.\n#[inline]\npub(crate) fn add(a: i32) -> i32 {\n    return a;\n}\n\nimpl Foo {\n    pub async fn bar(&self) {\n        let unused = 1;\n    }\n}\n";
        let mut f = Finding::new(Tool::Clippy, "clippy::needless_return", Severity::Warning, "/a.rs".into(), Position::new(3, 4));
        f.code = Some("clippy::needless_return".into());
        let c = suppress_change(&f, text).unwrap();
        assert_eq!((c.start, c.new_text.as_str()), (Position::new(2, 0), "#[allow(clippy::needless_return)]\n"));
        f.start = Position::new(8, 12);
        f.code = Some("unused_variables".into());
        let c = suppress_change(&f, text).unwrap();
        assert_eq!((c.start, c.new_text.as_str()), (Position::new(7, 0), "    #[allow(unused_variables)]\n"));
        // On the item line itself.
        f.start = Position::new(7, 4);
        assert_eq!(suppress_change(&f, text).unwrap().start, Position::new(7, 0));
        f.code = None;
        assert!(suppress_change(&f, text).is_err());
        f.code = Some("x".into());
        f.start = Position::new(99, 0);
        assert!(suppress_change(&f, text).is_err());
        // Siblings at the same indentation are not the enclosing item.
        let t2 = "fn outer() {\n    fn inner() {}\n    let x = 1;\n}\n";
        assert_eq!(enclosing_item_line(t2, 2), Some(0));
        assert_eq!(enclosing_item_line("const X: u32 = 1;\n", 0), Some(0));
        assert_eq!(enclosing_item_line("let x = 1;\n", 0), None);
    }

    #[test]
    fn linter_suppressions() {
        let mut f = Finding::new(Tool::Ruff, "F401 unused-import", Severity::Warning, "/a.py".into(), Position::new(1, 7));
        f.code = Some("F401".into());
        let c = suppress_change(&f, "x = 1\nimport os\n").unwrap();
        assert_eq!((c.start, c.new_text.as_str()), (Position::new(1, 9), "  # noqa: F401"));
        let c = suppress_change(&f, "x = 1\nimport os  # noqa: E501\n").unwrap();
        assert_eq!(c.new_text, ", F401");
        assert!(suppress_change(&f, "x = 1\nimport os  # noqa: E501, F401\n").is_err());

        let mut f = Finding::new(Tool::Eslint, "no-unused-vars", Severity::Error, "/a.js".into(), Position::new(1, 8));
        f.code = Some("no-unused-vars".into());
        let c = suppress_change(&f, "function f() {\n  let x = 1;\n}\n").unwrap();
        assert_eq!((c.start, c.new_text.as_str()), (Position::new(1, 0), "  // eslint-disable-next-line no-unused-vars\n"));

        let mut f = Finding::new(Tool::ClangTidy, "c", Severity::Warning, "/a.c".into(), Position::new(0, 4));
        f.code = Some("bugprone-x".into());
        assert_eq!(suppress_change(&f, "int y = 0;\n").unwrap().new_text, "  // NOLINT(bugprone-x)");
        f.tool = Tool::ZigFmt;
        assert!(suppress_change(&f, "x").is_err());
    }
}
