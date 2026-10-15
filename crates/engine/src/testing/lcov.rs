//! LCOV coverage reports (`cargo llvm-cov --lcov`, `pytest --cov-report=lcov`, gcov/lcov): the
//! parser, per-file line coverage and the directory roll-up the Coverage tool window shows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Line numbers above this are treated as garbage (a hostile report must not allocate wildly).
const MAX_LINE: usize = 50_000_000;
/// Files per report.
const MAX_FILES: usize = 200_000;

/// Coverage of one source file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCoverage {
    /// Instrumented lines (0-based) → execution count.
    pub lines: BTreeMap<usize, u64>,
    pub functions_found: usize,
    pub functions_hit: usize,
    pub branches_found: usize,
    pub branches_hit: usize,
}

impl FileCoverage {
    pub fn lines_found(&self) -> usize {
        self.lines.len()
    }

    pub fn lines_hit(&self) -> usize {
        self.lines.values().filter(|h| **h > 0).count()
    }

    /// Whether the 0-based line is covered (`None`: not instrumented).
    pub fn line(&self, line: usize) -> Option<bool> {
        self.lines.get(&line).map(|h| *h > 0)
    }
}

/// A parsed coverage report: absolute, normalized source path → coverage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageReport {
    /// Stored report id (see `Manage Coverage Reports`), empty for an unsaved one.
    pub id: String,
    pub name: String,
    pub files: BTreeMap<PathBuf, FileCoverage>,
}

/// One row of the Coverage tool window: a directory (aggregated) or a file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CoverageRow {
    pub path: PathBuf,
    /// Name shown in the tree (the directory or file name; the project-relative path at depth 0).
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub lines_hit: usize,
    pub lines_found: usize,
    pub functions_hit: usize,
    pub functions_found: usize,
}

/// A column of the Coverage tool window's table (M13 track RC).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverageColumn {
    /// Element: by name (the tree's order).
    #[default]
    Name,
    /// Line, %.
    Line,
    /// Function, %.
    Function,
}

impl CoverageColumn {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "name" | "element" => Some(Self::Name),
            "line" | "lines" | "line%" => Some(Self::Line),
            "function" | "functions" | "function%" => Some(Self::Function),
            _ => None,
        }
    }
}

/// The Coverage table's order: a column, ascending or descending. Siblings are sorted within
/// each directory; directories stay before files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageSort {
    pub column: CoverageColumn,
    pub descending: bool,
}

/// `rows` (a pre-order tree as [`CoverageReport::rows`] returns it) re-ordered by `sort`.
pub fn sort_rows(rows: Vec<CoverageRow>, sort: CoverageSort) -> Vec<CoverageRow> {
    if sort == CoverageSort::default() {
        return rows;
    }
    fn blocks(rows: &[CoverageRow]) -> Vec<&[CoverageRow]> {
        let mut out = Vec::new();
        let mut rest = rows;
        while let Some((first, tail)) = rest.split_first() {
            let len = 1 + tail.iter().take_while(|r| r.depth > first.depth).count();
            let (block, after) = rest.split_at(len.min(rest.len()));
            out.push(block);
            rest = after;
        }
        out
    }
    fn level(rows: &[CoverageRow], sort: CoverageSort, out: &mut Vec<CoverageRow>) {
        let mut bs = blocks(rows);
        bs.sort_by(|a, b| {
            let (Some(x), Some(y)) = (a.first(), b.first()) else { return std::cmp::Ordering::Equal };
            let key = match sort.column {
                CoverageColumn::Name => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                CoverageColumn::Line => x.line_percent().total_cmp(&y.line_percent()),
                CoverageColumn::Function => x.function_percent().total_cmp(&y.function_percent()),
            };
            let key = if sort.descending { key.reverse() } else { key };
            y.is_dir.cmp(&x.is_dir).then(key).then_with(|| x.name.cmp(&y.name))
        });
        for b in bs {
            if let Some((head, children)) = b.split_first() {
                out.push(head.clone());
                level(children, sort, out);
            }
        }
    }
    let mut out = Vec::with_capacity(rows.len());
    level(&rows, sort, &mut out);
    out
}

impl CoverageRow {
    /// Line coverage in percent (100 when nothing is instrumented).
    pub fn line_percent(&self) -> f64 {
        percent(self.lines_hit, self.lines_found)
    }

    pub fn function_percent(&self) -> f64 {
        percent(self.functions_hit, self.functions_found)
    }
}

pub fn percent(hit: usize, found: usize) -> f64 {
    if found == 0 { 100.0 } else { hit as f64 * 100.0 / found as f64 }
}

impl CoverageReport {
    /// Parse LCOV text. Relative `SF:` paths are resolved against `base`. Unknown records are
    /// skipped; malformed numbers are ignored.
    pub fn parse_lcov(text: &str, base: &Path) -> Self {
        let mut report = Self::default();
        let mut current: Option<(PathBuf, FileCoverage)> = None;
        let mut fn_names: BTreeMap<String, u64> = BTreeMap::new();
        let mut summary_fn: Option<(usize, usize)> = None;
        let mut summary_br: (Option<usize>, Option<usize>) = (None, None);
        let mut brda: (usize, usize) = (0, 0);
        for raw in text.lines() {
            let line = raw.trim();
            if line == "end_of_record" {
                if let Some((path, mut cov)) = current.take() {
                    let (found, hit) =
                        summary_fn.unwrap_or((fn_names.len(), fn_names.values().filter(|h| **h > 0).count()));
                    cov.functions_found = found.max(fn_names.len());
                    cov.functions_hit = hit.min(cov.functions_found);
                    cov.branches_found = summary_br.0.unwrap_or(brda.0);
                    cov.branches_hit = summary_br.1.unwrap_or(brda.1).min(cov.branches_found);
                    report.add(path, cov);
                }
                fn_names.clear();
                summary_fn = None;
                summary_br = (None, None);
                brda = (0, 0);
                continue;
            }
            let Some((key, value)) = line.split_once(':') else { continue };
            match key {
                "SF" => {
                    let p = PathBuf::from(value);
                    let abs = if p.is_absolute() { p } else { base.join(p) };
                    current = Some((crate::normalize_path(&abs), FileCoverage::default()));
                }
                "DA" => {
                    let Some((_, cov)) = current.as_mut() else { continue };
                    let mut parts = value.split(',');
                    let (Some(l), Some(h)) = (parts.next(), parts.next()) else { continue };
                    let (Ok(l), Some(h)) = (l.trim().parse::<usize>(), parse_hits(h)) else { continue };
                    if l == 0 || l > MAX_LINE {
                        continue;
                    }
                    let e = cov.lines.entry(l - 1).or_insert(0);
                    *e = e.saturating_add(h);
                }
                "FNDA" => {
                    let Some((h, name)) = value.split_once(',') else { continue };
                    let Some(h) = parse_hits(h) else { continue };
                    let e = fn_names.entry(name.to_string()).or_insert(0);
                    *e = e.saturating_add(h);
                }
                "FN" => {
                    if let Some((_, name)) = value.rsplit_once(',') {
                        fn_names.entry(name.to_string()).or_insert(0);
                    }
                }
                "FNF" => {
                    let f = value.trim().parse().unwrap_or(0);
                    summary_fn = Some((f, summary_fn.map_or(0, |s| s.1)));
                }
                "FNH" => {
                    let h = value.trim().parse().unwrap_or(0);
                    summary_fn = Some((summary_fn.map_or(0, |s| s.0), h));
                }
                "BRF" => summary_br.0 = value.trim().parse().ok(),
                "BRH" => summary_br.1 = value.trim().parse().ok(),
                "BRDA" => {
                    brda.0 += 1;
                    if value.rsplit(',').next().and_then(parse_hits).is_some_and(|h| h > 0) {
                        brda.1 += 1;
                    }
                }
                _ => {}
            }
        }
        // A last record without `end_of_record`.
        if let Some((path, cov)) = current.take() {
            report.add(path, cov);
        }
        report
    }

    fn add(&mut self, path: PathBuf, cov: FileCoverage) {
        if self.files.len() >= MAX_FILES && !self.files.contains_key(&path) {
            return;
        }
        let e = self.files.entry(path).or_default();
        for (l, h) in cov.lines {
            let x = e.lines.entry(l).or_insert(0);
            *x = x.saturating_add(h);
        }
        e.functions_found = e.functions_found.max(cov.functions_found);
        e.functions_hit = e.functions_hit.max(cov.functions_hit);
        e.branches_found = e.branches_found.max(cov.branches_found);
        e.branches_hit = e.branches_hit.max(cov.branches_hit);
    }

    /// Coverage of `path` (also matched by its normalized and canonical forms: tools report
    /// canonical paths, the editor may hold a symlinked one).
    pub fn file(&self, path: &Path) -> Option<&FileCoverage> {
        if self.files.is_empty() {
            return None;
        }
        self.files
            .get(path)
            .or_else(|| self.files.get(&crate::normalize_path(path)))
            .or_else(|| std::fs::canonicalize(path).ok().and_then(|c| self.files.get(&c)))
    }

    /// Total (lines hit, lines found).
    pub fn totals(&self) -> (usize, usize) {
        self.files.values().fold((0, 0), |(h, f), c| (h + c.lines_hit(), f + c.lines_found()))
    }

    pub fn line_percent(&self) -> f64 {
        let (h, f) = self.totals();
        percent(h, f)
    }

    /// Directory tree rows under `root` (files outside it are listed by full path at the end),
    /// directories first then files, each level sorted by name. Directory rows aggregate the files
    /// below them; a directory with a single child directory is merged into it (`src/util`).
    pub fn rows(&self, root: &Path) -> Vec<CoverageRow> {
        #[derive(Default)]
        struct Dir {
            dirs: BTreeMap<String, Dir>,
            files: BTreeMap<String, PathBuf>,
        }
        let mut tree = Dir::default();
        let mut outside: Vec<&PathBuf> = Vec::new();
        for path in self.files.keys() {
            let Ok(rel) = path.strip_prefix(root) else {
                outside.push(path);
                continue;
            };
            let comps: Vec<String> = rel.iter().map(|c| c.to_string_lossy().into_owned()).collect();
            let Some((file, dirs)) = comps.split_last() else { continue };
            let mut node = &mut tree;
            for d in dirs {
                node = node.dirs.entry(d.clone()).or_default();
            }
            node.files.insert(file.clone(), path.clone());
        }
        fn agg(report: &CoverageReport, d: &Dir) -> (usize, usize, usize, usize) {
            let mut t = (0, 0, 0, 0);
            for p in d.files.values() {
                if let Some(c) = report.files.get(p) {
                    t.0 += c.lines_hit();
                    t.1 += c.lines_found();
                    t.2 += c.functions_hit;
                    t.3 += c.functions_found;
                }
            }
            for sub in d.dirs.values() {
                let s = agg(report, sub);
                t = (t.0 + s.0, t.1 + s.1, t.2 + s.2, t.3 + s.3);
            }
            t
        }
        fn walk(report: &CoverageReport, d: &Dir, path: &Path, depth: usize, out: &mut Vec<CoverageRow>) {
            for (name, sub) in &d.dirs {
                // Merge single-child directory chains.
                let (mut name, mut sub, mut p) = (name.clone(), sub, path.join(name));
                while sub.files.is_empty() && sub.dirs.len() == 1 {
                    let Some((n, s)) = sub.dirs.iter().next() else { break };
                    name = format!("{name}/{n}");
                    p = p.join(n);
                    sub = s;
                }
                let (lh, lf, fh, ff) = agg(report, sub);
                out.push(CoverageRow {
                    path: p.clone(),
                    name,
                    depth,
                    is_dir: true,
                    lines_hit: lh,
                    lines_found: lf,
                    functions_hit: fh,
                    functions_found: ff,
                });
                walk(report, sub, &p, depth + 1, out);
            }
            for (name, p) in &d.files {
                let Some(c) = report.files.get(p) else { continue };
                out.push(CoverageRow {
                    path: p.clone(),
                    name: name.clone(),
                    depth,
                    is_dir: false,
                    lines_hit: c.lines_hit(),
                    lines_found: c.lines_found(),
                    functions_hit: c.functions_hit,
                    functions_found: c.functions_found,
                });
            }
        }
        let mut out = Vec::new();
        walk(self, &tree, root, 0, &mut out);
        for p in outside {
            if let Some(c) = self.files.get(p) {
                out.push(CoverageRow {
                    path: p.clone(),
                    name: p.display().to_string(),
                    depth: 0,
                    is_dir: false,
                    lines_hit: c.lines_hit(),
                    lines_found: c.lines_found(),
                    functions_hit: c.functions_hit,
                    functions_found: c.functions_found,
                });
            }
        }
        out
    }
}

/// An execution count (LCOV allows `-` for "no data" and some tools emit floats).
fn parse_hits(s: &str) -> Option<u64> {
    let s = s.trim();
    s.parse::<u64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCOV: &str = "TN:\nSF:/w/src/lib.rs\nFN:3,add\nFN:9,sub\nFNDA:4,add\nFNDA:0,sub\nFNF:2\nFNH:1\n\
DA:3,4\nDA:4,4\nDA:9,0\nDA:10,0\nBRDA:4,0,0,1\nBRDA:4,0,1,-\nLF:4\nLH:2\nend_of_record\n\
SF:src/util/mod.rs\nDA:1,1\nDA:2,1\nend_of_record\nSF:/elsewhere/gen.rs\nDA:1,0\nend_of_record\n";

    #[test]
    fn parses_records() {
        let r = CoverageReport::parse_lcov(LCOV, Path::new("/w"));
        assert_eq!(r.files.len(), 3);
        let lib = r.file(Path::new("/w/src/lib.rs")).unwrap();
        assert_eq!((lib.lines_hit(), lib.lines_found()), (2, 4));
        assert_eq!(lib.line(2), Some(true));
        assert_eq!(lib.line(8), Some(false));
        assert_eq!(lib.line(0), None);
        assert_eq!((lib.functions_hit, lib.functions_found), (1, 2));
        assert_eq!((lib.branches_hit, lib.branches_found), (1, 2));
        // Relative paths resolve against the base.
        assert!(r.file(Path::new("/w/src/util/mod.rs")).is_some());
        assert_eq!(r.totals(), (4, 7));
        assert!((r.line_percent() - 400.0 / 7.0).abs() < 1e-9);
    }

    #[test]
    fn rows_roll_up_directories() {
        let r = CoverageReport::parse_lcov(LCOV, Path::new("/w"));
        let rows = r.rows(Path::new("/w"));
        let view: Vec<(usize, &str, bool, usize, usize)> =
            rows.iter().map(|r| (r.depth, r.name.as_str(), r.is_dir, r.lines_hit, r.lines_found)).collect();
        assert_eq!(
            view,
            [
                (0, "src", true, 4, 6),
                (1, "util", true, 2, 2),
                (2, "mod.rs", false, 2, 2),
                (1, "lib.rs", false, 2, 4),
                (0, "/elsewhere/gen.rs", false, 0, 1),
            ]
        );
        assert!((rows[3].line_percent() - 50.0).abs() < 1e-9);
        assert!((CoverageRow { lines_found: 0, ..rows[0].clone() }.line_percent() - 100.0).abs() < 1e-9);
    }

    #[test]
    fn rows_sort_by_column_within_each_directory() {
        let r = CoverageReport::parse_lcov(LCOV, Path::new("/w"));
        let rows = r.rows(Path::new("/w"));
        let names = |rows: &[CoverageRow]| rows.iter().map(|r| (r.depth, r.name.clone())).collect::<Vec<_>>();
        assert_eq!(sort_rows(rows.clone(), CoverageSort::default()), rows);
        // Line % ascending: lib.rs (50%) before nothing else at its level; util (100%) after it
        // would mean files before directories, so directories stay first.
        let by_line = sort_rows(rows.clone(), CoverageSort { column: CoverageColumn::Line, descending: false });
        assert_eq!(
            names(&by_line),
            [
                (0, "src".into()),
                (1, "util".into()),
                (2, "mod.rs".into()),
                (1, "lib.rs".into()),
                (0, "/elsewhere/gen.rs".into())
            ]
        );
        // Name descending: the outside file (`/…`) sorts after `src` descending.
        let by_name = sort_rows(rows.clone(), CoverageSort { column: CoverageColumn::Name, descending: true });
        assert_eq!(by_name.len(), rows.len());
        assert_eq!(by_name[0].name, "src");
        let mut two = rows.clone();
        // Two files at the top level: sorted by line % descending.
        two.retain(|r| r.depth == 0);
        two[0].is_dir = false;
        let sorted = sort_rows(two, CoverageSort { column: CoverageColumn::Line, descending: true });
        assert_eq!(sorted[0].name, "src", "{:?}", names(&sorted));
        assert_eq!(CoverageColumn::parse("Function"), Some(CoverageColumn::Function));
        assert_eq!(CoverageColumn::parse("x"), None);
    }

    #[test]
    fn merges_duplicate_files_and_tolerates_junk() {
        let text = "SF:/a.rs\nDA:1,1\nend_of_record\nSF:/a.rs\nDA:1,2\nDA:2,0\nend_of_record\n";
        let r = CoverageReport::parse_lcov(text, Path::new("/"));
        let a = r.file(Path::new("/a.rs")).unwrap();
        assert_eq!(a.lines.get(&0), Some(&3));
        assert_eq!(a.lines_found(), 2);
        let junk = "DA:1,1\nSF:\nDA:x,y\nDA:0,5\nDA:99999999999,1\nDA:3,18446744073709551615\nDA:3,9\nFNDA:x\nFN:\n\
BRDA:\nFNF:z\nend_of_record\nend_of_record\nSF:/b.rs\nDA:2,1.5";
        let r = CoverageReport::parse_lcov(junk, Path::new("/"));
        assert_eq!(r.files.values().map(|f| f.lines.len()).sum::<usize>(), 2);
        assert_eq!(r.file(Path::new("/b.rs")).and_then(|f| f.line(1)), Some(true));
        assert_eq!(CoverageReport::parse_lcov("", Path::new("/")).files.len(), 0);
    }
}
