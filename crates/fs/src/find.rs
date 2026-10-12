//! Find in Files: a parallel, ignore-aware regex search.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkState};
use regex::Regex;

use crate::{FileMask, FileMatch, FindOptions, Result, io, walk};

/// Longest `line_text` reported, in chars.
const MAX_LINE_TEXT_CHARS: usize = 500;

pub(crate) fn find_in_files(
    root: &Path,
    regex: &Regex,
    opts: &FindOptions,
    cancel: &AtomicBool,
) -> Result<Vec<FileMatch>> {
    let (root, _) = walk::absolute_root(root)?;
    if opts.max_results == 0 || cancel.load(Ordering::Relaxed) {
        return Ok(Vec::new());
    }
    let mask = opts.file_mask.as_deref().map(FileMask::parse).unwrap_or_default();
    let search = Search {
        regex,
        anchored: has_anchor(regex.as_str()),
        mask: &mask,
        opts,
        cancel,
        found: AtomicUsize::new(0),
        results: Mutex::new(Vec::new()),
    };
    // Over-size files are rejected by `read_bytes_limited`'s stat, so the walker needs no size filter.
    walk::builder(&root, &opts.scan).build_parallel().visit(&mut SearcherBuilder { search: &search });
    let mut results = search.results.into_inner().unwrap_or_else(PoisonError::into_inner);
    results.sort_by(|a, b| (&a.path, a.line, a.column, a.end_column).cmp(&(&b.path, b.line, b.column, b.end_column)));
    results.truncate(opts.max_results);
    Ok(results)
}

/// Whether the pattern may use `^`, `$`, `\A` or `\z`, which only mean "line boundary" when
/// the regex runs on one line at a time (conservative: escaped or bracketed ones count too).
fn has_anchor(pattern: &str) -> bool {
    pattern.contains(['^', '$']) || pattern.contains("\\A") || pattern.contains("\\z")
}

struct Search<'a> {
    regex: &'a Regex,
    anchored: bool,
    mask: &'a FileMask,
    opts: &'a FindOptions,
    cancel: &'a AtomicBool,
    found: AtomicUsize,
    results: Mutex<Vec<FileMatch>>,
}

impl Search<'_> {
    fn should_stop(&self, pending: usize) -> bool {
        self.cancel.load(Ordering::Relaxed)
            || self.found.load(Ordering::Relaxed).saturating_add(pending) >= self.opts.max_results
    }

    fn search_entry(&self, entry: DirEntry) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            return;
        }
        if !self.mask.matches(&entry.file_name().to_string_lossy()) {
            return;
        }
        let path = entry.into_path();
        // Unreadable, over-size, binary and non-UTF-8 files are skipped, not errors.
        let Ok(bytes) = io::read_bytes_limited(&path, self.opts.max_file_size) else { return };
        let Ok(text) = io::decode(&path, bytes) else { return };
        let matches = self.search_text(&path, &text);
        if !matches.is_empty() {
            self.found.fetch_add(matches.len(), Ordering::Relaxed);
            let mut results = self.results.lock().unwrap_or_else(PoisonError::into_inner);
            results.extend(matches);
        }
    }

    /// Visits the lines that can contain a match: every line for anchored patterns, otherwise
    /// only lines where a whole-text search lands (most files and lines are skipped fast).
    fn search_text(&self, path: &Path, text: &str) -> Vec<FileMatch> {
        let mut out = Vec::new();
        let mut pos = 0usize; // always a line start
        let mut line_no = 0usize;
        let mut counted_to = 0usize; // newlines before this offset are counted in `line_no`
        while pos <= text.len() {
            if self.should_stop(out.len()) {
                break;
            }
            let line_start = if self.anchored {
                pos
            } else {
                let Some(found) = self.regex.find_at(text, pos) else { break };
                let before = text.get(pos..found.start()).unwrap_or("");
                before.rfind('\n').map_or(pos, |i| pos.saturating_add(i).saturating_add(1))
            };
            let line_end = text
                .get(line_start..)
                .and_then(|rest| rest.find('\n'))
                .map_or(text.len(), |i| line_start.saturating_add(i));
            line_no = line_no.saturating_add(text.get(counted_to..line_start).map_or(0, |s| s.matches('\n').count()));
            counted_to = line_start;
            let line = text.get(line_start..line_end).unwrap_or("");
            let line = line.strip_suffix('\r').unwrap_or(line);
            self.search_line(path, line_no, line, &mut out);
            pos = line_end.saturating_add(1);
        }
        out
    }

    fn search_line(&self, path: &Path, line_no: usize, line: &str, out: &mut Vec<FileMatch>) {
        let mut line_text: Option<String> = None;
        let (mut byte, mut column) = (0usize, 0usize);
        for found in self.regex.find_iter(line) {
            if found.is_empty() {
                continue;
            }
            if self.should_stop(out.len()) {
                return;
            }
            column = column.saturating_add(line.get(byte..found.start()).map_or(0, |s| s.chars().count()));
            byte = found.start();
            let text = line_text.get_or_insert_with(|| truncate_chars(line, MAX_LINE_TEXT_CHARS).to_owned());
            out.push(FileMatch {
                path: path.to_path_buf(),
                line: line_no,
                column,
                end_column: column.saturating_add(found.as_str().chars().count()),
                line_text: text.clone(),
            });
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((end, _)) => s.get(..end).unwrap_or(s),
        None => s,
    }
}

struct SearcherBuilder<'s> {
    search: &'s Search<'s>,
}

impl<'s> ParallelVisitorBuilder<'s> for SearcherBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Searcher { search: self.search })
    }
}

struct Searcher<'s> {
    search: &'s Search<'s>,
}

impl ParallelVisitor for Searcher<'_> {
    fn visit(&mut self, entry: std::result::Result<DirEntry, ignore::Error>) -> WalkState {
        if self.search.should_stop(0) {
            return WalkState::Quit;
        }
        if let Ok(entry) = entry {
            self.search.search_entry(entry);
        }
        WalkState::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::tests::touch;
    use crate::{FsError, ScanOptions, find_in_files};
    use std::fs;

    fn find(root: &Path, pattern: &str, opts: &FindOptions) -> Vec<FileMatch> {
        find_in_files(root, &Regex::new(pattern).unwrap(), opts, &AtomicBool::new(false)).unwrap()
    }

    /// `(relative path, line, column, end_column)`.
    fn brief(root: &Path, matches: &[FileMatch]) -> Vec<(String, usize, usize, usize)> {
        matches
            .iter()
            .map(|m| {
                let rel = m.path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
                (rel, m.line, m.column, m.end_column)
            })
            .collect()
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "src/main.rs", "fn main() {\n    let needle = 1;\n}\n");
        touch(dir.path(), "src/lib.rs", "// needle needle\npub fn f() {}\n");
        touch(dir.path(), "README.md", "no match here\nneedle at start\n");
        touch(dir.path(), "Cargo.toml", "[package]\nname = \"needle\"\n");
        touch(dir.path(), "target/debug/out.rs", "needle\n");
        touch(dir.path(), ".gitignore", "ignored.rs\n");
        touch(dir.path(), "src/ignored.rs", "needle\n");
        dir
    }

    #[test]
    fn finds_matches_sorted_with_ignore_rules() {
        let dir = fixture();
        let found = find(dir.path(), "needle", &FindOptions::default());
        assert_eq!(
            brief(dir.path(), &found),
            [
                ("Cargo.toml".into(), 1, 8, 14),
                ("README.md".into(), 1, 0, 6),
                ("src/lib.rs".into(), 0, 3, 9),
                ("src/lib.rs".into(), 0, 10, 16),
                ("src/main.rs".into(), 1, 8, 14),
            ]
        );
        assert_eq!(found[1].line_text, "needle at start");
        assert_eq!(found[2].line_text, "// needle needle");
        assert!(found.iter().all(|m| m.path.is_absolute()));
    }

    #[test]
    fn file_masks_include_and_exclude() {
        let dir = fixture();
        let opts = |mask: &str| FindOptions { file_mask: Some(mask.into()), ..FindOptions::default() };
        let files = |mask: &str| {
            let mut v: Vec<String> =
                brief(dir.path(), &find(dir.path(), "needle", &opts(mask))).into_iter().map(|m| m.0).collect();
            v.dedup();
            v
        };
        assert_eq!(files("*.rs"), ["src/lib.rs", "src/main.rs"]);
        assert_eq!(files("*.rs, *.TOML"), ["Cargo.toml", "src/lib.rs", "src/main.rs"]);
        assert_eq!(files("!*.rs"), ["Cargo.toml", "README.md"]);
        assert_eq!(files("*.rs,!main.rs"), ["src/lib.rs"]);
        assert_eq!(files(""), ["Cargo.toml", "README.md", "src/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn scan_options_apply() {
        let dir = fixture();
        let opts = FindOptions {
            scan: ScanOptions { respect_gitignore: false, exclude_dirs: vec![], ..ScanOptions::default() },
            file_mask: Some("*.rs".into()),
            ..FindOptions::default()
        };
        let files: Vec<String> =
            brief(dir.path(), &find(dir.path(), "needle", &opts)).into_iter().map(|m| m.0).collect();
        assert_eq!(files, ["src/ignored.rs", "src/lib.rs", "src/lib.rs", "src/main.rs", "target/debug/out.rs"]);
    }

    #[test]
    fn skips_binary_oversize_and_non_utf8_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("bin.dat"), b"needle\0needle").unwrap();
        fs::write(dir.path().join("latin1.txt"), b"needle caf\xE9").unwrap();
        fs::write(dir.path().join("big.txt"), format!("needle{}", "x".repeat(100))).unwrap();
        fs::write(dir.path().join("ok.txt"), b"\xEF\xBB\xBFneedle").unwrap();
        let opts = FindOptions { max_file_size: 50, ..FindOptions::default() };
        let found = find(dir.path(), "needle", &opts);
        assert_eq!(brief(dir.path(), &found), [("ok.txt".into(), 0, 0, 6)], "BOM stripped, so column 0");
    }

    #[test]
    fn columns_are_chars_not_bytes() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "u.txt", "héllo wörld 🦀 crab\r\nζ crab\n");
        let found = find(dir.path(), "crab|wörld", &FindOptions::default());
        assert_eq!(
            brief(dir.path(), &found),
            [("u.txt".into(), 0, 6, 11), ("u.txt".into(), 0, 14, 18), ("u.txt".into(), 1, 2, 6)]
        );
        assert_eq!(found[0].line_text, "héllo wörld 🦀 crab", "CR stripped");
        let emoji = find(dir.path(), "🦀", &FindOptions::default());
        assert_eq!(brief(dir.path(), &emoji), [("u.txt".into(), 0, 12, 13)]);
    }

    #[test]
    fn anchors_apply_per_line_and_matches_do_not_span_lines() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.rs", "use a;\nfn f() {}\nuse b;\r\nx use\n");
        let starts = find(dir.path(), "^use", &FindOptions::default());
        assert_eq!(brief(dir.path(), &starts), [("a.rs".into(), 0, 0, 3), ("a.rs".into(), 2, 0, 3)]);
        let ends = find(dir.path(), r";$", &FindOptions::default());
        assert_eq!(brief(dir.path(), &ends), [("a.rs".into(), 0, 5, 6), ("a.rs".into(), 2, 5, 6)]);
        assert!(find(dir.path(), r"a;\nfn", &FindOptions::default()).is_empty());
        assert!(find(dir.path(), r"\{\}\s+use", &FindOptions::default()).is_empty());
        // A whole-text match spanning lines must not hide a match on a later line.
        let greedy = find(dir.path(), r"a;[\s\S]*|b;", &FindOptions::default());
        assert_eq!(brief(dir.path(), &greedy), [("a.rs".into(), 0, 4, 6), ("a.rs".into(), 2, 4, 6)]);
    }

    #[test]
    fn empty_matches_are_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "abc\n\nxyz");
        assert!(find(dir.path(), "q*", &FindOptions::default()).is_empty());
        assert!(find(dir.path(), "^", &FindOptions::default()).is_empty());
        let found = find(dir.path(), "z*", &FindOptions::default());
        assert_eq!(brief(dir.path(), &found), [("a.txt".into(), 2, 2, 3)]);
    }

    #[test]
    fn long_lines_are_truncated_at_char_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let line = format!("{}needle{}", "é".repeat(600), "x".repeat(10));
        touch(dir.path(), "long.txt", &line);
        let found = find(dir.path(), "needle", &FindOptions::default());
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].column, found[0].end_column), (600, 606));
        assert_eq!(found[0].line_text.chars().count(), 500);
        assert!(found[0].line_text.chars().all(|c| c == 'é'));
    }

    #[test]
    fn max_results_caps_the_result_count() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            touch(dir.path(), &format!("f{i:02}.txt"), "x x x\nx\n");
        }
        let opts = FindOptions { max_results: 7, ..FindOptions::default() };
        let found = find(dir.path(), "x", &opts);
        assert_eq!(found.len(), 7);
        let mut sorted = found.clone();
        sorted.sort_by(|a, b| (&a.path, a.line, a.column).cmp(&(&b.path, b.line, b.column)));
        assert_eq!(found, sorted);
        assert!(find(dir.path(), "x", &FindOptions { max_results: 0, ..FindOptions::default() }).is_empty());
        assert_eq!(find(dir.path(), "x", &FindOptions::default()).len(), 80);
    }

    #[test]
    fn cancel_stops_the_search() {
        let dir = fixture();
        let cancelled = AtomicBool::new(true);
        let found =
            find_in_files(dir.path(), &Regex::new("needle").unwrap(), &FindOptions::default(), &cancelled).unwrap();
        assert!(found.is_empty());
        // Cancel mid-search from another thread: must return promptly with partial results.
        let big = tempfile::tempdir().unwrap();
        for i in 0..200 {
            touch(big.path(), &format!("d{}/f{i}.txt", i % 10), &"needle line\n".repeat(200));
        }
        let cancel = AtomicBool::new(false);
        let regex = Regex::new("needle").unwrap();
        let opts = FindOptions { max_results: usize::MAX, ..FindOptions::default() };
        let partial = std::thread::scope(|s| {
            let handle = s.spawn(|| find_in_files(big.path(), &regex, &opts, &cancel).unwrap());
            cancel.store(true, Ordering::Relaxed);
            handle.join().unwrap()
        });
        assert!(partial.len() <= 200 * 200);
    }

    #[test]
    fn single_file_root_and_missing_root() {
        let dir = fixture();
        let found = find(&dir.path().join("src/lib.rs"), "needle", &FindOptions::default());
        assert_eq!(found.len(), 2);
        let err = find_in_files(
            &dir.path().join("missing"),
            &Regex::new("x").unwrap(),
            &FindOptions::default(),
            &AtomicBool::new(false),
        );
        assert!(matches!(err, Err(FsError::Io { .. })));
    }

    /// `CODECRAFT_FS_PERF_ROOT=~/src/big cargo test --release -p codecraft-fs -- --ignored --nocapture scan_and_find_perf`
    #[test]
    #[ignore]
    fn scan_and_find_perf() {
        let Some(root) = std::env::var_os("CODECRAFT_FS_PERF_ROOT") else {
            println!("CODECRAFT_FS_PERF_ROOT not set; skipping");
            return;
        };
        let root = Path::new(&root);
        let start = std::time::Instant::now();
        let tree = crate::scan(root, &ScanOptions::default()).unwrap();
        println!("scan: {} nodes ({} files) in {:?}", tree.len(), tree.files().count(), start.elapsed());
        let opts = FindOptions { max_results: usize::MAX, ..FindOptions::default() };
        for pattern in [r"fn\s+main", "unsafe", r"^use std", "zzzzqqqq"] {
            let start = std::time::Instant::now();
            let found = find(root, pattern, &opts);
            println!("find {pattern:>12}: {:>6} matches in {:?}", found.len(), start.elapsed());
        }
    }

    #[test]
    fn anchor_detection_is_conservative() {
        assert!(has_anchor("^fn"));
        assert!(has_anchor("x$"));
        assert!(has_anchor(r"\Ax"));
        assert!(has_anchor(r"x\z"));
        assert!(has_anchor(r"[$]"));
        assert!(!has_anchor(r"fn\s+\w+"));
    }
}
