//! What the spec view writes: the sync baseline (private to this checkout) and new feature
//! scaffolds. Existing spec files are never overwritten.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::feature::load_feature;
use crate::index::{CodeItem, requirement_key};
use crate::links::is_feature_name;
use crate::model::Feature;
use crate::{DESIGN_FILE, REQUIREMENTS_FILE, SPEC_DIR, TASKS_FILE};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecError {
    /// The request itself is wrong (a bad name, a feature that exists, an unknown requirement).
    Invalid(String),
    /// The file system refused.
    Io(String),
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(m) | Self::Io(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SpecError {}

fn io(context: &str, e: &std::io::Error) -> SpecError {
    SpecError::Io(format!("{context}: {e}"))
}

/// The fingerprints recorded when requirements were last marked in sync with the code.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Baseline {
    #[serde(default)]
    pub version: u32,
    /// By [`requirement_key`] (`login/REQ-3`) → fingerprint.
    #[serde(default)]
    pub requirements: BTreeMap<String, String>,
    /// By [`requirement_key`] → the code that pointed at the requirement when it was synced
    /// (item key → fingerprint, see [`CodeItem`]). Missing for requirements synced before code
    /// was fingerprinted: their code is not compared until they are synced again.
    #[serde(default)]
    pub code: BTreeMap<String, BTreeMap<String, String>>,
}

pub fn baseline_path(root: &Path) -> PathBuf {
    root.join(".codecraft").join("spec-state.json")
}

/// The saved baseline; missing, unreadable or oversized files read as empty (everything is
/// then simply "not synced").
pub fn load_baseline(root: &Path) -> Baseline {
    let path = baseline_path(root);
    let ok = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() <= 4 * 1024 * 1024);
    if !ok {
        return Baseline::default();
    }
    fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Save atomically (a temporary file in the same directory, then a rename).
pub fn save_baseline(root: &Path, baseline: &Baseline) -> Result<(), SpecError> {
    let path = baseline_path(root);
    let dir = path.parent().ok_or_else(|| SpecError::Invalid("no directory for the spec state".into()))?;
    fs::create_dir_all(dir).map_err(|e| io("cannot create .codecraft", &e))?;
    let text = serde_json::to_string_pretty(&Baseline {
        version: 1,
        requirements: baseline.requirements.clone(),
        code: baseline.code.clone(),
    })
    .map_err(|e| SpecError::Invalid(e.to_string()))?;
    let tmp = dir.join(format!(".spec-state.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, &path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io("cannot save the spec state", &e)
    })
}

/// Record the current fingerprints of `feature`'s requirements (only `only`, when given) and of
/// the code that points at them (`code`: by [`requirement_key`]) as in sync, and forget
/// requirements that no longer exist. Returns how many were recorded.
pub fn mark_synced(
    baseline: &mut Baseline,
    feature: &Feature,
    only: Option<&str>,
    code: &BTreeMap<String, Vec<CodeItem>>,
) -> Result<usize, SpecError> {
    if let Some(id) = only
        && feature.requirement(id).is_none()
    {
        return Err(SpecError::Invalid(format!("{} has no requirement {id}", feature.name)));
    }
    let prefix = requirement_key(&feature.name, "");
    let exists = |k: &String| k.strip_prefix(prefix.as_str()).is_none_or(|id| feature.requirement(id).is_some());
    baseline.requirements.retain(|k, _| exists(k));
    baseline.code.retain(|k, _| exists(k));
    let mut n = 0;
    for r in feature.requirements.iter().filter(|r| only.is_none_or(|id| id == r.id)) {
        let key = requirement_key(&feature.name, &r.id);
        let items = code.get(&key).map(Vec::as_slice).unwrap_or_default();
        baseline.code.insert(key.clone(), items.iter().map(|i| (i.key.clone(), i.fingerprint.clone())).collect());
        baseline.requirements.insert(key, r.hash.clone());
        n += 1;
    }
    Ok(n)
}

/// One more than the highest requirement number in any feature: new requirements get ids that
/// are unique across the project, so a bare `REQ-n` in code is unambiguous.
pub fn next_requirement_number(features: &[Feature]) -> u32 {
    features.iter().flat_map(|f| f.requirements.iter()).map(|r| r.number).max().map_or(1, |n| n.saturating_add(1))
}

/// `Login & Sessions` → `login-sessions`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if matches!(c, ' ' | '-' | '_' | '.') && !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').chars().take(64).collect()
}

/// Create `specs/<name>/` with a starting `requirements.md`, `design.md` and `tasks.md`.
/// `first_number` is the id of the placeholder requirement ([`next_requirement_number`]).
/// Refuses a feature that already exists; nothing is overwritten.
pub fn new_feature(root: &Path, name: &str, first_number: u32) -> Result<Feature, SpecError> {
    let slug = slug(name);
    if slug.is_empty() || !is_feature_name(&slug) {
        return Err(SpecError::Invalid(format!("`{name}` is not a usable feature name (use letters and digits)")));
    }
    let specs = root.join(SPEC_DIR);
    let dir = specs.join(&slug);
    if fs::symlink_metadata(&specs).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(SpecError::Invalid(format!("{SPEC_DIR} is a symbolic link: not writing through it")));
    }
    fs::create_dir_all(&specs).map_err(|e| io(&format!("cannot create {SPEC_DIR}"), &e))?;
    match fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(SpecError::Invalid(format!("the feature `{slug}` already exists")));
        }
        Err(e) => return Err(io("cannot create the feature", &e)),
    }
    let title = title_of(name, &slug);
    let n = first_number.max(1);
    let files = [
        (
            REQUIREMENTS_FILE,
            format!(
                "# Requirements: {title}\n\n## REQ-{n}: <short title>\n\nWHEN <trigger> THE SYSTEM SHALL <response>.\n"
            ),
        ),
        (DESIGN_FILE, format!("# Design: {title}\n\n## Overview\n\n## Components\n")),
        (TASKS_FILE, format!("# Tasks: {title}\n\n- [ ] 1. <first task> _Requirements: REQ-{n}_\n")),
    ];
    for (file, text) in files {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(file))
            .map_err(|e| io(&format!("cannot create {file}"), &e))?;
        f.write_all(text.as_bytes()).map_err(|e| io(&format!("cannot write {file}"), &e))?;
    }
    load_feature(&dir).ok_or_else(|| SpecError::Io("the new feature cannot be read back".into()))
}

/// The title for the scaffold: what the user typed when it has words, else the slug.
fn title_of(name: &str, slug: &str) -> String {
    let t: String = name.trim().chars().filter(|c| !c.is_control()).take(80).collect();
    if t.is_empty() { slug.to_string() } else { t }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::parse_feature;

    #[test]
    fn slugs() {
        assert_eq!(slug("Login & Sessions"), "login-sessions");
        assert_eq!(slug("  --a__b..c  "), "a-b-c");
        assert_eq!(slug("../../etc"), "etc");
        assert_eq!(slug("é"), "");
        assert_eq!(slug(&"a".repeat(100)).len(), 64);
        assert_eq!(slug("x/y"), "xy");
    }

    #[test]
    fn new_feature_scaffolds_three_readable_files_and_never_overwrites() {
        let root = tempfile::tempdir().unwrap();
        let f = new_feature(root.path(), "User Login", 7).unwrap();
        assert_eq!(f.name, "user-login");
        assert_eq!(f.title.as_deref(), Some("Requirements: User Login"));
        assert_eq!(f.requirements.len(), 1);
        assert_eq!(f.requirements[0].id, "REQ-7");
        assert_eq!(f.tasks[0].requirements, ["REQ-7"]);
        assert!(f.diagnostics.is_empty(), "{:?}", f.diagnostics);
        assert_eq!(f.design.len(), 3);
        let again = new_feature(root.path(), "user login", 8);
        assert!(matches!(again, Err(SpecError::Invalid(m)) if m.contains("already exists")));
        assert!(matches!(new_feature(root.path(), "???", 1), Err(SpecError::Invalid(_))));
        assert!(matches!(new_feature(root.path(), "..", 1), Err(SpecError::Invalid(_))));
    }

    #[cfg(unix)]
    #[test]
    fn new_feature_does_not_write_through_a_symlinked_specs_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("specs")).unwrap();
        assert!(matches!(new_feature(root.path(), "x", 1), Err(SpecError::Invalid(_))));
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn baseline_round_trips_and_bad_files_read_as_empty() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load_baseline(root.path()), Baseline::default());
        let mut b = Baseline::default();
        b.requirements.insert("a/REQ-1".into(), "0123456789abcdef".into());
        save_baseline(root.path(), &b).unwrap();
        let loaded = load_baseline(root.path());
        assert_eq!(loaded.requirements, b.requirements);
        assert_eq!(loaded.version, 1);
        fs::write(baseline_path(root.path()), "{ not json").unwrap();
        assert_eq!(load_baseline(root.path()), Baseline::default());
        fs::write(baseline_path(root.path()), r#"{"requirements": 5}"#).unwrap();
        assert_eq!(load_baseline(root.path()), Baseline::default());
        fs::write(baseline_path(root.path()), r#"{"requirements": {"a/REQ-1": "x"}, "future": true}"#).unwrap();
        assert_eq!(load_baseline(root.path()).requirements.len(), 1, "unknown fields are ignored");
    }

    #[test]
    fn mark_synced_records_prunes_and_validates() {
        let f = parse_feature("a", PathBuf::from("a"), Some("## REQ-1\nx\n## REQ-2\ny\n"), None, None);
        let mut b = Baseline::default();
        b.requirements.insert("a/REQ-9".into(), "stale".into());
        b.requirements.insert("other/REQ-1".into(), "kept".into());
        assert_eq!(mark_synced(&mut b, &f, Some("REQ-2"), &BTreeMap::new()).unwrap(), 1);
        assert!(!b.requirements.contains_key("a/REQ-9"), "deleted requirements are forgotten");
        assert_eq!(b.requirements.get("other/REQ-1").map(String::as_str), Some("kept"));
        assert!(b.requirements.contains_key("a/REQ-2") && !b.requirements.contains_key("a/REQ-1"));
        assert_eq!(mark_synced(&mut b, &f, None, &BTreeMap::new()).unwrap(), 2);
        assert!(matches!(mark_synced(&mut b, &f, Some("REQ-5"), &BTreeMap::new()), Err(SpecError::Invalid(_))));
    }

    #[test]
    fn mark_synced_records_the_code_snapshot_and_old_baselines_still_load() {
        let f = parse_feature("a", PathBuf::from("a"), Some("## REQ-1\nx\n## REQ-2\ny\n"), None, None);
        let item = |key: &str, fp: &str| CodeItem {
            key: key.into(),
            path: "src/a.rs".into(),
            symbol: "fn f()".into(),
            line: 1,
            fingerprint: fp.into(),
        };
        let mut code = BTreeMap::new();
        code.insert("a/REQ-1".to_string(), vec![item("src/a.rs#fn f()", "aa"), item("src/a.rs#fn g()", "bb")]);
        let mut b = Baseline::default();
        b.code.insert("a/REQ-9".into(), BTreeMap::new());
        mark_synced(&mut b, &f, None, &code).unwrap();
        assert_eq!(b.code["a/REQ-1"].len(), 2);
        assert_eq!(b.code["a/REQ-1"]["src/a.rs#fn g()"], "bb");
        assert!(b.code["a/REQ-2"].is_empty(), "synced with no code: tracked, and empty");
        assert!(!b.code.contains_key("a/REQ-9"), "deleted requirements are forgotten");
        // It survives a save and a load.
        let root = tempfile::tempdir().unwrap();
        save_baseline(root.path(), &b).unwrap();
        assert_eq!(load_baseline(root.path()).code, b.code);
        // A baseline written before code was fingerprinted has none, and loads.
        fs::write(baseline_path(root.path()), r#"{"version": 1, "requirements": {"a/REQ-1": "x"}}"#).unwrap();
        let old = load_baseline(root.path());
        assert_eq!(old.requirements.len(), 1);
        assert!(old.code.is_empty());
    }

    #[test]
    fn requirement_numbers_are_project_wide() {
        assert_eq!(next_requirement_number(&[]), 1);
        let a = parse_feature("a", PathBuf::from("a"), Some("## REQ-3\nx\n"), None, None);
        let b = parse_feature("b", PathBuf::from("b"), Some("## REQ-11\nx\n## REQ-2\ny\n"), None, None);
        assert_eq!(next_requirement_number(&[a, b]), 12);
    }
}
