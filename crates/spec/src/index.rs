//! Which code points at which requirement, and whether each requirement is in sync.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::MAX_DANGLING;
use crate::code::Symbol;
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
    /// The item the reference is attached to, when it could be delimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<Symbol>,
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
            let target = LinkRef { path: path.clone(), line: link.line, kind: link.kind, symbol: link.symbol.clone() };
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

/// One piece of code that points at a requirement, as the baseline records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeItem {
    /// `src/auth.rs#pub fn sign_in(user:&str)->bool`: the baseline's key for the item (the signature
    /// without layout, so reformatting it does not rename the item).
    pub key: String,
    /// Relative to the project root, `/`-separated.
    pub path: String,
    /// The item's signature.
    pub symbol: String,
    /// 1-based line of the reference.
    pub line: u32,
    /// Fingerprint of the item's code (comments and whitespace ignored).
    pub fingerprint: String,
}

/// The items `links` are attached to (references whose item could not be delimited are not
/// tracked). Two references to one item count once; two items with the same signature in a file
/// get a `#2` suffix, in order.
pub fn code_items(root: &Path, links: &[LinkRef]) -> Vec<CodeItem> {
    let mut seen: BTreeSet<(&Path, u32)> = BTreeSet::new();
    let mut keys: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for link in links {
        let Some(symbol) = &link.symbol else { continue };
        if !seen.insert((link.path.as_path(), symbol.start_line)) {
            continue;
        }
        let rel = link.path.strip_prefix(root).unwrap_or(&link.path).to_string_lossy().replace('\\', "/");
        let identity = crate::code::compact(&symbol.signature);
        let mut key = format!("{rel}#{identity}");
        let mut n = 1u32;
        while !keys.insert(key.clone()) {
            n = n.saturating_add(1);
            key = format!("{rel}#{identity}#{n}");
        }
        out.push(CodeItem {
            key,
            path: rel,
            symbol: symbol.signature.clone(),
            line: link.line,
            fingerprint: symbol.fingerprint.clone(),
        });
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CodeChangeKind {
    /// The item's code differs from the synced code.
    Modified,
    /// An item that pointed at the requirement at the last sync no longer does (deleted, renamed,
    /// its signature changed, or the reference was removed).
    Removed,
    /// An item points at the requirement that did not at the last sync.
    Added,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeChange {
    pub path: String,
    pub symbol: String,
    /// 1-based line of the reference; `None` for a removed item.
    pub line: Option<u32>,
    pub change: CodeChangeKind,
}

/// How the code that points at a requirement differs from what was recorded when it was synced
/// (`synced`: item key → fingerprint). `None` when nothing was recorded (a baseline from before
/// code was fingerprinted): then nothing is compared.
pub fn code_changes(synced: Option<&BTreeMap<String, String>>, now: &[CodeItem]) -> Vec<CodeChange> {
    let Some(synced) = synced else { return Vec::new() };
    let mut out = Vec::new();
    for item in now {
        let change = match synced.get(&item.key) {
            None => CodeChangeKind::Added,
            Some(fp) if *fp != item.fingerprint => CodeChangeKind::Modified,
            Some(_) => continue,
        };
        out.push(CodeChange { path: item.path.clone(), symbol: item.symbol.clone(), line: Some(item.line), change });
    }
    let current: BTreeSet<&str> = now.iter().map(|i| i.key.as_str()).collect();
    for key in synced.keys().filter(|k| !current.contains(k.as_str())) {
        let (path, symbol) = key.split_once('#').unwrap_or((key.as_str(), ""));
        out.push(CodeChange {
            path: path.to_string(),
            symbol: symbol.to_string(),
            line: None,
            change: CodeChangeKind::Removed,
        });
    }
    out
}

/// Where a requirement stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SpecStatus {
    /// Its text changed since the last sync: the code may no longer match (needs implementing).
    SpecChanged,
    /// The code that points at it changed since the last sync (the spec did not): the spec may no
    /// longer describe the code (needs reconciling).
    CodeChanged,
    /// No code points at it.
    Unlinked,
    /// Code points at it; no sync has been recorded yet.
    Linked,
    /// Code points at it and its text is the one recorded at the last sync.
    InSync,
}

/// `links`: how many places in code point at the requirement; `synced`: the fingerprint recorded
/// at the last sync, if any; `code_changed`: the code differs from the code recorded then.
pub fn status(req: &Requirement, links: usize, synced: Option<&str>, code_changed: bool) -> SpecStatus {
    match synced {
        Some(h) if h != req.hash => SpecStatus::SpecChanged,
        Some(_) if code_changed => SpecStatus::CodeChanged,
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
        CodeLink { feature: feature.map(str::to_string), id: id.into(), line, kind: LinkKind::Comment, symbol: None }
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
        assert_eq!(status(r, 0, None, false), SpecStatus::Unlinked);
        assert_eq!(status(r, 2, None, false), SpecStatus::Linked);
        assert_eq!(status(r, 2, Some(&r.hash), false), SpecStatus::InSync);
        assert_eq!(status(r, 0, Some(&r.hash), false), SpecStatus::Unlinked);
        assert_eq!(status(r, 2, Some("0000000000000000"), false), SpecStatus::SpecChanged);
        assert_eq!(status(r, 0, Some("0000000000000000"), false), SpecStatus::SpecChanged, "a changed text wins");
        assert_eq!(status(r, 2, Some(&r.hash), true), SpecStatus::CodeChanged);
        assert_eq!(status(r, 0, Some(&r.hash), true), SpecStatus::CodeChanged, "removed code is a change too");
        assert_eq!(
            status(r, 2, Some("0000000000000000"), true),
            SpecStatus::SpecChanged,
            "the spec wins over the code"
        );
    }

    fn item_link(path: &str, signature: &str, start: u32, fp: &str) -> LinkRef {
        LinkRef {
            path: PathBuf::from(path),
            line: start.saturating_sub(1).max(1),
            kind: LinkKind::Comment,
            symbol: Some(Symbol {
                signature: signature.into(),
                start_line: start,
                end_line: start + 2,
                fingerprint: fp.into(),
            }),
        }
    }

    #[test]
    fn code_items_are_keyed_by_file_and_signature_and_deduplicated() {
        let links = [
            item_link("/p/src/a.rs", "fn f()", 3, "aa"),
            item_link("/p/src/a.rs", "fn f()", 3, "aa"),
            item_link("/p/src/a.rs", "fn f()", 20, "bb"),
            item_link("/p/src/b.rs", "fn f()", 3, "cc"),
            LinkRef { path: PathBuf::from("/p/src/c.rs"), line: 1, kind: LinkKind::Attribute, symbol: None },
        ];
        let items = code_items(Path::new("/p"), &links);
        let keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["src/a.rs#fn f()", "src/a.rs#fn f()#2", "src/b.rs#fn f()"]);
        assert_eq!(items[0].path, "src/a.rs");
        assert_eq!(items[0].line, 2);
    }

    #[test]
    fn code_changes_compare_with_the_synced_snapshot() {
        let now = code_items(
            Path::new("/p"),
            &[
                item_link("/p/a.rs", "fn same()", 1, "11"),
                item_link("/p/a.rs", "fn edited()", 9, "22"),
                item_link("/p/a.rs", "fn new()", 20, "33"),
            ],
        );
        assert!(code_changes(None, &now).is_empty(), "nothing recorded: nothing to compare");
        let synced: BTreeMap<String, String> =
            [("a.rs#fn same()", "11"), ("a.rs#fn edited()", "00"), ("a.rs#fn gone()", "44")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        let changes = code_changes(Some(&synced), &now);
        let view: Vec<(CodeChangeKind, &str)> = changes.iter().map(|c| (c.change, c.symbol.as_str())).collect();
        assert_eq!(
            view,
            [
                (CodeChangeKind::Modified, "fn edited()"),
                (CodeChangeKind::Added, "fn new()"),
                (CodeChangeKind::Removed, "fn gone()"),
            ]
        );
        assert_eq!(changes[2].line, None);
        assert!(code_changes(Some(&BTreeMap::new()), &[]).is_empty());
        assert_eq!(code_changes(Some(&BTreeMap::new()), &now).len(), 3, "everything is new after an empty snapshot");
    }
}
