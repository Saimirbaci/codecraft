//! Changelists: named groups of local changes, like RustRover's. Pure data: which changed file
//! belongs to which list, the active list (where new changes go), and the rules for creating,
//! renaming, deleting and moving. The engine keeps one [`ChangeLists`] per project and persists
//! it as JSON; it calls [`ChangeLists::sync`] with every new status so new changes land in the
//! active list and committed or reverted files leave their list.
//!
//! Files are assigned whole (no partial changelists of hunks). Lists are identified by name
//! (unique, compared exactly).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{GitError, Result};

/// The name of the list every project starts with.
pub const DEFAULT_NAME: &str = "Changes";
/// More lists than this are refused (the file is user-editable; keep it bounded).
pub const MAX_LISTS: usize = 200;
const MAX_NAME: usize = 200;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChangeList {
    pub name: String,
    /// Description; RustRover offers it as the commit message.
    pub comment: String,
    /// Repository-relative paths of the changed files in this list.
    pub files: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChangeLists {
    pub version: u32,
    pub lists: Vec<ChangeList>,
    /// The active list's name: new changes go there.
    pub active: String,
}

impl Default for ChangeLists {
    fn default() -> Self {
        Self {
            version: 1,
            lists: vec![ChangeList { name: DEFAULT_NAME.into(), ..ChangeList::default() }],
            active: DEFAULT_NAME.into(),
        }
    }
}

fn check_name(name: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(GitError::Invalid("enter a name for the changelist".into()));
    }
    if n.chars().count() > MAX_NAME || n.chars().any(char::is_control) {
        return Err(GitError::Invalid("the changelist name is too long or has control characters".into()));
    }
    Ok(n.to_string())
}

impl ChangeLists {
    /// Repair a loaded file: at least one list, unique non-empty names, a file in one list only,
    /// a valid active list.
    pub fn normalize(&mut self) {
        let mut seen_names = BTreeSet::new();
        let mut seen_files = BTreeSet::new();
        self.lists.retain(|l| !l.name.trim().is_empty() && seen_names.insert(l.name.clone()));
        self.lists.truncate(MAX_LISTS);
        for l in &mut self.lists {
            l.files.retain(|f| !f.is_empty() && seen_files.insert(f.clone()));
        }
        if self.lists.is_empty() {
            self.lists.push(ChangeList { name: DEFAULT_NAME.into(), ..ChangeList::default() });
        }
        if !self.lists.iter().any(|l| l.name == self.active) {
            self.active = self.lists.first().map(|l| l.name.clone()).unwrap_or_default();
        }
    }

    pub fn get(&self, name: &str) -> Option<&ChangeList> {
        self.lists.iter().find(|l| l.name == name)
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut ChangeList> {
        self.lists.iter_mut().find(|l| l.name == name)
    }

    fn require(&self, name: &str) -> Result<()> {
        self.get(name).map(|_| ()).ok_or_else(|| GitError::Invalid(format!("there is no changelist '{name}'")))
    }

    /// The list a file is in.
    pub fn list_of(&self, path: &str) -> Option<&str> {
        self.lists.iter().find(|l| l.files.contains(path)).map(|l| l.name.as_str())
    }

    /// Assign changes that are in no list to the active one and drop files that are no longer
    /// changed. Returns whether anything moved.
    pub fn sync<'a>(&mut self, changed: impl IntoIterator<Item = &'a str>) -> bool {
        let changed: BTreeSet<&str> = changed.into_iter().collect();
        let mut moved = false;
        for l in &mut self.lists {
            let before = l.files.len();
            l.files.retain(|f| changed.contains(f.as_str()));
            moved |= before != l.files.len();
        }
        let assigned: BTreeSet<String> = self.lists.iter().flat_map(|l| l.files.iter().cloned()).collect();
        let active = self.active.clone();
        if let Some(target) = self.get_mut(&active) {
            for f in changed {
                if !assigned.contains(f) {
                    target.files.insert(f.to_string());
                    moved = true;
                }
            }
        }
        moved
    }

    /// Group `changed` (repository-relative paths) by list, in list order. Files in no list
    /// (not synced yet) count as the active list's.
    pub fn group<'a>(&self, changed: impl IntoIterator<Item = &'a str>) -> Vec<(String, Vec<String>)> {
        let mut by: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for f in changed {
            let list = self.list_of(f).unwrap_or(self.active.as_str());
            by.entry(list).or_default().push(f.to_string());
        }
        self.lists.iter().map(|l| (l.name.clone(), by.remove(l.name.as_str()).unwrap_or_default())).collect()
    }

    /// Create a list (optionally making it active).
    pub fn create(&mut self, name: &str, comment: &str, make_active: bool) -> Result<String> {
        let name = check_name(name)?;
        if self.get(&name).is_some() {
            return Err(GitError::Invalid(format!("a changelist named '{name}' already exists")));
        }
        if self.lists.len() >= MAX_LISTS {
            return Err(GitError::Invalid("too many changelists".into()));
        }
        self.lists.push(ChangeList { name: name.clone(), comment: comment.to_string(), files: BTreeSet::new() });
        if make_active {
            self.active = name.clone();
        }
        Ok(name)
    }

    /// Rename a list (and set its comment when given).
    pub fn rename(&mut self, old: &str, new: &str, comment: Option<&str>) -> Result<String> {
        self.require(old)?;
        let new = check_name(new)?;
        if new != old && self.get(&new).is_some() {
            return Err(GitError::Invalid(format!("a changelist named '{new}' already exists")));
        }
        if self.active == old {
            self.active = new.clone();
        }
        if let Some(l) = self.get_mut(old) {
            l.name = new.clone();
            if let Some(c) = comment {
                l.comment = c.to_string();
            }
        }
        Ok(new)
    }

    /// Delete a list; its files move to the active list. The active list can't be deleted.
    pub fn delete(&mut self, name: &str) -> Result<()> {
        self.require(name)?;
        if self.active == name {
            return Err(GitError::Invalid(
                "the active changelist can't be deleted: make another one active first".into(),
            ));
        }
        let pos = self.lists.iter().position(|l| l.name == name).unwrap_or(0);
        let removed = self.lists.remove(pos);
        let active = self.active.clone();
        if let Some(a) = self.get_mut(&active) {
            a.files.extend(removed.files);
        }
        Ok(())
    }

    pub fn set_active(&mut self, name: &str) -> Result<()> {
        self.require(name)?;
        self.active = name.to_string();
        Ok(())
    }

    /// Move files to a list (they leave whatever list they were in).
    pub fn move_files(&mut self, paths: &[String], to: &str) -> Result<usize> {
        self.require(to)?;
        for l in &mut self.lists {
            for p in paths {
                l.files.remove(p);
            }
        }
        let mut n = 0;
        if let Some(target) = self.get_mut(to) {
            for p in paths.iter().filter(|p| !p.is_empty()) {
                n += usize::from(target.files.insert(p.clone()));
            }
        }
        Ok(n)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn new_changes_go_to_the_active_list_and_leave_when_committed() {
        let mut c = ChangeLists::default();
        assert!(c.sync(["a.rs", "b.rs"]));
        assert_eq!(c.list_of("a.rs"), Some(DEFAULT_NAME));
        c.create("Feature", "Work on x", true).unwrap();
        assert!(c.sync(["a.rs", "b.rs", "c.rs"]));
        assert_eq!(c.list_of("c.rs"), Some("Feature"));
        assert_eq!(c.list_of("a.rs"), Some(DEFAULT_NAME), "existing assignments stay");
        assert!(!c.sync(["a.rs", "b.rs", "c.rs"]), "nothing moved");
        // b.rs was committed.
        assert!(c.sync(["a.rs", "c.rs"]));
        assert_eq!(c.list_of("b.rs"), None);
        let groups = c.group(["a.rs", "c.rs", "z.rs"]);
        assert_eq!(groups, vec![(DEFAULT_NAME.into(), s(&["a.rs"])), ("Feature".into(), s(&["c.rs", "z.rs"]))]);
    }

    #[test]
    fn create_rename_delete_move() {
        let mut c = ChangeLists::default();
        c.sync(["a", "b"]);
        assert!(c.create("  ", "", false).is_err());
        assert!(c.create(DEFAULT_NAME, "", false).is_err(), "duplicate");
        assert!(c.create("bad\u{7}name", "", false).is_err());
        c.create("Two", "", false).unwrap();
        assert_eq!(c.move_files(&s(&["a"]), "Two").unwrap(), 1);
        assert_eq!(c.list_of("a"), Some("Two"));
        assert!(c.move_files(&s(&["a"]), "Nope").is_err());
        // Renaming the active list keeps it active.
        c.rename(DEFAULT_NAME, "Main", Some("desc")).unwrap();
        assert_eq!(c.active, "Main");
        assert_eq!(c.get("Main").unwrap().comment, "desc");
        assert!(c.rename("Main", "Two", None).is_err(), "taken");
        assert!(c.rename("Ghost", "X", None).is_err());
        // The active list can't be deleted; another can, its files go to the active one.
        assert!(c.delete("Main").is_err());
        c.delete("Two").unwrap();
        assert_eq!(c.list_of("a"), Some("Main"));
        assert!(c.delete("Two").is_err());
        c.create("Three", "", false).unwrap();
        c.set_active("Three").unwrap();
        assert!(c.set_active("Ghost").is_err());
        c.delete("Main").unwrap();
        assert_eq!(c.list_of("b"), Some("Three"));
    }

    #[test]
    fn hostile_files_are_repaired() {
        let mut c: ChangeLists = serde_json::from_str(
            r#"{"lists":[{"name":""},{"name":"A","files":["x","x",""]},{"name":"A"},{"name":"B","files":["x","y"]}],"active":"Q"}"#,
        )
        .unwrap();
        c.normalize();
        assert_eq!(c.lists.len(), 2);
        assert_eq!(c.active, "A");
        assert_eq!(c.get("B").unwrap().files.iter().collect::<Vec<_>>(), vec!["y"]);
        let mut empty: ChangeLists = serde_json::from_str(r#"{"lists":[]}"#).unwrap();
        empty.normalize();
        assert_eq!(empty, ChangeLists::default());
    }
}
