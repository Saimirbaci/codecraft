//! Which code points at which requirement, and whether each requirement is in sync.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::MAX_DANGLING;
use crate::model::{CodeLink, Feature, LinkKind, Requirement};

/// `login/REQ-3`: how a requirement is named across features (the key of the maps below).
pub fn requirement_key(feature: &str, id: &str) -> String {
    format!("{feature}/{id}")
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRef {
    pub path: PathBuf,
    /// 1-based.
    pub line: u32,
    pub kind: LinkKind,
}

/// A reference that names no requirement (or, for `ambiguous`, several).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dangling {
    pub path: PathBuf,
    pub line: u32,
    /// As written: `REQ-9` or `login/REQ-9`.
    pub reference: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkIndex {
    /// By [`requirement_key`].
    pub by_requirement: BTreeMap<String, Vec<LinkRef>>,
    /// References to requirements that do not exist: the requirement was deleted, renumbered or
    /// mistyped. The code and the spec disagree.
    pub dangling: Vec<Dangling>,
    /// Bare `REQ-n` references that more than one feature defines (qualify them: `login/REQ-n`).
    pub ambiguous: Vec<Dangling>,
}

impl LinkIndex {
    pub fn links_of(&self, feature: &str, id: &str) -> &[LinkRef] {
        self.by_requirement.get(&requirement_key(feature, id)).map_or(&[], Vec::as_slice)
    }
}

/// Resolve the references found in code (`files`: by file) against the features' requirements.
pub fn build_index(features: &[Feature], files: &BTreeMap<PathBuf, Vec<CodeLink>>) -> LinkIndex {
    let mut index = LinkIndex::default();
    for (path, links) in files {
        for link in links {
            let reference = match &link.feature {
                Some(f) => requirement_key(f, &link.id),
                None => link.id.clone(),
            };
            let target = LinkRef { path: path.clone(), line: link.line, kind: link.kind };
            let owners: Vec<&Feature> = features
                .iter()
                .filter(|f| link.feature.as_ref().is_none_or(|name| *name == f.name))
                .filter(|f| f.requirement(&link.id).is_some())
                .collect();
            if owners.is_empty() {
                if index.dangling.len() < MAX_DANGLING {
                    index.dangling.push(Dangling { path: path.clone(), line: link.line, reference });
                }
                continue;
            }
            if owners.len() > 1 && index.ambiguous.len() < MAX_DANGLING {
                index.ambiguous.push(Dangling { path: path.clone(), line: link.line, reference });
            }
            for f in owners {
                index.by_requirement.entry(requirement_key(&f.name, &link.id)).or_default().push(target.clone());
            }
        }
    }
    index
}

/// Where a requirement stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpecStatus {
    /// Its text changed since the last sync: the code may no longer match (needs implementing).
    SpecChanged,
    /// No code points at it.
    Unlinked,
    /// Code points at it; no sync has been recorded yet.
    Linked,
    /// Code points at it and its text is the one recorded at the last sync.
    InSync,
}

/// `links`: how many places in code point at the requirement; `synced`: the fingerprint recorded
/// at the last sync, if any.
pub fn status(req: &Requirement, links: usize, synced: Option<&str>) -> SpecStatus {
    match synced {
        Some(h) if h != req.hash => SpecStatus::SpecChanged,
        _ if links == 0 => SpecStatus::Unlinked,
        Some(_) => SpecStatus::InSync,
        None => SpecStatus::Linked,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::parse_feature;

    fn feature(name: &str, text: &str) -> Feature {
        parse_feature(name, PathBuf::from(name), Some(text), None, None)
    }

    fn link(feature: Option<&str>, id: &str, line: u32) -> CodeLink {
        CodeLink { feature: feature.map(str::to_string), id: id.into(), line, kind: LinkKind::Comment }
    }

    #[test]
    fn resolves_bare_and_qualified_references() {
        let fs = [feature("a", "## REQ-1\nx\n## REQ-2\ny\n"), feature("b", "## REQ-2\nz\n")];
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("src/x.rs"),
            vec![link(None, "REQ-1", 3), link(Some("b"), "REQ-2", 4), link(None, "REQ-2", 5)],
        );
        files.insert(
            PathBuf::from("src/y.rs"),
            vec![link(None, "REQ-7", 1), link(Some("c"), "REQ-1", 2), link(Some("a"), "REQ-5", 3)],
        );
        let idx = build_index(&fs, &files);
        assert_eq!(idx.links_of("a", "REQ-1").len(), 1);
        assert_eq!(idx.links_of("a", "REQ-2").len(), 1, "the bare REQ-2 reaches both features");
        assert_eq!(idx.links_of("b", "REQ-2").len(), 2, "qualified + bare");
        let dangling: Vec<_> = idx.dangling.iter().map(|d| d.reference.as_str()).collect();
        assert_eq!(dangling, ["REQ-7", "c/REQ-1", "a/REQ-5"]);
        assert_eq!(idx.ambiguous.len(), 1);
        assert_eq!(idx.ambiguous[0].line, 5);
        assert!(idx.links_of("a", "REQ-9").is_empty());
    }

    #[test]
    fn dangling_lists_are_capped() {
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("f"), (0..MAX_DANGLING as u32 + 50).map(|n| link(None, "REQ-1", n)).collect());
        assert_eq!(build_index(&[], &files).dangling.len(), MAX_DANGLING);
    }

    #[test]
    fn status_rules() {
        let f = feature("a", "## REQ-1\nTHE SYSTEM SHALL x.\n");
        let r = &f.requirements[0];
        assert_eq!(status(r, 0, None), SpecStatus::Unlinked);
        assert_eq!(status(r, 2, None), SpecStatus::Linked);
        assert_eq!(status(r, 2, Some(&r.hash)), SpecStatus::InSync);
        assert_eq!(status(r, 0, Some(&r.hash)), SpecStatus::Unlinked);
        assert_eq!(status(r, 2, Some("0000000000000000")), SpecStatus::SpecChanged);
        assert_eq!(status(r, 0, Some("0000000000000000")), SpecStatus::SpecChanged, "a changed text wins");
    }
}
