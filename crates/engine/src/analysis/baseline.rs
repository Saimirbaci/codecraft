//! Baselines: save the current problems, then show only new ones.
//!
//! A problem's fingerprint ignores its line number (edits above it move it): analyzer family,
//! rule, project-relative path and the text of its line with whitespace collapsed. Fingerprints
//! are counted, so a file with three identical problems in the baseline and four now has one new
//! problem. The default file is `<project>/.codecraft/inspection-baseline.json`; once it exists,
//! every run compares with it (findings in it get `baseline: true`).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::Session;
use crate::inspections::{Finding, InspectionResults};
use crate::user_store::{load_json, save_json};

const FORMAT: &str = "codecraft-inspection-baseline/1";
/// Entries kept in a baseline file.
const MAX_ENTRIES: usize = 200_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineEntry {
    pub fingerprint: String,
    /// For people reading the file: where it was and what it said.
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub line: usize,
    #[serde(default)]
    pub analyzer: String,
    #[serde(default)]
    pub rule: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BaselineFile {
    #[serde(default)]
    format: String,
    #[serde(default)]
    problems: Vec<BaselineEntry>,
}

/// A loaded baseline: fingerprint → how many problems it accepts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Baseline {
    pub path: PathBuf,
    pub counts: HashMap<String, usize>,
    pub total: usize,
}

/// The baseline state of a session: the file in use and whether it was looked for.
#[derive(Debug, Default)]
pub struct BaselineState {
    /// The project the default file was looked up for.
    checked_for: Option<PathBuf>,
    pub current: Option<Arc<Baseline>>,
}

pub fn default_path(root: &Path) -> PathBuf {
    root.join(".codecraft").join("inspection-baseline.json")
}

fn line_hash(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    collapsed.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// The fingerprint of a finding given its file's text.
pub fn fingerprint(root: &Path, f: &Finding, text: Option<&str>) -> String {
    let rel = f.path.strip_prefix(root).unwrap_or(&f.path).to_string_lossy().replace('\\', "/");
    let line = text.and_then(|t| t.split('\n').nth(f.start.line)).unwrap_or("");
    // Without a line (a whole-file problem, an unreadable file) the message stands in.
    let anchor = if line.trim().is_empty() { line_hash(&f.message) } else { line_hash(line) };
    format!("{}|{}|{rel}|{anchor}", super::profiles::family(f.tool), f.rule())
}

/// Give findings their fingerprints and mark those the baseline already has (worker side).
pub(crate) fn mark(root: &Path, baseline: Option<&Baseline>, findings: &mut [Finding], texts: &HashMap<PathBuf, Option<String>>) {
    let mut left: HashMap<&str, usize> =
        baseline.map(|b| b.counts.iter().map(|(k, v)| (k.as_str(), *v)).collect()).unwrap_or_default();
    for f in findings.iter_mut() {
        f.fingerprint = fingerprint(root, f, texts.get(&f.path).and_then(|t| t.as_deref()));
        f.baseline = match left.get_mut(f.fingerprint.as_str()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        };
    }
}

pub fn load(path: &Path) -> Result<Baseline, String> {
    let file: BaselineFile = load_json(path)?.ok_or_else(|| format!("{} does not exist", path.display()))?;
    if !file.format.is_empty() && file.format != FORMAT {
        return Err(format!("{} is not a CodeCraft baseline ({})", path.display(), file.format));
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut total = 0usize;
    for e in file.problems.into_iter().take(MAX_ENTRIES) {
        if e.fingerprint.is_empty() || e.fingerprint.len() > 2048 {
            continue;
        }
        *counts.entry(e.fingerprint).or_insert(0) += 1;
        total += 1;
    }
    Ok(Baseline { path: path.to_path_buf(), counts, total })
}

/// Write the open problems of `results` as a baseline.
pub fn save(path: &Path, root: &Path, results: &InspectionResults) -> Result<Baseline, String> {
    let mut problems = Vec::new();
    for f in results.open() {
        if f.fingerprint.is_empty() {
            continue;
        }
        problems.push(BaselineEntry {
            fingerprint: f.fingerprint.clone(),
            path: f.path.strip_prefix(root).unwrap_or(&f.path).to_string_lossy().into_owned(),
            line: f.start.line + 1,
            analyzer: f.tool.id().into(),
            rule: f.rule().to_string(),
            message: f.message.chars().take(300).collect(),
        });
    }
    save_json(path, &BaselineFile { format: FORMAT.into(), problems }, false)?;
    load(path)
}

/// The baseline runs compare with: one set explicitly, else the project's default file.
pub(crate) fn current(s: &mut Session) -> Option<Arc<Baseline>> {
    let root = s.project_root();
    let st = &mut s.analysis.baseline;
    if st.checked_for != root {
        st.checked_for = root.clone();
        st.current = None;
        if let Some(root) = root {
            let path = default_path(&root);
            if path.is_file() {
                match load(&path) {
                    Ok(b) => st.current = Some(Arc::new(b)),
                    Err(e) => log::warn!("baseline: {e}"),
                }
            }
        }
    }
    st.current.clone()
}

/// Mark the current results against a baseline (after saving or loading one).
pub(crate) fn remark(s: &mut Session) {
    let Some(root) = s.project_root() else { return };
    let baseline = s.analysis.baseline.current.clone();
    let Some(results) = s.inspections.results.as_mut() else { return };
    let mut left: HashMap<String, usize> = baseline.as_ref().map(|b| b.counts.clone()).unwrap_or_default();
    for f in results.findings.iter_mut() {
        if f.fingerprint.is_empty() {
            // Imported results: no text was read; fingerprint from the message.
            f.fingerprint = fingerprint(&root, f, None);
        }
        f.baseline = match left.get_mut(&f.fingerprint) {
            Some(n) if *n > 0 => {
                *n -= 1;
                true
            }
            _ => false,
        };
    }
    results.baseline = baseline.map(|b| b.path.clone());
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::inspections::{JobKind, Scope, ScopeKind, Tool};
    use codecraft_text::Position;
    use codecraft_workspace::Severity;

    fn f(line: usize, rule: &str) -> Finding {
        let mut f = Finding::new(Tool::Clippy, rule, Severity::Warning, "/p/src/a.rs".into(), Position::new(line, 0));
        f.code = Some(rule.into());
        f.message = format!("{rule} here");
        f
    }

    #[test]
    fn fingerprints_survive_moves_and_count_duplicates() {
        let root = Path::new("/p");
        let before = "fn a() {\n    return 1;\n}\nfn b() {\n    return 1;\n}\n";
        let after = "// new line\nfn a() {\n    return   1;\n}\nfn b() {\n    return 1;\n}\nfn c() {\n    return 1;\n}\n";
        let mut old = vec![f(1, "clippy::needless_return"), f(4, "clippy::needless_return")];
        mark(root, None, &mut old, &HashMap::from([(PathBuf::from("/p/src/a.rs"), Some(before.to_string()))]));
        assert_eq!(old[0].fingerprint, old[1].fingerprint, "same rule, same line text");
        assert!(old[0].fingerprint.starts_with("clippy|clippy::needless_return|src/a.rs|"));
        let mut counts = HashMap::new();
        for x in &old {
            *counts.entry(x.fingerprint.clone()).or_insert(0) += 1;
        }
        let b = Baseline { path: "/p/b.json".into(), counts, total: 2 };
        let mut now = vec![f(2, "clippy::needless_return"), f(5, "clippy::needless_return"), f(8, "clippy::needless_return"), f(0, "other")];
        mark(root, Some(&b), &mut now, &HashMap::from([(PathBuf::from("/p/src/a.rs"), Some(after.to_string()))]));
        let new: Vec<usize> = now.iter().filter(|x| !x.baseline).map(|x| x.start.line).collect();
        assert_eq!(new, [8, 0], "the third copy and the other rule are new");
    }

    #[test]
    fn save_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mut findings = vec![f(0, "x"), f(1, "y")];
        findings[1].id = 1;
        mark(Path::new("/p"), None, &mut findings, &HashMap::from([(PathBuf::from("/p/src/a.rs"), Some("a\nb\n".to_string()))]));
        let mut r = InspectionResults {
            findings,
            ..InspectionResults::new(JobKind::Inspect, Scope::new(ScopeKind::Project, None), "p")
        };
        r.resolved.insert(1);
        let path = default_path(root);
        let b = save(&path, Path::new("/p"), &r).unwrap();
        assert_eq!(b.total, 1, "fixed problems are not baselined");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"rule\": \"x\""), "{text}");
        assert_eq!(load(&path).unwrap(), b);
        std::fs::write(&path, "{\"format\": \"something-else\"}").unwrap();
        assert!(load(&path).is_err());
        std::fs::write(&path, "nope").unwrap();
        assert!(load(&path).is_err());
        assert!(load(&root.join("missing.json")).is_err());
    }
}
