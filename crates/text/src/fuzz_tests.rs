//! Deterministic randomized tests: hundreds of random ops on random documents with random
//! (often out-of-range) selections. Nothing may panic, every edit must be well-formed, and undo
//! must walk back through every intermediate text to the original.

use crate::movement::{self, Motion};
use crate::ops::{self, OpsConfig};
use crate::search::{self, SearchQuery};
use crate::{Document, Edit, EditGroup, Selection, SelectionSet};

/// xorshift64*: tiny, deterministic, good enough to pick test inputs.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const PIECES: &[&str] = &[
    "a", "b", "Z", "_", "x1", "fn", "let", " ", "  ", "    ", "\t", "\n", "\n", "\n    ", "\r\n", "(", ")", "{", "}",
    "[", "]", "\"", "'", "&", "<", ">", "/", "//", "/*", "*/", ".", ",", ";", "é", "ß", "😀", "👍🏽", "e\u{301}", "🇫🇷",
    "\u{2028}", "\u{c}",
];

const TYPED: &[char] =
    &['a', 'Z', '_', ' ', '\t', '\n', '\r', '(', ')', '[', ']', '{', '}', '"', '\'', '/', '*', 'é', '😀', '\u{301}'];

fn random_text(rng: &mut Rng, pieces: usize) -> String {
    (0..pieces).map(|_| *rng.pick(PIECES)).collect()
}

/// An offset that is usually valid, sometimes just past the end, occasionally absurd.
fn offset(rng: &mut Rng, len: usize) -> usize {
    match rng.below(20) {
        0 => usize::MAX,
        1 => len + rng.below(10),
        _ => rng.below(len + 1),
    }
}

fn random_sels(rng: &mut Rng, len: usize) -> SelectionSet {
    let n = 1 + rng.below(4);
    let ranges = (0..n)
        .map(|_| {
            let a = offset(rng, len);
            let goal = if rng.below(4) == 0 { Some(rng.below(12)) } else { None };
            let h = if rng.below(2) == 0 { a } else { offset(rng, len) };
            Selection { anchor: a, head: h, goal_column: goal }
        })
        .collect();
    SelectionSet::from_vec(ranges, rng.below(n + 2))
}

fn random_query(rng: &mut Rng) -> SearchQuery {
    let patterns = ["a", "é", "😀", "fn", "\\w+", "^\\s+", "(a|b)", "x*", "[", "(", "/", ".", "\\b"];
    SearchQuery {
        pattern: (*rng.pick(&patterns)).to_string(),
        case_sensitive: rng.below(2) == 0,
        whole_word: rng.below(3) == 0,
        regex: rng.below(2) == 0,
    }
}

fn random_cfg(rng: &mut Rng) -> OpsConfig {
    match rng.below(4) {
        0 => OpsConfig { indent_unit: "\t".into(), ..OpsConfig::default() },
        1 => OpsConfig { auto_close: false, tab_width: 0, ..OpsConfig::default() },
        2 => OpsConfig { indent_unit: String::new(), line_comment: None, block_comment: None, ..OpsConfig::default() },
        _ => OpsConfig::default(),
    }
}

/// One random editing operation (`None` when the op declines, e.g. moving lines at an edge).
fn random_op(rng: &mut Rng, doc: &Document, sels: &SelectionSet) -> Option<Edit> {
    let cfg = random_cfg(rng);
    Some(match rng.below(27) {
        0 => ops::insert_text(doc, sels, &random_text(rng, 3)),
        1..=4 => ops::type_char(doc, sels, *rng.pick(TYPED), &cfg),
        5 | 6 => ops::backspace(doc, sels, &cfg),
        7 => ops::delete_forward(doc, sels),
        8 => ops::delete_word_backward(doc, sels),
        9 => ops::delete_word_forward(doc, sels),
        10 => ops::newline(doc, sels, &cfg),
        11 => ops::start_new_line(doc, sels),
        12 => ops::indent(doc, sels, &cfg),
        13 => ops::unindent(doc, sels, &cfg),
        14 => ops::toggle_line_comment(doc, sels, &cfg),
        15 => ops::toggle_block_comment(doc, sels, &cfg),
        16 => ops::duplicate(doc, sels),
        17 => ops::delete_line(doc, sels),
        18 => ops::move_lines(doc, sels, rng.below(2) == 0)?,
        19 => ops::join_lines(doc, sels),
        20 => ops::sort_lines(doc, sels),
        21 => ops::reverse_lines(doc, sels),
        22 => ops::toggle_case(doc, sels),
        23 => ops::transpose(doc, sels),
        24 => ops::cut(doc, sels).2,
        25 => ops::paste(doc, sels, &random_text(rng, 4), rng.below(2) == 0),
        _ => search::replace_all(doc, &random_query(rng), &random_text(rng, 2), sels).ok()?,
    })
}

fn random_selection_change(rng: &mut Rng, doc: &Document, sels: &SelectionSet) -> SelectionSet {
    let motions = [
        Motion::Left,
        Motion::Right,
        Motion::Up,
        Motion::Down,
        Motion::WordLeft,
        Motion::WordRight,
        Motion::LineStart,
        Motion::LineEnd,
        Motion::DocStart,
        Motion::DocEnd,
        Motion::PageUp(3),
        Motion::PageDown(3),
    ];
    match rng.below(12) {
        0..=3 => random_sels(rng, doc.len_chars()),
        4 => movement::extend_selection(doc, sels),
        5 => movement::add_caret_vertical(doc, sels, rng.below(2) == 0),
        6 => movement::add_next_occurrence(doc, sels),
        7 => movement::select_all_occurrences(doc, sels),
        8 => movement::add_carets_to_line_ends(doc, sels),
        9 => movement::unselect_occurrence(doc, sels),
        _ => movement::move_selections(doc, sels, *rng.pick(&motions), rng.below(2) == 0),
    }
}

fn assert_well_formed(doc: &Document, sels: &SelectionSet) {
    assert!(!sels.is_empty());
    assert!(sels.primary_index() < sels.len());
    let mut prev_end = 0;
    for (i, s) in sels.ranges().iter().enumerate() {
        assert!(s.end() <= doc.len_chars(), "selection {s:?} past {}", doc.len_chars());
        if i > 0 {
            assert!(s.start() >= prev_end, "unsorted or overlapping {sels:?}");
        }
        prev_end = s.end();
    }
}

#[test]
fn random_edits_undo_back_to_the_original() {
    for seed in 1..=24u64 {
        let mut rng = Rng::new(seed);
        let original = random_text(&mut rng, 60);
        let mut doc = Document::from_text(&original);
        let start = doc.text();
        let mut sels = random_sels(&mut rng, doc.len_chars());
        let mut history = vec![start.clone()];
        for _ in 0..300 {
            if rng.below(3) == 0 {
                sels = random_selection_change(&mut rng, &doc, &sels);
                assert_well_formed(&doc, &sels.clamp(doc.len_chars()));
            }
            let Some(edit) = random_op(&mut rng, &doc, &sels) else { continue };
            let len = doc.len_chars();
            assert!(edit.changes.check_bounds(len).is_ok(), "seed {seed}: {edit:?}");
            let len_after = edit.changes.len_after(len);
            doc.apply(&edit, &sels, EditGroup::Standalone).unwrap();
            assert_eq!(doc.len_chars(), len_after, "seed {seed}");
            assert_well_formed(&doc, &edit.selections);
            if !edit.changes.is_empty() {
                history.push(doc.text());
            }
            sels = edit.selections;
        }
        // Guard against the test degenerating into no-ops.
        assert!(history.len() > 150, "seed {seed}: only {} edits changed the text", history.len() - 1);
        let end = doc.text();
        // Every undo step lands exactly on the text before that edit.
        while doc.undo().is_some() {
            history.pop();
            assert_eq!(Some(&doc.text()), history.last(), "seed {seed}");
        }
        assert_eq!(doc.text(), start, "seed {seed}");
        assert!(!doc.is_modified());
        while doc.redo().is_some() {}
        assert_eq!(doc.text(), end, "seed {seed}");
    }
}

#[test]
fn random_typing_groups_undo_to_the_original() {
    for seed in 100..=116u64 {
        let mut rng = Rng::new(seed);
        let mut doc = Document::from_text(&random_text(&mut rng, 40));
        let start = doc.text();
        let mut sels = SelectionSet::caret(rng.below(doc.len_chars() + 1));
        for _ in 0..400 {
            if rng.below(10) == 0 {
                sels = random_selection_change(&mut rng, &doc, &sels);
                if rng.below(2) == 0 {
                    doc.break_undo_group();
                }
            }
            let group = if rng.below(4) == 0 { EditGroup::Standalone } else { EditGroup::Typing };
            let Some(edit) = random_op(&mut rng, &doc, &sels) else { continue };
            doc.apply(&edit, &sels, group).unwrap();
            sels = edit.selections;
            if rng.below(25) == 0 {
                doc.mark_saved();
            }
        }
        while doc.undo().is_some() {}
        assert_eq!(doc.text(), start, "seed {seed}");
    }
}

#[test]
fn queries_and_motions_never_panic_on_hostile_input() {
    for seed in 200..=260u64 {
        let mut rng = Rng::new(seed);
        let pieces = rng.below(30);
        let doc = Document::from_text(&random_text(&mut rng, pieces));
        for _ in 0..40 {
            let sels = random_sels(&mut rng, doc.len_chars());
            let len = doc.len_chars();
            for motion in [Motion::Left, Motion::Right, Motion::Up, Motion::Down, Motion::WordLeft, Motion::WordRight] {
                assert_well_formed(&doc, &movement::move_selections(&doc, &sels, motion, rng.below(2) == 0));
            }
            for motion in [Motion::LineStart, Motion::LineEnd, Motion::PageUp(usize::MAX), Motion::PageDown(usize::MAX)]
            {
                assert_well_formed(&doc, &movement::move_selections(&doc, &sels, motion, true));
            }
            let at = offset(&mut rng, len);
            let word = movement::word_range_at(&doc, at);
            assert!(word.start <= word.end && word.end <= len);
            let line = movement::select_line(&doc, offset(&mut rng, len));
            assert!(line.end() <= len);
            assert!(movement::select_word_at(&doc, at).end() <= len);
            let (clip, _) = ops::copy_text(&doc, &sels);
            assert!(clip.chars().count() <= len + sels.len() * 2);
            let query = random_query(&mut rng);
            if let Ok(found) = search::find_all(&doc, &query, 50) {
                assert!(found.iter().all(|r| r.start < r.end && r.end <= len));
            }
            if let Ok(Some(r)) = search::find_next(&doc, &query, at, rng.below(2) == 0, true) {
                assert!(r.start < r.end && r.end <= len);
            }
        }
    }
}

#[test]
fn keystroke_ops_on_a_large_document() {
    // 100k lines; the per-keystroke ops must work locally (and quickly) at both ends.
    let text = "    let x = foo(bar, \"baz\");\n".repeat(100_000);
    let mut doc = Document::from_text(&text);
    let cfg = OpsConfig::default();
    let mut sels = SelectionSet::from_vec(vec![Selection::caret(4), Selection::caret(doc.len_chars() - 1)], 0);
    for ch in "fn x() {".chars() {
        let edit = ops::type_char(&doc, &sels, ch, &cfg);
        doc.apply(&edit, &sels, EditGroup::Typing).unwrap();
        sels = edit.selections;
    }
    for edit_fn in [ops::newline, ops::backspace] {
        let edit = edit_fn(&doc, &sels, &cfg);
        doc.apply(&edit, &sels, EditGroup::Typing).unwrap();
        sels = edit.selections;
    }
    let edit = ops::delete_forward(&doc, &sels);
    doc.apply(&edit, &sels, EditGroup::Typing).unwrap();
    assert_eq!(doc.len_lines(), 100_003);
    assert!(doc.line_text(0).is_some_and(|l| l.starts_with("    fn x() {")));
    while doc.undo().is_some() {}
    assert_eq!(doc.text(), text);
}
