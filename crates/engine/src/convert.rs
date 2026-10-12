//! Conversions between the text model (char offsets), LSP (line, UTF-16 column) and rustc
//! (1-based line, char column), and application of LSP workspace edits.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use codecraft_lsp::lsp_types as lt;
use codecraft_text::{Change, ChangeSet, Document, Edit, EditGroup, Position};
use codecraft_workspace::{Problem, ProblemSource, Severity};

use crate::{CommandError, Session};

pub(crate) fn lsp_pos(doc: &Document, offset: usize) -> lt::Position {
    let (line, character) = doc.offset_to_utf16(offset);
    lt::Position { line, character }
}

pub(crate) fn from_lsp_pos(doc: &Document, pos: &lt::Position) -> usize {
    doc.utf16_to_offset(pos.line, pos.character)
}

pub(crate) fn from_lsp_range(doc: &Document, r: &lt::Range) -> Range<usize> {
    let s = from_lsp_pos(doc, &r.start);
    let e = from_lsp_pos(doc, &r.end);
    s.min(e)..e.max(s)
}

pub(crate) fn lsp_range(doc: &Document, r: Range<usize>) -> lt::Range {
    lt::Range { start: lsp_pos(doc, r.start), end: lsp_pos(doc, r.end) }
}

/// Char column in `line_text` for a UTF-16 column.
pub(crate) fn utf16_col_to_char(line_text: &str, col16: u32) -> usize {
    let mut units = 0usize;
    let mut chars = 0usize;
    for c in line_text.chars() {
        let next = units + c.len_utf16();
        if next > col16 as usize {
            break;
        }
        units = next;
        chars += 1;
    }
    chars
}

/// LSP text edits → one change set (edits must not overlap, as the protocol requires).
pub(crate) fn text_edits_to_changes(doc: &Document, edits: &[lt::TextEdit]) -> Result<ChangeSet, CommandError> {
    let changes = edits.iter().map(|e| {
        let r = from_lsp_range(doc, &e.range);
        Change::new(r.start, r.end, e.new_text.clone())
    });
    ChangeSet::new(changes.collect())
        .map_err(|e| CommandError::Failed(format!("invalid edits from language server: {e}")))
}

pub(crate) fn severity(s: Option<lt::DiagnosticSeverity>) -> Severity {
    match s {
        Some(lt::DiagnosticSeverity::WARNING) => Severity::Warning,
        Some(lt::DiagnosticSeverity::INFORMATION) => Severity::Info,
        Some(lt::DiagnosticSeverity::HINT) => Severity::Hint,
        _ => Severity::Error,
    }
}

/// LSP diagnostic → problem. Uses the open document for exact columns; otherwise treats UTF-16
/// columns as char columns (exact for BMP text).
pub(crate) fn diagnostic_to_problem(path: &Path, doc: Option<&Document>, d: &lt::Diagnostic) -> Problem {
    let conv = |p: &lt::Position| match doc {
        Some(doc) => doc.offset_to_position(from_lsp_pos(doc, p)),
        None => Position::new(p.line as usize, p.character as usize),
    };
    let code = d.code.as_ref().map(|c| match c {
        lt::NumberOrString::Number(n) => n.to_string(),
        lt::NumberOrString::String(s) => s.clone(),
    });
    Problem {
        path: path.to_path_buf(),
        start: conv(&d.range.start),
        end: conv(&d.range.end),
        severity: severity(d.severity),
        message: d.message.clone(),
        code,
        source: ProblemSource::RustAnalyzer,
    }
}

/// Plain text of hover contents (Markdown kept as-is; the UI renders it).
pub(crate) fn hover_text(contents: &lt::HoverContents) -> String {
    fn marked(m: &lt::MarkedString) -> String {
        match m {
            lt::MarkedString::String(s) => s.clone(),
            lt::MarkedString::LanguageString(ls) => format!("```{}\n{}\n```", ls.language, ls.value),
        }
    }
    match contents {
        lt::HoverContents::Scalar(m) => marked(m),
        lt::HoverContents::Array(v) => v.iter().map(marked).collect::<Vec<_>>().join("\n\n"),
        lt::HoverContents::Markup(m) => m.value.clone(),
    }
}

pub(crate) fn documentation_text(d: &lt::Documentation) -> String {
    match d {
        lt::Documentation::String(s) => s.clone(),
        lt::Documentation::MarkupContent(m) => m.value.clone(),
    }
}

/// Strip snippet syntax and return the char offset of the final cursor (`$0`, else the first
/// tab stop), if the snippet has one.
pub(crate) fn snippet_with_cursor(s: &str) -> (String, Option<usize>) {
    for marker in ["$0", "${0}", "$1", "${1"] {
        if let Some(byte) = s.find(marker) {
            let before = strip_snippet(s.get(..byte).unwrap_or(""));
            let cursor = before.chars().count();
            return (strip_snippet(s), Some(cursor));
        }
    }
    (strip_snippet(s), None)
}

/// Strip LSP snippet syntax: `$0`, `$1`, `${1:text}` → `text`, `${1|a,b|}` → `a`, `\$` → `$`.
pub(crate) fn strip_snippet(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while let Some(&c) = chars.get(i) {
        if c == '\\'
            && let Some(&n) = chars.get(i + 1)
        {
            out.push(n);
            i += 2;
            continue;
        }
        if c == '$' {
            if chars.get(i + 1).is_some_and(|n| n.is_ascii_digit()) {
                i += 1;
                while chars.get(i).is_some_and(char::is_ascii_digit) {
                    i += 1;
                }
                continue;
            }
            if chars.get(i + 1) == Some(&'{') {
                // ${N:default} or ${N|a,b|} or ${N}
                let mut j = i + 2;
                while chars.get(j).is_some_and(char::is_ascii_digit) {
                    j += 1;
                }
                let mut depth = 1usize;
                let mut inner = String::new();
                let mut k = match chars.get(j) {
                    Some(':') | Some('|') => j + 1,
                    _ => j,
                };
                while let Some(&ch) = chars.get(k) {
                    if ch == '{' {
                        depth += 1;
                    } else if ch == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    inner.push(ch);
                    k += 1;
                }
                let inner = if chars.get(j) == Some(&'|') {
                    inner.trim_end_matches('|').split(',').next().unwrap_or_default().to_string()
                } else {
                    strip_snippet(&inner)
                };
                out.push_str(&inner);
                i = k + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

impl Session {
    /// Apply an LSP workspace edit across files (opening unopened files as documents).
    pub(crate) fn apply_workspace_edit(&mut self, edit: &lt::WorkspaceEdit) -> Result<usize, CommandError> {
        let mut per_file: BTreeMap<PathBuf, Vec<lt::TextEdit>> = BTreeMap::new();
        if let Some(changes) = &edit.changes {
            for (uri, edits) in changes {
                if let Some(path) = codecraft_lsp::uri_to_path(uri) {
                    per_file.entry(path).or_default().extend(edits.iter().cloned());
                }
            }
        }
        if let Some(doc_changes) = &edit.document_changes {
            let edits: Vec<&lt::TextDocumentEdit> = match doc_changes {
                lt::DocumentChanges::Edits(v) => v.iter().collect(),
                lt::DocumentChanges::Operations(ops) => ops
                    .iter()
                    .filter_map(|op| match op {
                        lt::DocumentChangeOperation::Edit(e) => Some(e),
                        lt::DocumentChangeOperation::Op(_) => None,
                    })
                    .collect(),
            };
            for e in edits {
                if let Some(path) = codecraft_lsp::uri_to_path(&e.text_document.uri) {
                    let list = per_file.entry(path).or_default();
                    for te in &e.edits {
                        match te {
                            lt::OneOf::Left(t) => list.push(t.clone()),
                            lt::OneOf::Right(a) => list.push(a.text_edit.clone()),
                        }
                    }
                }
            }
            if let lt::DocumentChanges::Operations(ops) = doc_changes {
                for op in ops {
                    if let lt::DocumentChangeOperation::Op(rop) = op {
                        self.apply_resource_op(rop)?;
                    }
                }
            }
        }
        let files = per_file.len();
        for (path, edits) in per_file {
            self.apply_text_edits_to_path(&path, &edits)?;
        }
        Ok(files)
    }

    fn apply_resource_op(&mut self, op: &lt::ResourceOp) -> Result<(), CommandError> {
        let io = |e: std::io::Error| CommandError::Failed(e.to_string());
        match op {
            lt::ResourceOp::Create(c) => {
                if let Some(p) = codecraft_lsp::uri_to_path(&c.uri) {
                    if let Some(dir) = p.parent() {
                        std::fs::create_dir_all(dir).map_err(io)?;
                    }
                    if !p.exists() {
                        std::fs::write(&p, "").map_err(io)?;
                    }
                }
            }
            lt::ResourceOp::Rename(r) => {
                if let (Some(a), Some(b)) =
                    (codecraft_lsp::uri_to_path(&r.old_uri), codecraft_lsp::uri_to_path(&r.new_uri))
                {
                    self.save_all()?;
                    std::fs::rename(&a, &b).map_err(io)?;
                    if let Some(id) = self.ws.doc_by_path(&a)
                        && let Some(od) = self.ws.doc_mut(id)
                    {
                        od.path = Some(b);
                    }
                }
            }
            lt::ResourceOp::Delete(d) => {
                if let Some(p) = codecraft_lsp::uri_to_path(&d.uri) {
                    if p.is_dir() {
                        std::fs::remove_dir_all(&p).map_err(io)?;
                    } else {
                        std::fs::remove_file(&p).map_err(io)?;
                    }
                }
            }
        }
        self.jobs_rescan();
        Ok(())
    }

    /// Apply LSP text edits to a file's document (loading it if needed) as one undo step.
    pub(crate) fn apply_text_edits_to_path(&mut self, path: &Path, edits: &[lt::TextEdit]) -> Result<(), CommandError> {
        let doc_id = match self.ws.doc_by_path(path) {
            Some(d) => d,
            None => {
                let text = codecraft_fs::read_text_file(path).map_err(|e| CommandError::Failed(e.to_string()))?;
                let id = self.ws.add_document(Some(path.to_path_buf()), &text);
                self.lsp_did_open(id);
                id
            }
        };
        let editor = self.ws.editor_for_doc(doc_id);
        let Some(od) = self.ws.doc_mut(doc_id) else { return Ok(()) };
        let changes = text_edits_to_changes(&od.doc, edits)?;
        let before = editor.and_then(|e| self.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
        let after = before.map(&changes);
        let Some(od) = self.ws.doc_mut(doc_id) else { return Ok(()) };
        od.doc
            .apply(&Edit { changes, selections: after.clone() }, &before, EditGroup::Standalone)
            .map_err(|e| CommandError::Failed(e.to_string()))?;
        if let Some(e) = editor {
            self.ws.set_selections(e, after);
            self.after_text_change(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_are_stripped() {
        assert_eq!(strip_snippet("foo($1)$0"), "foo()");
        assert_eq!(strip_snippet("foo(${1:a}, ${2:b})"), "foo(a, b)");
        assert_eq!(strip_snippet("${1|x,y|}"), "x");
        assert_eq!(strip_snippet("a\\$b"), "a$b");
        assert_eq!(strip_snippet("${1:Vec<${2:T}>}"), "Vec<T>");
        assert_eq!(strip_snippet("$"), "$");
        assert_eq!(strip_snippet("${"), "");
    }

    #[test]
    fn utf16_columns() {
        assert_eq!(utf16_col_to_char("a😀b", 3), 2);
        assert_eq!(utf16_col_to_char("a😀b", 2), 1);
        assert_eq!(utf16_col_to_char("abc", 99), 3);
    }
}
