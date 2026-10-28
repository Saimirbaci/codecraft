//! Reading a feature directory.

use std::fs;
use std::path::{Path, PathBuf};

use crate::links::is_feature_name;
use crate::model::{Diagnostic, Feature, Severity};
use crate::requirements::parse_requirements;
use crate::tasks::{parse_outline, parse_tasks};
use crate::{DESIGN_FILE, MAX_FEATURES, MAX_FILE_BYTES, REQUIREMENTS_FILE, SPEC_DIR, TASKS_FILE};

/// Build a [`Feature`] from the text of its files (`None` = the file does not exist). Pure: the
/// loaders below read the files and call this.
pub fn parse_feature(
    name: &str,
    dir: PathBuf,
    requirements: Option<&str>,
    design: Option<&str>,
    tasks: Option<&str>,
) -> Feature {
    let mut f = Feature { name: name.to_string(), dir: dir.clone(), ..Feature::default() };
    if let Some(text) = requirements {
        let doc = parse_requirements(text);
        f.requirements_file = Some(dir.join(REQUIREMENTS_FILE));
        f.title = doc.title;
        f.requirements = doc.requirements;
        f.diagnostics = doc.diagnostics;
    }
    if let Some(text) = design {
        f.design_file = Some(dir.join(DESIGN_FILE));
        f.design = parse_outline(text);
    }
    if let Some(text) = tasks {
        f.tasks_file = Some(dir.join(TASKS_FILE));
        f.tasks = parse_tasks(text);
        // A task that names a requirement nobody wrote is a typo or a deleted requirement.
        if requirements.is_some() {
            for t in &f.tasks {
                for id in &t.requirements {
                    if f.requirement(id).is_none() {
                        f.diagnostics.push(Diagnostic {
                            file: TASKS_FILE.into(),
                            line: t.line,
                            severity: Severity::Warning,
                            message: format!("{id} is not a requirement of this feature"),
                        });
                    }
                }
            }
        }
    }
    f
}

/// The text of a spec file: `Ok(None)` when it does not exist (or is not a regular file; links
/// are not followed), `Err` with a reason when it cannot be read.
fn read_spec_file(path: &Path) -> Result<Option<String>, String> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!("larger than {} KiB: not read", MAX_FILE_BYTES / 1024));
    }
    fs::read_to_string(path).map(Some).map_err(|e| e.to_string())
}

/// Read one feature directory. `None` when it holds none of the three files.
pub fn load_feature(dir: &Path) -> Option<Feature> {
    let name = dir.file_name()?.to_str()?.to_string();
    let mut errors: Vec<Diagnostic> = Vec::new();
    let mut read = |file: &str| match read_spec_file(&dir.join(file)) {
        Ok(text) => text,
        Err(message) => {
            errors.push(Diagnostic { file: file.into(), line: 0, severity: Severity::Error, message });
            None
        }
    };
    let requirements = read(REQUIREMENTS_FILE);
    let design = read(DESIGN_FILE);
    let tasks = read(TASKS_FILE);
    if requirements.is_none() && design.is_none() && tasks.is_none() && errors.is_empty() {
        return None;
    }
    let mut f = parse_feature(&name, dir.to_path_buf(), requirements.as_deref(), design.as_deref(), tasks.as_deref());
    errors.append(&mut f.diagnostics);
    f.diagnostics = errors;
    Some(f)
}

/// Every feature under `<root>/specs/`, by name. A missing directory is no specs, not an error;
/// links, hidden entries and names that are not feature names are skipped.
pub fn load_features(root: &Path) -> Vec<Feature> {
    let Ok(entries) = fs::read_dir(root.join(SPEC_DIR)) else { return Vec::new() };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name().to_str().is_some_and(is_feature_name))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.into_iter().take(MAX_FEATURES).filter_map(|d| load_feature(&d)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, file: &str, text: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(file), text).unwrap();
    }

    #[test]
    fn parses_the_three_files_and_flags_unknown_task_references() {
        let f = parse_feature(
            "login",
            PathBuf::from("/p/specs/login"),
            Some("# Requirements: Login\n## REQ-1: A\nTHE SYSTEM SHALL a.\n"),
            Some("# Design\n## Flow\n"),
            Some("- [ ] 1. Do it _Requirements: REQ-1, REQ-9_\n"),
        );
        assert_eq!(f.title.as_deref(), Some("Requirements: Login"));
        assert_eq!(f.requirements.len(), 1);
        assert_eq!(f.design.len(), 2);
        assert_eq!(f.task_progress(), (0, 1));
        assert_eq!(f.diagnostics.len(), 1);
        assert!(f.diagnostics[0].message.starts_with("REQ-9"));
        assert_eq!((f.diagnostics[0].file.as_str(), f.diagnostics[0].line), ("tasks.md", 1));
        assert_eq!(f.requirements_file, Some(PathBuf::from("/p/specs/login/requirements.md")));
    }

    #[test]
    fn loads_features_in_name_order_and_skips_what_is_not_a_feature() {
        let root = tempfile::tempdir().unwrap();
        let specs = root.path().join("specs");
        write(&specs.join("b-feature"), "requirements.md", "## REQ-2: B\nTHE SYSTEM SHALL b.\n");
        write(&specs.join("a-feature"), "tasks.md", "- [x] 1. Done\n");
        write(&specs.join(".hidden"), "requirements.md", "## REQ-3\n");
        write(&specs.join("empty"), "notes.txt", "nothing");
        fs::write(specs.join("stray.md"), "# stray").unwrap();
        let names: Vec<_> = load_features(root.path()).into_iter().map(|f| f.name).collect();
        assert_eq!(names, ["a-feature", "b-feature"]);
    }

    #[test]
    fn a_missing_specs_directory_is_no_specs() {
        let root = tempfile::tempdir().unwrap();
        assert!(load_features(root.path()).is_empty());
        assert!(load_feature(&root.path().join("nope")).is_none());
    }

    #[test]
    fn unreadable_files_become_diagnostics_not_failures() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("specs").join("f");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("requirements.md"), [0xff, 0xfe, 0x00, 0x80]).unwrap();
        fs::write(dir.join("tasks.md"), vec![b'x'; usize::try_from(MAX_FILE_BYTES).unwrap() + 1]).unwrap();
        let f = load_feature(&dir).unwrap();
        assert_eq!(f.diagnostics.len(), 2);
        assert!(f.diagnostics.iter().all(|d| d.severity == Severity::Error));
        assert!(f.requirements.is_empty() && f.tasks.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_and_directories_are_not_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(outside.path(), "requirements.md", "## REQ-1: secret\nTHE SYSTEM SHALL x.\n");
        let specs = root.path().join("specs");
        fs::create_dir_all(&specs).unwrap();
        std::os::unix::fs::symlink(outside.path(), specs.join("linked-dir")).unwrap();
        let real = specs.join("real");
        fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(outside.path().join("requirements.md"), real.join("requirements.md")).unwrap();
        assert!(load_features(root.path()).is_empty());
    }
}
