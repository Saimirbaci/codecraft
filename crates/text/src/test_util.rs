//! Test helpers: documents with selections written inline.
//!
//! `|` is a caret, `«abc»` a forward selection (anchor at `«`, head at `»`) and `»abc«` a
//! backward one. The first selection written is the primary unless a spec says otherwise.

use crate::{Document, Edit, EditGroup, Selection, SelectionSet};

pub(crate) fn parse(spec: &str) -> (Document, SelectionSet) {
    let mut text = String::new();
    let mut sels = Vec::new();
    let mut open: Option<(char, usize)> = None;
    let mut n = 0;
    for c in spec.chars() {
        match c {
            '|' => sels.push(Selection::caret(n)),
            '«' | '»' => match open.take() {
                None => open = Some((c, n)),
                Some(('«', at)) => sels.push(Selection::new(at, n)),
                Some((_, at)) => sels.push(Selection::new(n, at)),
            },
            _ => {
                text.push(c);
                n += 1;
            }
        }
    }
    assert!(open.is_none(), "unbalanced selection markers in {spec:?}");
    let set = if sels.is_empty() { SelectionSet::caret(0) } else { SelectionSet::from_vec(sels, 0) };
    (Document::from_text(&text), set)
}

pub(crate) fn render(doc: &Document, sels: &SelectionSet) -> String {
    // (offset, closes a selection first, marker); at one offset closing markers go first.
    let mut marks: Vec<(usize, u8, char)> = Vec::new();
    for s in sels.ranges() {
        if s.is_empty() {
            marks.push((s.head, 1, '|'));
        } else {
            marks.push((s.anchor, u8::from(s.anchor != s.end()), '«'));
            marks.push((s.head, u8::from(s.head != s.end()), '»'));
        }
    }
    marks.sort_by_key(|m| (m.0, m.1));
    let mut out = String::new();
    let mut it = marks.iter().peekable();
    for (i, c) in doc.text().chars().enumerate() {
        while let Some((_, _, m)) = it.next_if(|(at, _, _)| *at == i) {
            out.push(*m);
        }
        out.push(c);
    }
    for (_, _, m) in it {
        out.push(*m);
    }
    out
}

/// Run an op on `spec`, apply it, check that undo restores the text, and render the result.
pub(crate) fn run(spec: &str, op: impl FnOnce(&Document, &SelectionSet) -> Edit) -> String {
    let (mut doc, sels) = parse(spec);
    let original = doc.text();
    let edit = op(&doc, &sels);
    doc.apply(&edit, &sels, EditGroup::Standalone).unwrap();
    let out = render(&doc, &edit.selections);
    if !edit.changes.is_empty() {
        assert_eq!(doc.undo(), Some(sels.clone()));
        assert_eq!(doc.text(), original, "undo of {spec:?}");
        doc.redo();
    }
    out
}

/// Run a selection-only function on `spec` and render the result.
pub(crate) fn sel(spec: &str, f: impl FnOnce(&Document, &SelectionSet) -> SelectionSet) -> String {
    let (doc, sels) = parse(spec);
    render(&doc, &f(&doc, &sels))
}

#[test]
fn parse_and_render_roundtrip() {
    for spec in ["a|b", "«ab»c", "»ab«c", "|", "x|\n|y", "«a»«b»", "a«b»|"] {
        let (doc, sels) = parse(spec);
        let expected = if spec == "a«b»|" { "a«b»" } else { spec };
        assert_eq!(render(&doc, &sels), expected);
    }
}
