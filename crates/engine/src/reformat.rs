//! Replacing a document's text with a reformatted version as a minimal edit: one change per
//! run of changed lines (so undo, incremental highlighting and LSP sync see only what moved),
//! with carets mapped through it. Inside a changed run a caret keeps its place among the
//! non-whitespace characters, which is exact for whitespace-only reformatting (most of rustfmt).

use codecraft_text::{Assoc, Change, ChangeSet, Document, Edit, Selection, SelectionSet};

/// The edit turning `doc`'s text into `new` (with `\r\n`/`\r` already or not yet normalized)
/// and the selections `sels` mapped onto the new text.
pub(crate) fn diff_edit(doc: &Document, new: &str, sels: &SelectionSet) -> Edit {
    let old = doc.text();
    let new = new.replace("\r\n", "\n").replace('\r', "\n");
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    let old_starts = line_starts(&old_chars);
    let new_starts = line_starts(&new_chars);
    let at = |starts: &[usize], line: usize, len: usize| starts.get(line).copied().unwrap_or(len);
    let mut hunks = Vec::new();
    for h in codecraft_vcs::lines::line_changes(&old, &new) {
        let (os, oe) = (at(&old_starts, h.old_start, old_chars.len()), at(&old_starts, h.old_end, old_chars.len()));
        let (ns, ne) = (at(&new_starts, h.start, new_chars.len()), at(&new_starts, h.end, new_chars.len()));
        if os > oe || ns > ne {
            return whole(doc, &new, sels);
        }
        hunks.push((os, oe, ns, ne));
    }
    let changes = hunks.iter().map(|&(os, oe, ns, ne)| Change::new(os, oe, slice(&new_chars, ns, ne))).collect();
    let Ok(changes) = ChangeSet::new(changes) else { return whole(doc, &new, sels) };
    if changes.len_after(old_chars.len()) != new_chars.len() {
        return whole(doc, &new, sels);
    }
    let map = |p: usize| -> usize {
        match hunks.iter().find(|(os, oe, ..)| *os <= p && p < *oe) {
            Some(&(os, _, ns, ne)) => {
                let before = old_chars.get(os..p).unwrap_or(&[]);
                let k = before.iter().filter(|c| !c.is_whitespace()).count();
                // On a token, or in a line's leading whitespace: stay before the next token.
                // Elsewhere in whitespace: stay right after the previous one.
                let ws = before.iter().rev().take_while(|c| c.is_whitespace()).count();
                let gap = before.get(before.len().saturating_sub(ws)..).unwrap_or(&[]);
                let next_token =
                    old_chars.get(p).is_some_and(|c| !c.is_whitespace()) || ws == before.len() || gap.contains(&'\n');
                place(new_chars.get(ns..ne).unwrap_or(&[]), k, next_token) + ns
            }
            None => changes.map_pos(p, Assoc::After),
        }
    };
    let selections = sels.transform(|s| Selection::new(map(s.anchor), map(s.head))).clamp(new_chars.len());
    Edit { changes, selections }
}

/// A single whole-text replacement (the diff could not be used); carets are clamped.
fn whole(doc: &Document, new: &str, sels: &SelectionSet) -> Edit {
    let len = new.chars().count();
    Edit { changes: ChangeSet::single(0, doc.len_chars(), new), selections: sels.clamp(len) }
}

/// Char offsets where each line starts (lines end after `\n`, as the line diff splits them).
fn line_starts(chars: &[char]) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(chars.iter().enumerate().filter(|(_, c)| **c == '\n').map(|(i, _)| i + 1));
    starts
}

fn slice(chars: &[char], a: usize, b: usize) -> String {
    chars.get(a..b).map(|s| s.iter().collect()).unwrap_or_default()
}

/// Where a caret preceded by `k` non-whitespace chars of a changed run lands in its new text:
/// before the next token (`next_token`), else just past the `k`-th non-whitespace char.
fn place(text: &[char], k: usize, next_token: bool) -> usize {
    let mut seen = 0;
    let mut after_kth = 0;
    for (i, c) in text.iter().enumerate() {
        if c.is_whitespace() {
            continue;
        }
        if seen == k && next_token {
            return i;
        }
        seen += 1;
        if seen == k {
            after_kth = i + 1;
        }
    }
    if seen >= k && !next_token { after_kth } else { text.len() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_text::EditGroup;

    fn reformat(old: &str, new: &str, carets: &[usize]) -> (String, Vec<usize>, usize) {
        let mut doc = Document::from_text(old);
        let sels = SelectionSet::from_vec(carets.iter().map(|c| Selection::caret(*c)).collect(), 0);
        let edit = diff_edit(&doc, new, &sels);
        let n = edit.changes.changes().len();
        doc.apply(&edit, &sels, EditGroup::Standalone).unwrap();
        (doc.text(), edit.selections.ranges().iter().map(|s| s.head).collect(), n)
    }

    #[test]
    fn carets_keep_their_token_through_whitespace_changes() {
        let old = "fn main(){\nlet x=1;\n    foo( x );\n}\n";
        let new = "fn main() {\n    let x = 1;\n    foo(x);\n}\n";
        // Carets: before `let`, after `x=` (before 1), on `x` in foo, end of file.
        let carets = [11, 17, 29, old.chars().count()];
        let (text, mapped, n) = reformat(old, new, &carets);
        assert_eq!(text, new);
        assert_eq!(n, 1, "one run of changed lines");
        let at = |i: usize| new.chars().skip(mapped[i]).take(3).collect::<String>();
        assert_eq!(at(0), "let");
        assert_eq!(at(1), "1;\n");
        assert_eq!(at(2), "x);");
        assert_eq!(mapped[3], new.chars().count());
    }

    #[test]
    fn untouched_lines_map_exactly_and_changes_are_minimal() {
        let old = "a\nbb  \ncc\nd\n";
        let new = "a\nbb\ncc\nd\n";
        let (text, mapped, n) = reformat(old, new, &[0, 8, 10]);
        assert_eq!(text, new);
        assert_eq!(n, 1);
        // `cc` and `d` moved left by the two removed spaces.
        assert_eq!(mapped, vec![0, 6, 8]);
        // A caret in trailing whitespace lands after the token it followed.
        let (_, mapped, _) = reformat(old, new, &[5]);
        assert_eq!(mapped, vec![4]);
        // One in re-indented leading whitespace lands before the line's first token.
        let (_, mapped, _) = reformat("{\n  x\n}\n", "{\n    x\n}\n", &[3]);
        assert_eq!(mapped, vec![6]);
    }

    #[test]
    fn identical_text_and_insertions_and_crlf() {
        let (text, mapped, n) = reformat("x\n", "x\n", &[1]);
        assert_eq!((text.as_str(), mapped, n), ("x\n", vec![1], 0));
        // An inserted line before the caret's line pushes it down.
        let (text, mapped, _) = reformat("use a;\nfn f() {}\n", "use a;\n\nfn f() {}\n", &[7]);
        assert_eq!(text, "use a;\n\nfn f() {}\n");
        assert_eq!(mapped, vec![8]);
        let (text, _, _) = reformat("a\nb\n", "a\r\nc\r\n", &[0]);
        assert_eq!(text, "a\nc\n");
    }

    #[test]
    fn hostile_inputs_do_not_panic() {
        for (old, new) in [("", "x"), ("x", ""), ("😀\n\n", "\n😀"), ("a\n", "a"), ("a", "a\n"), ("\r", "\n\n")] {
            for caret in 0..=old.chars().count() + 2 {
                let mut doc = Document::from_text(old);
                let sels = SelectionSet::caret(caret);
                let edit = diff_edit(&doc, new, &sels);
                doc.apply(&edit, &sels.clamp(doc.len_chars()), EditGroup::Standalone).unwrap();
                assert_eq!(doc.text(), new.replace("\r\n", "\n").replace('\r', "\n"));
            }
        }
    }
}
