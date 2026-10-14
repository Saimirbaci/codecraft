//! Code › Analyze Code › Locate Duplicates…: token-based clone detection.
//!
//! Files are lexed (comments and whitespace ignored), identifiers and literals are optionally
//! normalized, and every window of `min_tokens` tokens is hashed (Rabin–Karp). Windows with equal
//! hashes are verified, extended to maximal clones and grouped: each group is a fragment that
//! occurs two or more times (in one file or across files of the same language). Runs on a worker
//! thread; results are grouped in the Duplicates window.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_text::Document;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::inspections::ScopeKind;
use crate::params::{bool_or, opt_path, opt_str, opt_usize};
use crate::ssr::{self, TokKind};
use crate::{CommandError, Session};

pub const DEFAULT_MIN_TOKENS: usize = 40;
const MIN_MIN_TOKENS: usize = 5;
/// Groups reported.
const MAX_GROUPS: usize = 500;
/// Occurrences compared per hash bucket.
const MAX_BUCKET: usize = 64;
/// Tokens analyzed per run.
const MAX_TOKENS: usize = 4_000_000;
/// Files analyzed per run.
const MAX_FILES: usize = 20_000;
/// Chars per file.
const MAX_FILE_CHARS: usize = 2 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Options {
    pub min_tokens: usize,
    /// Treat all identifiers as equal (finds renamed copies).
    pub anonymize_identifiers: bool,
    /// Treat all literals as equal.
    pub anonymize_literals: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { min_tokens: DEFAULT_MIN_TOKENS, anonymize_identifiers: true, anonymize_literals: true }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub path: PathBuf,
    /// Char range and zero-based lines (inclusive).
    pub start: usize,
    pub end: usize,
    pub start_line: usize,
    pub end_line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub tokens: usize,
    pub lines: usize,
    pub occurrences: Vec<Occurrence>,
    /// The first lines of the first occurrence.
    pub preview: String,
}

/// One file's input.
pub struct Source {
    pub path: PathBuf,
    pub language: Language,
    pub text: String,
}

const SEPARATOR_BASE: u32 = u32::MAX - (MAX_FILES as u32) - 1;

/// Find duplicated fragments. Cancellable; never panics on any input.
pub fn find(sources: &[Source], opts: &Options, cancel: &AtomicBool) -> Vec<DuplicateGroup> {
    let k = opts.min_tokens.max(MIN_MIN_TOKENS);
    // Token ids over all files; each file ends with a unique separator so no window crosses files.
    let mut ids: HashMap<String, u32> = HashMap::new();
    let mut seq: Vec<u32> = Vec::new();
    // Per token: (file index, char start, char end).
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (fi, src) in sources.iter().enumerate().take(MAX_FILES) {
        if cancel.load(Ordering::Relaxed) {
            return Vec::new();
        }
        for t in ssr::tokenize(src.language, &src.text) {
            if seq.len() >= MAX_TOKENS {
                break;
            }
            let key = match t.kind {
                TokKind::Ident if opts.anonymize_identifiers => "\u{1}id".to_string(),
                TokKind::Literal if opts.anonymize_literals => "\u{1}lit".to_string(),
                _ => format!("{}\u{2}{}", lang_key(src.language), t.text),
            };
            let next = u32::try_from(ids.len()).unwrap_or(u32::MAX).min(SEPARATOR_BASE - 1);
            let id = *ids.entry(key).or_insert(next);
            seq.push(id);
            spans.push((fi, t.start, t.end));
        }
        seq.push(SEPARATOR_BASE.saturating_add(u32::try_from(fi).unwrap_or(0)));
        spans.push((fi, 0, 0));
    }
    if seq.len() < k {
        return Vec::new();
    }
    let is_sep = |v: u32| v >= SEPARATOR_BASE;
    // Rolling hash of every window without a separator.
    const B: u64 = 1_000_003;
    let mut pow = 1u64;
    for _ in 1..k {
        pow = pow.wrapping_mul(B);
    }
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut h = 0u64;
    let mut run = 0usize;
    for (i, &v) in seq.iter().enumerate() {
        if is_sep(v) {
            h = 0;
            run = 0;
            continue;
        }
        if run == k {
            if let Some(&out) = seq.get(i - k) {
                h = h.wrapping_sub(u64::from(out).wrapping_mul(pow));
            }
            run -= 1;
        }
        h = h.wrapping_mul(B).wrapping_add(u64::from(v));
        run += 1;
        if run == k {
            let list = buckets.entry(h).or_default();
            if list.len() < MAX_BUCKET {
                list.push(i + 1 - k);
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    // Maximal clone pairs: equal windows not extendable to the left, extended to the right.
    let same = |a: usize, b: usize| match (seq.get(a), seq.get(b)) {
        (Some(x), Some(y)) => x == y && !is_sep(*x),
        _ => false,
    };
    let mut groups: BTreeMap<(usize, Vec<u32>), Vec<usize>> = BTreeMap::new();
    let mut pairs_seen: HashSet<(usize, usize)> = HashSet::new();
    let mut work = 0usize;
    for list in buckets.values().filter(|l| l.len() > 1) {
        for (x, &a) in list.iter().enumerate() {
            for &b in list.iter().skip(x + 1) {
                work += 1;
                if work.is_multiple_of(4096) && cancel.load(Ordering::Relaxed) {
                    return Vec::new();
                }
                if work > 5_000_000 {
                    break;
                }
                let (a, b) = if a < b { (a, b) } else { (b, a) };
                if !(0..k).all(|o| same(a + o, b + o)) {
                    continue;
                }
                if a > 0 && same(a - 1, b - 1) && b > a + k {
                    continue;
                }
                let mut len = k;
                while same(a + len, b + len) && a + len < b {
                    len += 1;
                }
                if a + len > b {
                    // Overlapping self-similar run (`x x x x`): not a clone.
                    continue;
                }
                if !pairs_seen.insert((a, b)) {
                    continue;
                }
                let key_seq: Vec<u32> = seq.get(a..a + len).map(<[u32]>::to_vec).unwrap_or_default();
                let g = groups.entry((len, key_seq)).or_default();
                for p in [a, b] {
                    if !g.contains(&p) {
                        g.push(p);
                    }
                }
            }
        }
    }
    // Build groups, biggest first; drop groups whose occurrences all lie inside a reported one.
    let mut list: Vec<(usize, Vec<usize>)> = groups.into_iter().map(|((len, _), starts)| (len, starts)).collect();
    list.sort_by(|x, y| (y.0 * y.1.len()).cmp(&(x.0 * x.1.len())).then(y.0.cmp(&x.0)));
    let mut covered: Vec<(usize, usize)> = Vec::new();
    let mut docs: HashMap<usize, Document> = HashMap::new();
    let mut out = Vec::new();
    for (len, mut starts) in list {
        starts.sort_unstable();
        if starts.iter().all(|s| covered.iter().any(|(c0, c1)| *c0 <= *s && s + len <= *c1)) {
            continue;
        }
        let mut occurrences = Vec::new();
        for &st in &starts {
            let (Some(&(fi, cs, _)), Some(&(_, _, ce))) = (spans.get(st), spans.get(st + len - 1)) else { continue };
            let Some(src) = sources.get(fi) else { continue };
            let doc = docs.entry(fi).or_insert_with(|| Document::from_text(&src.text));
            occurrences.push(Occurrence {
                path: src.path.clone(),
                start: cs,
                end: ce,
                start_line: doc.char_to_line(cs.min(doc.len_chars())),
                end_line: doc.char_to_line(ce.min(doc.len_chars())),
            });
            covered.push((st, st + len));
        }
        if occurrences.len() < 2 {
            continue;
        }
        let Some(first) = occurrences.first() else { continue };
        let preview = sources
            .iter()
            .find(|s| s.path == first.path)
            .map(|s| {
                let text: String = s.text.chars().skip(first.start).take(first.end.saturating_sub(first.start)).collect();
                text.lines().take(6).collect::<Vec<_>>().join("\n")
            })
            .unwrap_or_default();
        let lines = first.end_line.saturating_sub(first.start_line) + 1;
        out.push(DuplicateGroup { tokens: len, lines, occurrences, preview });
        if out.len() >= MAX_GROUPS {
            break;
        }
    }
    out
}

fn lang_key(l: Language) -> &'static str {
    ssr::language_id(l)
}

/// Languages worth checking for duplicated code.
fn is_code(l: Language) -> bool {
    matches!(
        l,
        Language::Rust
            | Language::C
            | Language::Cpp
            | Language::Python
            | Language::Zig
            | Language::TypeScript
            | Language::JavaScript
            | Language::Sql
            | Language::Shell
    )
}

// ---------------------------------------------------------------- state and commands

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicatesDialog {
    pub scope: String,
    pub min_tokens: usize,
    pub anonymize_identifiers: bool,
    pub anonymize_literals: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DuplicatesResult {
    pub scope_label: String,
    pub options: Option<Options>,
    pub files: usize,
    pub groups: Vec<DuplicateGroup>,
    pub done: bool,
}

pub struct DuplicatesRun {
    cancel: Arc<AtomicBool>,
    rx: Receiver<Vec<DuplicateGroup>>,
}

#[derive(Default)]
pub struct DuplicatesState {
    pub dialog: Option<DuplicatesDialog>,
    pub result: Option<DuplicatesResult>,
    pub running: Option<DuplicatesRun>,
}

impl DuplicatesState {
    pub fn busy(&self) -> bool {
        self.running.is_some()
    }
}

fn start(s: &mut Session, p: &Value) -> CmdResult {
    let scope = opt_str(p, "scope").unwrap_or("project");
    let kind = ScopeKind::parse(scope).ok_or_else(|| CommandError::BadParams(format!("unknown scope `{scope}`")))?;
    let opts = Options {
        min_tokens: opt_usize(p, "minTokens")?.unwrap_or(DEFAULT_MIN_TOKENS).clamp(MIN_MIN_TOKENS, 100_000),
        anonymize_identifiers: bool_or(p, "anonymizeIdentifiers", true),
        anonymize_literals: bool_or(p, "anonymizeLiterals", true),
    };
    let r = crate::inspections::resolve_scope(s, kind, opt_path(p, "path"))?;
    let files: Vec<(PathBuf, Option<String>)> = r
        .files
        .iter()
        .filter(|f| is_code(Language::from_path(f)))
        .take(MAX_FILES)
        .map(|f| (f.clone(), s.ws.doc_by_path(f).and_then(|d| s.ws.doc(d)).map(|d| d.doc.text())))
        .collect();
    if let Some(run) = s.duplicates.running.take() {
        run.cancel.store(true, Ordering::Relaxed);
    }
    let count = files.len();
    s.duplicates.dialog = None;
    s.duplicates.result =
        Some(DuplicatesResult { scope_label: r.label, options: Some(opts), files: count, groups: Vec::new(), done: false });
    let cancel = Arc::new(AtomicBool::new(false));
    let job = move |flag: &AtomicBool| {
        let sources: Vec<Source> = files
            .into_iter()
            .filter_map(|(path, open)| {
                let text = match open {
                    Some(t) => t,
                    None => Document::from_text(&codecraft_fs::read_text_file(&path).ok()?).text(),
                };
                (text.chars().count() <= MAX_FILE_CHARS).then(|| Source { language: Language::from_path(&path), path, text })
            })
            .collect();
        find(&sources, &opts, flag)
    };
    if bool_or(p, "sync", false) {
        let groups = job(&cancel);
        finish(s, groups);
    } else {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let flag = Arc::clone(&cancel);
        std::thread::Builder::new()
            .name("codecraft-duplicates".into())
            .spawn(move || {
                let _ = tx.send(job(&flag));
            })
            .map_err(|e| CommandError::Failed(format!("cannot start: {e}")))?;
        s.duplicates.running = Some(DuplicatesRun { cancel, rx });
        s.status = "Locating duplicates…".into();
    }
    Ok(json!({ "files": count, "groups": s.duplicates.result.as_ref().filter(|r| r.done).map(|r| r.groups.len()) }))
}

fn finish(s: &mut Session, groups: Vec<DuplicateGroup>) {
    let n = groups.len();
    if let Some(r) = &mut s.duplicates.result {
        r.groups = groups;
        r.done = true;
    }
    s.status = format!("Locate Duplicates: {n} duplicated fragment(s)");
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(run) = &s.duplicates.running else { return false };
    match run.rx.try_recv() {
        Ok(groups) => {
            s.duplicates.running = None;
            finish(s, groups);
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            s.duplicates.running = None;
            if let Some(r) = &mut s.duplicates.result {
                r.done = true;
            }
            true
        }
    }
}

pub(crate) fn shutdown(s: &mut Session) {
    if let Some(run) = s.duplicates.running.take() {
        run.cancel.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "code.analyzeCode.locateDuplicates",
            "Locate Duplicates…",
            None,
            "{scope?: \"project\"|\"directory\"|\"file\"|\"uncommitted\", path?: string, minTokens?: int (default 40), anonymizeIdentifiers?: bool, anonymizeLiterals?: bool, sync?: bool} — find duplicated code fragments (token-based); without params opens the settings dialog",
            Context::Global,
            has_project,
            |s, p| {
                if p.as_object().is_none_or(|o| o.is_empty()) {
                    s.duplicates.dialog = Some(DuplicatesDialog {
                        scope: "project".into(),
                        min_tokens: DEFAULT_MIN_TOKENS,
                        anonymize_identifiers: true,
                        anonymize_literals: true,
                    });
                    return Ok(json!({ "dialog": true }));
                }
                start(s, p)
            },
        ),
        spec(
            "duplicates.dialog.ok",
            "Locate Duplicates",
            None,
            "Run with the dialog's settings",
            Context::Global,
            |s| s.duplicates.dialog.as_ref().map(|_| ()).ok_or_else(|| "the dialog is not open".into()),
            |s, _| {
                let d = s.duplicates.dialog.clone().ok_or_else(|| CommandError::Failed("no dialog".into()))?;
                start(
                    s,
                    &json!({
                        "scope": d.scope,
                        "minTokens": d.min_tokens,
                        "anonymizeIdentifiers": d.anonymize_identifiers,
                        "anonymizeLiterals": d.anonymize_literals,
                    }),
                )
            },
        ),
        spec("duplicates.close", "Close Duplicates", None, "", Context::Global, always, |s, _| {
            shutdown(s);
            s.duplicates.dialog = None;
            s.duplicates.result = None;
            ok()
        }),
        spec("duplicates.results", "Duplicates", None, "→ the last Locate Duplicates result", Context::Global, always, |s, _| {
            Ok(json!({ "running": s.duplicates.busy(), "result": s.duplicates.result }))
        }),
        spec(
            "duplicates.open",
            "Open Duplicate",
            None,
            "{group: int, occurrence: int} — open an occurrence",
            Context::Global,
            |s| s.duplicates.result.as_ref().map(|_| ()).ok_or_else(|| "no duplicates".into()),
            |s, p| {
                let g = opt_usize(p, "group")?.unwrap_or(0);
                let o = opt_usize(p, "occurrence")?.unwrap_or(0);
                let occ = s
                    .duplicates
                    .result
                    .as_ref()
                    .and_then(|r| r.groups.get(g))
                    .and_then(|g| g.occurrences.get(o))
                    .cloned()
                    .ok_or_else(|| CommandError::BadParams(format!("no occurrence {o} in group {g}")))?;
                s.navigate_to(&occ.path, occ.start)?;
                ok()
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn src(path: &str, text: &str) -> Source {
        Source { path: path.into(), language: Language::Rust, text: text.into() }
    }

    const BODY_A: &str = "fn total(items: &[Item]) -> u64 {\n    let mut sum = 0;\n    for item in items {\n        if item.enabled {\n            sum += item.price * item.count;\n        }\n    }\n    sum\n}\n";
    const BODY_B: &str = "fn amount(rows: &[Row]) -> u64 {\n    let mut acc = 0;\n    for row in rows {\n        if row.enabled {\n            acc += row.cost * row.qty;\n        }\n    }\n    acc\n}\n";

    #[test]
    fn finds_renamed_copies_across_files() {
        let opts = Options { min_tokens: 20, ..Options::default() };
        let cancel = AtomicBool::new(false);
        let groups = find(&[src("/a.rs", BODY_A), src("/b.rs", &format!("// header\n\n{BODY_B}"))], &opts, &cancel);
        assert_eq!(groups.len(), 1, "{groups:?}");
        let g = &groups[0];
        assert_eq!(g.occurrences.len(), 2);
        assert!(g.tokens >= 40, "{}", g.tokens);
        assert_eq!(g.occurrences[1].path, PathBuf::from("/b.rs"));
        assert_eq!(g.occurrences[1].start_line, 2);
        assert!(g.preview.starts_with("fn total"));
        // Without anonymized identifiers they differ.
        let strict = Options { min_tokens: 20, anonymize_identifiers: false, anonymize_literals: true };
        assert!(find(&[src("/a.rs", BODY_A), src("/b.rs", BODY_B)], &strict, &cancel).is_empty());
        // Too small to report.
        let big = Options { min_tokens: 500, ..Options::default() };
        assert!(find(&[src("/a.rs", BODY_A), src("/b.rs", BODY_B)], &big, &cancel).is_empty());
    }

    #[test]
    fn three_copies_in_one_file_form_one_group() {
        let text = format!("{BODY_A}\n{BODY_A}\n{BODY_A}");
        let groups = find(&[src("/a.rs", &text)], &Options { min_tokens: 20, ..Options::default() }, &AtomicBool::new(false));
        assert_eq!(groups.len(), 1, "{groups:?}");
        assert_eq!(groups[0].occurrences.len(), 3);
    }

    #[test]
    fn hostile_inputs() {
        let cancel = AtomicBool::new(false);
        let opts = Options { min_tokens: 0, ..Options::default() };
        for t in ["", "x", &"x ".repeat(20_000), &"(".repeat(10_000), "😀 😀 😀 😀 😀 😀 😀"] {
            let _ = find(&[src("/a.rs", t), src("/b.rs", t)], &opts, &cancel);
        }
        cancel.store(true, Ordering::Relaxed);
        assert!(find(&[src("/a.rs", BODY_A), src("/b.rs", BODY_A)], &opts, &cancel).is_empty());
    }
}
