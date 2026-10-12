//! IntelliJ-style fuzzy matching (Go to File, Search Everywhere).
//!
//! A Smith-Waterman-like dynamic program over (pattern char, candidate char) with affine gap
//! penalties, in the spirit of fzf's v2 algorithm: every candidate position carries a bonus
//! (candidate start, after a path separator, after other punctuation, camel hump), consecutive
//! matches inherit the bonus of the run's first char, and matches in the file name (after the
//! last `/` or `\`) are worth much more than matches in directories. The score-only pass keeps
//! one column of state per pattern char and allocates nothing; indices come from a second pass
//! that records predecessors, run only for results that are actually shown.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::FuzzyMatch;

type Score = i64;

/// "No alignment" marker. Far from any real score, and kept exactly here (never drifting), so
/// arithmetic on it cannot overflow.
const NEG: Score = Score::MIN / 4;

const SCORE_MATCH: Score = 16;
const BONUS_START: Score = 10;
const BONUS_SEPARATOR: Score = 10;
const BONUS_WORD: Score = 9;
const BONUS_CAMEL: Score = 8;
const BONUS_CONSECUTIVE: Score = 5;
const FIRST_CHAR_MULTIPLIER: Score = 2;
const BONUS_IN_NAME: Score = 12;
const BONUS_EXACT_CASE: Score = 1;
const GAP_OPEN: Score = 3;
const GAP_EXTEND: Score = 1;
/// The pattern's last segment is a prefix of the file name, …
const BONUS_NAME_PREFIX: Score = 40;
/// … followed by a `.` or the end of the name (`main` → `main.rs`), …
const BONUS_NAME_STEM: Score = 30;
/// … or the whole name (`main.rs` → `main.rs`).
const BONUS_NAME_EXACT: Score = 30;
/// Predecessor matrix cap for the indices pass (4 MiB); beyond it indices come from a greedy pass.
const MAX_MATRIX_CELLS: usize = 1 << 20;
const NO_PRED: u32 = u32::MAX;

#[derive(Clone, Debug)]
struct Row {
    /// Case-folded pattern char (`/` for separator rows).
    folded: char,
    original: char,
    /// A `/` in the pattern: matches `/` or `\` and ends a pattern segment.
    slash: bool,
    /// Leftmost position this row can match at (greedy prefilter).
    first_pos: usize,
    // DP state for the previous candidate column:
    /// Best score with this pattern char matched exactly at the previous column.
    m: Score,
    /// Bonus of the first char of the run that ends at `m`.
    run: Score,
    /// Best score with this char matched earlier and a gap up to the current column.
    gap: Score,
    gap_from: usize,
}

impl Row {
    fn new(folded: char, original: char, slash: bool) -> Self {
        Self { folded, original, slash, first_pos: 0, m: NEG, run: 0, gap: NEG, gap_from: 0 }
    }

    #[inline]
    fn accepts(&self, folded: char, sep: bool) -> bool {
        if self.slash { sep } else { self.folded == folded }
    }
}

/// What the prefilter pass learns about a candidate.
struct Prepared {
    /// First column where pattern char 0 can match, its byte offset and the char before it.
    start: usize,
    start_byte: usize,
    start_prev: Option<char>,
    /// Last column where the final pattern char can match.
    end: usize,
    /// First column (and byte) of the file name, after the last separator.
    name_start: usize,
    name_start_byte: usize,
    len_chars: usize,
    /// The candidate is ASCII, so columns are bytes.
    ascii: bool,
}

/// A compiled fuzzy pattern with reusable buffers: build once, match many candidates.
#[derive(Clone, Debug)]
pub struct FuzzyMatcher {
    rows: Vec<Row>,
    has_slash: bool,
    /// Row index where the pattern's last segment starts.
    last_segment: usize,
    pred: Vec<u32>,
}

impl FuzzyMatcher {
    /// Compile `pattern`. Whitespace is ignored, `\` is read as `/`, leading separators are
    /// dropped and repeated ones collapsed.
    pub fn new(pattern: &str) -> Self {
        let mut rows: Vec<Row> = Vec::with_capacity(pattern.len().min(256));
        for c in pattern.chars() {
            if c.is_whitespace() {
                continue;
            }
            if is_sep(c) {
                if rows.last().is_some_and(|r| !r.slash) {
                    rows.push(Row::new('/', '/', true));
                }
            } else {
                rows.push(Row::new(fold(c), c, false));
            }
        }
        let has_slash = rows.iter().any(|r| r.slash);
        let last_segment = rows.iter().rposition(|r| r.slash).map_or(0, |i| i.saturating_add(1));
        Self { rows, has_slash, last_segment, pred: Vec::new() }
    }

    /// Whether the pattern is empty (after dropping whitespace), so it matches everything.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The score [`FuzzyMatcher::fuzzy_match`] would report, without computing indices.
    pub fn score(&mut self, candidate: &str) -> Option<i64> {
        if self.rows.is_empty() {
            return Some(0);
        }
        let prep = self.prefilter(candidate)?;
        let (best, _) = self.dp::<false>(candidate, &prep)?;
        Some(self.finish(candidate, &prep, best))
    }

    /// Match `candidate`, returning its score and the matched char indices.
    pub fn fuzzy_match(&mut self, candidate: &str) -> Option<FuzzyMatch> {
        if self.rows.is_empty() {
            return Some(FuzzyMatch { score: 0, indices: Vec::new() });
        }
        let prep = self.prefilter(candidate)?;
        let cells = self.rows.len().checked_mul(prep.len_chars).filter(|&c| c <= MAX_MATRIX_CELLS);
        let record = cells.is_some() && u32::try_from(prep.len_chars).is_ok();
        let (best, end_col) =
            if record { self.dp::<true>(candidate, &prep)? } else { self.dp::<false>(candidate, &prep)? };
        let indices = if record { self.backtrack(end_col, prep.len_chars) } else { None };
        let indices = match indices {
            Some(indices) => indices,
            None => self.greedy_indices(candidate)?,
        };
        Some(FuzzyMatch { score: self.finish(candidate, &prep, best), indices })
    }

    /// Checks the pattern is a subsequence of the candidate (ignoring segment rules), and finds
    /// the DP's column range and the file-name start. Most candidates are rejected here.
    fn prefilter(&mut self, candidate: &str) -> Option<Prepared> {
        if candidate.is_ascii() { self.prefilter_ascii(candidate.as_bytes()) } else { self.prefilter_chars(candidate) }
    }

    /// Byte-level prefilter: greedy forward search per pattern char, then backward scans.
    fn prefilter_ascii(&mut self, bytes: &[u8]) -> Option<Prepared> {
        let mut pos = 0usize;
        for row in &mut self.rows {
            // A non-ASCII pattern char cannot match an ASCII candidate.
            let want = u8::try_from(row.folded).ok()?;
            let rest = bytes.get(pos..)?;
            let found = if row.slash {
                rest.iter().position(|&b| b == b'/' || b == b'\\')?
            } else {
                rest.iter().position(|&b| b.to_ascii_lowercase() == want)?
            };
            row.first_pos = pos.saturating_add(found);
            pos = row.first_pos.saturating_add(1);
        }
        let start = self.rows.first()?.first_pos;
        let last = self.rows.last()?;
        let want = u8::try_from(last.folded).ok()?;
        let end = if last.slash {
            bytes.iter().rposition(|&b| b == b'/' || b == b'\\')?
        } else {
            bytes.iter().rposition(|&b| b.to_ascii_lowercase() == want)?
        };
        let name_start = bytes.iter().rposition(|&b| b == b'/' || b == b'\\').map_or(0, |i| i.saturating_add(1));
        Some(Prepared {
            start,
            start_byte: start,
            start_prev: start.checked_sub(1).and_then(|i| bytes.get(i)).map(|&b| char::from(b)),
            end,
            name_start,
            name_start_byte: name_start,
            len_chars: bytes.len(),
            ascii: true,
        })
    }

    /// Char-level prefilter for non-ASCII candidates, in one pass.
    fn prefilter_chars(&mut self, candidate: &str) -> Option<Prepared> {
        let count = self.rows.len();
        let mut next = 0usize;
        let mut prep = Prepared {
            start: 0,
            start_byte: 0,
            start_prev: None,
            end: 0,
            name_start: 0,
            name_start_byte: 0,
            len_chars: 0,
            ascii: false,
        };
        let mut found_end = false;
        let mut prev = None;
        for (j, (byte, c)) in candidate.char_indices().enumerate() {
            let sep = is_sep(c);
            if sep {
                prep.name_start = j.saturating_add(1);
                prep.name_start_byte = byte.saturating_add(c.len_utf8());
            }
            let folded = fold(c);
            if let Some(row) = self.rows.get_mut(next)
                && row.accepts(folded, sep)
            {
                row.first_pos = j;
                if next == 0 {
                    prep.start = j;
                    prep.start_byte = byte;
                    prep.start_prev = prev;
                }
                next = next.saturating_add(1);
            }
            if next == count && self.rows.last().is_some_and(|r| r.accepts(folded, sep)) {
                prep.end = j;
                found_end = true;
            }
            prev = Some(c);
            prep.len_chars = j.saturating_add(1);
        }
        found_end.then_some(prep)
    }

    /// The dynamic program over columns `prep.start..=prep.end`. Returns the best score of an
    /// alignment of the whole pattern and the column of its last char.
    /// With `RECORD`, also fills the predecessor matrix for [`FuzzyMatcher::backtrack`].
    fn dp<const RECORD: bool>(&mut self, candidate: &str, prep: &Prepared) -> Option<(Score, usize)> {
        let tail = candidate.get(prep.start_byte..)?;
        if prep.ascii {
            self.dp_columns::<RECORD>(tail.bytes().map(char::from), prep)
        } else {
            self.dp_columns::<RECORD>(tail.chars(), prep)
        }
    }

    fn dp_columns<const RECORD: bool>(
        &mut self,
        columns: impl Iterator<Item = char>,
        prep: &Prepared,
    ) -> Option<(Score, usize)> {
        for row in &mut self.rows {
            row.m = NEG;
            row.run = 0;
            row.gap = NEG;
            row.gap_from = 0;
        }
        let width = prep.len_chars;
        if RECORD {
            self.pred.clear();
            self.pred.resize(self.rows.len().saturating_mul(width), NO_PRED);
        }
        let last = self.rows.len().checked_sub(1)?;
        let mut active = 0usize; // rows 0..=active can match at the current column
        let mut best = NEG;
        let mut best_col = 0usize;
        let mut prev = prep.start_prev;
        for (offset, c) in columns.enumerate() {
            let j = prep.start.saturating_add(offset);
            if j > prep.end {
                break;
            }
            while self.rows.get(active.saturating_add(1)).is_some_and(|r| r.first_pos <= j) {
                active = active.saturating_add(1);
            }
            let sep = is_sep(c);
            let folded = fold(c);
            let bonus = position_bonus(prev, c);
            let in_name = if j >= prep.name_start { BONUS_IN_NAME } else { 0 };
            // A separator inside a pattern segment's gap breaks the segment.
            let kill = self.has_slash && sep;

            // Previous-column state of row i - 1 (before this column overwrote it).
            let mut up_m = NEG;
            let mut up_run = 0;
            let mut up_gap = NEG;
            let mut up_gap_from = 0usize;
            for (i, row) in self.rows.iter_mut().enumerate().take(active.saturating_add(1)) {
                let (old_m, old_run, old_gap, old_gap_from) = (row.m, row.run, row.gap, row.gap_from);
                let mut new_m = NEG;
                let mut new_run = 0;
                let mut from = usize::MAX;
                if row.accepts(folded, sep) {
                    let case = if row.original == c { BONUS_EXACT_CASE } else { 0 };
                    let base = SCORE_MATCH + in_name + case;
                    if i == 0 {
                        new_m = base + bonus * FIRST_CHAR_MULTIPLIER;
                        new_run = bonus;
                    } else {
                        if up_m > NEG {
                            new_m = up_m + base + bonus.max(up_run).max(BONUS_CONSECUTIVE);
                            new_run = up_run.max(bonus);
                            from = j.saturating_sub(1);
                        }
                        if up_gap > NEG {
                            let gapped = up_gap + base + bonus;
                            if gapped > new_m {
                                new_m = gapped;
                                new_run = bonus;
                                from = up_gap_from;
                            }
                        }
                    }
                }
                // Gap state for the next column: extend the old gap or open one after the
                // previous column's match.
                let mut gap = (old_gap - GAP_EXTEND).max(NEG);
                let mut gap_from = old_gap_from;
                if old_m > NEG && old_m - GAP_OPEN >= gap {
                    gap = old_m - GAP_OPEN;
                    gap_from = j.saturating_sub(1);
                }
                if kill && !row.slash {
                    gap = NEG;
                }
                row.m = new_m;
                row.run = new_run;
                row.gap = gap;
                row.gap_from = gap_from;
                if RECORD
                    && new_m > NEG
                    && let Some(cell) =
                        i.checked_mul(width).and_then(|r| r.checked_add(j)).and_then(|k| self.pred.get_mut(k))
                {
                    *cell = u32::try_from(from).unwrap_or(NO_PRED);
                }
                if i == last && new_m > best {
                    best = new_m;
                    best_col = j;
                }
                up_m = old_m;
                up_run = old_run;
                up_gap = old_gap;
                up_gap_from = old_gap_from;
            }
            prev = Some(c);
        }
        (best > NEG).then_some((best, best_col))
    }

    fn backtrack(&self, end_col: usize, width: usize) -> Option<Vec<usize>> {
        let count = self.rows.len();
        let mut indices = vec![0usize; count];
        let mut j = end_col;
        for i in (0..count).rev() {
            *indices.get_mut(i)? = j;
            if i == 0 {
                break;
            }
            let pred = *self.pred.get(i.checked_mul(width)?.checked_add(j)?)?;
            if pred == NO_PRED || pred as usize >= j {
                return None;
            }
            j = pred as usize;
        }
        Some(indices)
    }

    /// Leftmost valid alignment, for candidates too long for the predecessor matrix.
    fn greedy_indices(&self, candidate: &str) -> Option<Vec<usize>> {
        if !self.has_slash {
            return Some(self.rows.iter().map(|r| r.first_pos).collect());
        }
        let chars: Vec<char> = candidate.chars().collect();
        let mut indices = Vec::with_capacity(self.rows.len());
        let mut pos = 0usize;
        let mut rows = self.rows.as_slice();
        while !rows.is_empty() {
            let seg_len = rows.iter().position(|r| r.slash).unwrap_or(rows.len());
            let (segment, rest) = rows.split_at(seg_len);
            // Try each candidate segment from `pos` on until one contains this pattern segment.
            loop {
                let seg_end =
                    chars.get(pos..)?.iter().position(|&c| is_sep(c)).map_or(chars.len(), |e| pos.saturating_add(e));
                let mut found = Vec::with_capacity(segment.len());
                let mut k = pos;
                for row in segment {
                    while k < seg_end && chars.get(k).is_some_and(|&c| fold(c) != row.folded) {
                        k = k.saturating_add(1);
                    }
                    if k >= seg_end {
                        break;
                    }
                    found.push(k);
                    k = k.saturating_add(1);
                }
                if found.len() == segment.len() && (rest.is_empty() || seg_end < chars.len()) {
                    indices.extend(found);
                    if !rest.is_empty() {
                        indices.push(seg_end);
                    }
                    pos = seg_end.saturating_add(1);
                    break;
                }
                if seg_end >= chars.len() {
                    return None;
                }
                pos = seg_end.saturating_add(1);
            }
            rows = rest.get(1..).unwrap_or(&[]);
        }
        Some(indices)
    }

    /// Whole-candidate adjustments: file-name prefix bonuses and the length penalty.
    fn finish(&self, candidate: &str, prep: &Prepared, best: Score) -> i64 {
        let name = candidate.get(prep.name_start_byte..).unwrap_or("");
        let len = Score::try_from(candidate.len()).unwrap_or(Score::MAX);
        let name_len = Score::try_from(name.len()).unwrap_or(Score::MAX);
        let penalty = len / 8 + name_len / 4;
        best.saturating_add(self.name_bonus(name)).saturating_sub(penalty)
    }

    fn name_bonus(&self, name: &str) -> Score {
        let segment = self.rows.get(self.last_segment..).unwrap_or(&[]);
        if segment.is_empty() {
            return 0;
        }
        let mut chars = name.chars();
        for row in segment {
            match chars.next() {
                Some(c) if fold(c) == row.folded => {}
                _ => return 0,
            }
        }
        match chars.next() {
            None => BONUS_NAME_PREFIX + BONUS_NAME_STEM + BONUS_NAME_EXACT,
            Some('.') => BONUS_NAME_PREFIX + BONUS_NAME_STEM,
            Some(_) => BONUS_NAME_PREFIX,
        }
    }
}

#[inline]
fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// Simple one-to-one case folding (chars whose lowercase is several chars stay as they are).
#[inline]
pub(crate) fn fold(c: char) -> char {
    if c.is_ascii() {
        return c.to_ascii_lowercase();
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(l), None) => l,
        _ => c,
    }
}

#[inline]
fn position_bonus(prev: Option<char>, c: char) -> Score {
    match prev {
        None => BONUS_START,
        Some(p) if is_sep(p) => BONUS_SEPARATOR,
        Some(p) if !p.is_alphanumeric() => {
            if c.is_alphanumeric() {
                BONUS_WORD
            } else {
                0
            }
        }
        Some(p) => {
            if (p.is_lowercase() && c.is_uppercase()) || (!p.is_numeric() && c.is_numeric()) {
                BONUS_CAMEL
            } else {
                0
            }
        }
    }
}

/// Orders results best first: higher score, then shorter candidate, then lower index.
type RankKey = (Reverse<Score>, usize, usize);

/// Below this many candidates ranking stays on the calling thread.
const PARALLEL_MIN_CANDIDATES: usize = 16 * 1024;
const MAX_RANK_THREADS: usize = 8;

pub(crate) fn rank<'a>(
    pattern: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> Vec<(usize, FuzzyMatch)> {
    if limit == 0 {
        return Vec::new();
    }
    let mut matcher = FuzzyMatcher::new(pattern);
    let candidates: Vec<&'a str> = candidates.into_iter().collect();
    let threads = if candidates.len() < PARALLEL_MIN_CANDIDATES {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_RANK_THREADS))
    };
    let mut top = if threads > 1 {
        let chunk = candidates.len().div_ceil(threads);
        std::thread::scope(|scope| {
            let parts: Vec<_> = candidates
                .chunks(chunk)
                .enumerate()
                .map(|(k, part)| {
                    let offset = k.saturating_mul(chunk);
                    let mut local = matcher.clone();
                    let spawned = std::thread::Builder::new()
                        .name("codecraft-rank".into())
                        .spawn_scoped(scope, move || best_of(&mut local, part, offset, limit));
                    (spawned, part, offset)
                })
                .collect();
            let mut merged = Vec::new();
            for (spawned, part, offset) in parts {
                // A thread that could not start (or died) is redone here.
                match spawned.map(|handle| handle.join()) {
                    Ok(Ok(found)) => merged.extend(found),
                    _ => merged.extend(best_of(&mut matcher.clone(), part, offset, limit)),
                }
            }
            merged
        })
    } else {
        best_of(&mut matcher, &candidates, 0, limit)
    };
    top.sort_unstable_by_key(|&(key, _)| key);
    top.truncate(limit);
    top.into_iter()
        .filter_map(|((_, _, index), candidate)| matcher.fuzzy_match(candidate).map(|m| (index, m)))
        .collect()
}

/// The best `limit` of `candidates` (whose first index is `offset`), unordered.
fn best_of<'a>(
    matcher: &mut FuzzyMatcher,
    candidates: &[&'a str],
    offset: usize,
    limit: usize,
) -> Vec<(RankKey, &'a str)> {
    // Max-heap on (worse score, longer, later index): the top is the current worst kept.
    let mut heap: BinaryHeap<(RankKey, &'a str)> = BinaryHeap::with_capacity(limit.min(4096));
    for (index, &candidate) in candidates.iter().enumerate() {
        let Some(score) = matcher.score(candidate) else { continue };
        let key = (Reverse(score), candidate.len(), offset.saturating_add(index));
        if heap.len() < limit {
            heap.push((key, candidate));
        } else if heap.peek().is_some_and(|(worst, _)| key < *worst) {
            heap.pop();
            heap.push((key, candidate));
        }
    }
    heap.into_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fuzzy_match, rank};

    fn indices(pattern: &str, candidate: &str) -> Vec<usize> {
        fuzzy_match(pattern, candidate).unwrap().indices
    }

    fn score(pattern: &str, candidate: &str) -> i64 {
        fuzzy_match(pattern, candidate).unwrap().score
    }

    fn ranked<'a>(pattern: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        rank(pattern, candidates.iter().copied(), usize::MAX).into_iter().map(|(i, _)| candidates[i]).collect()
    }

    #[test]
    fn empty_pattern_matches_everything() {
        assert_eq!(fuzzy_match("", "anything"), Some(FuzzyMatch { score: 0, indices: vec![] }));
        assert_eq!(fuzzy_match("  ", ""), Some(FuzzyMatch { score: 0, indices: vec![] }));
        assert_eq!(fuzzy_match("/", "x"), Some(FuzzyMatch { score: 0, indices: vec![] }));
    }

    #[test]
    fn no_match_is_none() {
        assert_eq!(fuzzy_match("xyz", "main.rs"), None);
        assert_eq!(fuzzy_match("a", ""), None);
        assert_eq!(fuzzy_match("srm", "main.rs"), None);
        assert_eq!(fuzzy_match("mainn", "main.rs"), None);
    }

    #[test]
    fn camel_humps() {
        assert_eq!(indices("FB", "FooBar.rs"), [0, 3]);
        assert_eq!(indices("fb", "FooBar.rs"), [0, 3]);
        assert!(score("FB", "FooBar.rs") > score("FB", "Foobar.rs"));
        assert_eq!(indices("hm", "src/HashMap.rs"), [4, 8]);
    }

    #[test]
    fn word_starts_after_punctuation() {
        assert_eq!(indices("fba", "foo_bar-baz.rs"), [0, 4, 5]);
        assert_eq!(indices("mrs", "main.rs"), [0, 5, 6]);
        assert!(score("mrs", "main.rs") > score("mrs", "macros.rs"));
    }

    #[test]
    fn prefers_consecutive_runs_in_the_name() {
        assert_eq!(indices("main", "src/main.rs"), [4, 5, 6, 7]);
        assert_eq!(indices("main", "m/a/i/n/main.rs"), [8, 9, 10, 11]);
        assert!(score("main", "src/main.rs") > score("main", "main/src/lib.rs"));
        assert!(score("lib", "src/lib.rs") > score("lib", "src/glib.rs"));
    }

    #[test]
    fn exact_case_is_a_slight_bonus() {
        assert!(score("Main", "Main.rs") > score("Main", "main.rs"));
        assert!(score("Main", "Main.rs") - score("Main", "main.rs") <= 4);
    }

    #[test]
    fn slash_patterns_match_segments_in_order() {
        assert_eq!(indices("src/ma", "src/main.rs"), [0, 1, 2, 3, 4, 5]);
        assert_eq!(indices("src/ma", "crates/src/foo/main.rs"), [7, 8, 9, 10, 15, 16]);
        assert_eq!(fuzzy_match("src/ma", "s/r/c/ma.rs"), None);
        assert_eq!(fuzzy_match("src/ma", "lib/main.rs"), None);
        assert_eq!(fuzzy_match("ma/src", "src/main.rs"), None);
        assert!(fuzzy_match("sc/main", "src/main.rs").is_some());
        assert!(fuzzy_match("src\\ma", "src/main.rs").is_some());
        assert!(fuzzy_match("src/ma", "src\\main.rs").is_some());
        assert!(fuzzy_match("/src//ma", "src/main.rs").is_some());
        // A trailing slash means "something under a matching directory".
        assert!(fuzzy_match("src/", "src/main.rs").is_some());
        assert_eq!(fuzzy_match("src/", "src.rs"), None);
        // Without a slash, matches may span segments.
        assert!(fuzzy_match("srcma", "src/main.rs").is_some());
    }

    #[test]
    fn whitespace_in_pattern_is_ignored() {
        assert_eq!(indices("main rs", "main.rs"), [0, 1, 2, 3, 5, 6]);
    }

    #[test]
    fn unicode_is_matched_by_char() {
        assert_eq!(indices("ä", "dir/Äpfel.txt"), [4]);
        assert_eq!(indices("ÄP", "dir/äpfel.txt"), [4, 5]);
        assert_eq!(indices("日本", "docs/日本語.md"), [5, 6]);
        assert_eq!(fuzzy_match("ü", "u.rs"), None);
    }

    #[test]
    fn exact_name_and_prefix_rank_first() {
        let candidates = [
            "crates/fs/src/glib.rs",
            "library/x.rs",
            "lib_helpers.rs",
            "crates/fs/src/lib.rs",
            "src/lib.rs",
            "libs.rs",
        ];
        let order = ranked("lib", &candidates);
        assert_eq!(&order[..4], ["src/lib.rs", "crates/fs/src/lib.rs", "libs.rs", "lib_helpers.rs"]);
        assert_eq!(order.len(), candidates.len());
        let with_backup = ["lib.rs.bak", "crates/fs/src/lib.rs", "src/lib.rs"];
        assert_eq!(ranked("lib.rs", &with_backup), ["src/lib.rs", "crates/fs/src/lib.rs", "lib.rs.bak"]);
    }

    #[test]
    fn name_matches_beat_directory_matches() {
        let candidates = ["main/src/lib.rs", "src/domain.rs", "src/main.rs", "src/bin/main_window.rs"];
        assert_eq!(
            ranked("main", &candidates),
            ["src/main.rs", "src/bin/main_window.rs", "src/domain.rs", "main/src/lib.rs"]
        );
    }

    #[test]
    fn rank_limits_and_breaks_ties() {
        let candidates = ["b/x.rs", "a/x.rs", "x.rs", "zz", "long/path/x.rs"];
        let all = rank("x", candidates.iter().copied(), 10);
        let order: Vec<usize> = all.iter().map(|(i, _)| *i).collect();
        assert_eq!(order, [2, 0, 1, 4]);
        assert_eq!(all[1].1.score, all[2].1.score, "same score: shorter, then lower index");
        let top: Vec<usize> = rank("x", candidates.iter().copied(), 2).iter().map(|(i, _)| *i).collect();
        assert_eq!(top, [2, 0]);
        assert!(rank("x", candidates.iter().copied(), 0).is_empty());
        let empty: Vec<usize> = rank("", candidates.iter().copied(), 10).iter().map(|(i, _)| *i).collect();
        assert_eq!(empty, [3, 2, 0, 1, 4]);
        assert!(rank("x", std::iter::empty(), 5).is_empty());
    }

    #[test]
    fn score_only_pass_agrees_with_indices_pass() {
        let mut m = FuzzyMatcher::new("src/ma");
        for c in ["src/main.rs", "crates/src/foo/main.rs", "a/src/x/m/a", "src/ma"] {
            assert_eq!(m.score(c), m.fuzzy_match(c).map(|f| f.score), "{c}");
        }
    }

    #[test]
    fn oversized_candidates_use_greedy_indices() {
        let long = format!("{}/src/{}/main.rs", "a".repeat(400_000), "b".repeat(400_000));
        let mut m = FuzzyMatcher::new("src/main");
        let found = m.fuzzy_match(&long).unwrap();
        assert_eq!(found.indices, [400_001, 400_002, 400_003, 400_004, 800_006, 800_007, 800_008, 800_009]);
        assert_eq!(Some(found.score), m.score(&long));
        let plain = FuzzyMatcher::new("ab").fuzzy_match(&"xab".repeat(500_000)).unwrap();
        assert_eq!(plain.indices, [1, 2]);
    }

    // --- property tests ------------------------------------------------------------------

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn string(&mut self, alphabet: &[char], max_len: usize) -> String {
            let len = self.below(max_len + 1);
            (0..len).map(|_| alphabet[self.below(alphabet.len())]).collect()
        }
    }

    const UNICODE: &[char] = &[
        'a',
        'b',
        'B',
        'c',
        'Z',
        '0',
        '9',
        '/',
        '\\',
        '.',
        '_',
        '-',
        ' ',
        'İ',
        'ß',
        'ẞ',
        'Σ',
        'σ',
        'ς',
        'ﬁ',
        '日',
        '\u{301}',
        '\u{0}',
        '\u{10FFFF}',
        '🦀',
        'Ä',
        'ä',
        '\t',
        '\n',
    ];

    /// Re-scores an alignment from scratch (independent of the DP); `None` if it is invalid.
    fn evaluate(pattern: &str, candidate: &str, indices: &[usize]) -> Option<Score> {
        let m = FuzzyMatcher::new(pattern);
        let chars: Vec<char> = candidate.chars().collect();
        if indices.len() != m.rows.len() || !indices.windows(2).all(|w| w[0] < w[1]) {
            return None;
        }
        let name_start = chars.iter().rposition(|&c| is_sep(c)).map_or(0, |i| i + 1);
        let mut total = 0;
        let mut run = 0;
        for (i, (&j, row)) in indices.iter().zip(&m.rows).enumerate() {
            let c = *chars.get(j)?;
            if !row.accepts(fold(c), is_sep(c)) {
                return None;
            }
            let bonus = position_bonus(j.checked_sub(1).map(|p| chars[p]), c);
            let base = SCORE_MATCH
                + if j >= name_start { BONUS_IN_NAME } else { 0 }
                + if row.original == c { BONUS_EXACT_CASE } else { 0 };
            if i == 0 {
                total += base + bonus * FIRST_CHAR_MULTIPLIER;
                run = bonus;
                continue;
            }
            let k = indices[i - 1];
            if m.has_slash && !m.rows[i - 1].slash && chars[k + 1..j].iter().any(|&c| is_sep(c)) {
                return None; // gap crosses a separator inside a pattern segment
            }
            if k + 1 == j {
                total += base + bonus.max(run).max(BONUS_CONSECUTIVE);
                run = run.max(bonus);
            } else {
                total += base + bonus - GAP_OPEN - GAP_EXTEND * (j - k - 2) as Score;
                run = bonus;
            }
        }
        Some(total)
    }

    /// Every increasing index tuple, for tiny inputs.
    fn feasible(pattern: &str, candidate: &str) -> bool {
        let rows = FuzzyMatcher::new(pattern).rows.len();
        let n = candidate.chars().count();
        let mut idx: Vec<usize> = (0..rows).collect();
        if rows > n {
            return false;
        }
        loop {
            if evaluate(pattern, candidate, &idx).is_some() {
                return true;
            }
            // next combination
            let mut i = rows;
            loop {
                if i == 0 {
                    return false;
                }
                i -= 1;
                if idx[i] < n - rows + i {
                    idx[i] += 1;
                    for k in i + 1..rows {
                        idx[k] = idx[k - 1] + 1;
                    }
                    break;
                }
            }
        }
    }

    #[test]
    fn random_unicode_never_panics_and_indices_are_consistent() {
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        let mut matched = 0;
        for _ in 0..20_000 {
            let pattern = rng.string(UNICODE, 6);
            let candidate = rng.string(UNICODE, 40);
            let mut matcher = FuzzyMatcher::new(&pattern);
            let result = matcher.fuzzy_match(&candidate);
            assert_eq!(matcher.score(&candidate), result.as_ref().map(|r| r.score));
            if let Some(r) = result {
                if matcher.is_empty() {
                    continue;
                }
                matched += 1;
                let n = candidate.chars().count();
                assert!(r.indices.iter().all(|&i| i < n));
                let raw = evaluate(&pattern, &candidate, &r.indices).expect("returned alignment is valid");
                let prep = matcher.prefilter(&candidate).unwrap();
                assert_eq!(r.score, matcher.finish(&candidate, &prep, raw));
            }
        }
        assert!(matched > 100, "the generator should produce some matches ({matched})");
    }

    #[test]
    fn dp_finds_a_match_exactly_when_one_exists() {
        let alphabet = ['a', 'b', 'A', '/', '\\', '.', '_'];
        let mut rng = XorShift(42);
        for _ in 0..5_000 {
            let pattern = rng.string(&alphabet, 4);
            let candidate = rng.string(&alphabet, 9);
            if FuzzyMatcher::new(&pattern).is_empty() {
                continue;
            }
            assert_eq!(
                fuzzy_match(&pattern, &candidate).is_some(),
                feasible(&pattern, &candidate),
                "{pattern:?} vs {candidate:?}"
            );
        }
    }

    #[test]
    fn hostile_inputs() {
        let long_pattern = "a".repeat(3_000);
        assert_eq!(fuzzy_match(&long_pattern, "aaaa"), None);
        assert_eq!(fuzzy_match(&long_pattern, &"a".repeat(3_000)).unwrap().indices.len(), 3_000);
        assert!(fuzzy_match("a", &"/".repeat(100_000)).is_none());
        assert!(fuzzy_match("/", "a/b").is_some());
        assert!(fuzzy_match("a/", "a").is_none());
    }

    fn synthetic_paths(count: usize) -> Vec<String> {
        let words = ["src", "crates", "engine", "workspace", "ui", "tests", "fixtures", "lsp", "cargo", "text"];
        let names = ["main", "lib", "mod", "session", "commands", "editor_view", "FileTree", "fuzzy", "find_in_files"];
        let exts = ["rs", "toml", "md", "json"];
        let mut rng = XorShift(7);
        (0..count)
            .map(|i| {
                let depth = 1 + rng.below(6);
                let mut p = String::new();
                for _ in 0..depth {
                    p.push_str(words[rng.below(words.len())]);
                    p.push('/');
                }
                p.push_str(names[rng.below(names.len())]);
                p.push_str(&format!("_{i}."));
                p.push_str(exts[rng.below(exts.len())]);
                if i % 7 == 0 {
                    p.push_str("/ünïcode");
                }
                p
            })
            .collect()
    }

    #[test]
    fn parallel_rank_matches_serial() {
        let paths = synthetic_paths(PARALLEL_MIN_CANDIDATES + 1000);
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        for (pattern, limit) in [("main", 50), ("ftr", 7), ("", 3), ("ünï", 10), ("zzzz", 5)] {
            let parallel = rank(pattern, refs.iter().copied(), limit);
            let mut serial = best_of(&mut FuzzyMatcher::new(pattern), &refs, 0, limit);
            serial.sort_unstable_by_key(|&(key, _)| key);
            let serial: Vec<(usize, i64)> =
                serial.iter().map(|&((Reverse(score), _, index), _)| (index, score)).collect();
            let parallel: Vec<(usize, i64)> = parallel.iter().map(|(index, m)| (*index, m.score)).collect();
            assert_eq!(parallel, serial, "{pattern:?}");
        }
    }

    /// `cargo test --release -p codecraft-fs -- --ignored --nocapture rank_perf`
    #[test]
    #[ignore]
    fn rank_perf() {
        let paths = synthetic_paths(100_000);
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        for pattern in ["m", "main", "ftr", "sess.rs", "src/main", "engine/cmd", "zzzz", "editorview"] {
            let start = std::time::Instant::now();
            let top = rank(pattern, refs.iter().copied(), 100);
            let elapsed = start.elapsed();
            let start = std::time::Instant::now();
            let serial = best_of(&mut FuzzyMatcher::new(pattern), &refs, 0, 100);
            let serial_elapsed = start.elapsed();
            assert_eq!(top.len(), serial.len());
            println!("rank {pattern:>12}: {:>4} results in {elapsed:?} (single thread {serial_elapsed:?})", top.len());
        }
    }
}
