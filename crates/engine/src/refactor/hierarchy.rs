//! The Hierarchy tool window's model: call hierarchy (callers/callees via LSP callHierarchy),
//! type hierarchy (LSP typeHierarchy, or implementations when the server lacks it) and method
//! hierarchy (implementations of a trait method). A lazily expanded tree in an arena.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use codecraft_lsp::lsp_types as lt;

use crate::convert::utf16_col_to_char;

/// Most nodes one hierarchy holds (recursive call graphs are expanded lazily, one level a time).
pub const MAX_NODES: usize = 5000;
/// Deepest level that can be expanded.
pub const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HierarchyKind {
    Call,
    Type,
    Method,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Callers,
    Callees,
    Subtypes,
    Supertypes,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Callers => "Callers",
            Self::Callees => "Callees",
            Self::Subtypes => "Subtypes",
            Self::Supertypes => "Supertypes",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "callers" | "incoming" => Self::Callers,
            "callees" | "outgoing" => Self::Callees,
            "subtypes" => Self::Subtypes,
            "supertypes" => Self::Supertypes,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HierarchyNode {
    pub name: String,
    pub detail: Option<String>,
    /// "fn", "method", "struct", "impl", …
    pub kind: String,
    pub path: PathBuf,
    /// Zero-based line and char column of the symbol's name.
    pub line: usize,
    pub column: usize,
    /// Number of call sites (call hierarchy children).
    pub sites: usize,
    pub depth: usize,
    /// `None` until loaded; leaves have `Some(empty)`.
    pub children: Option<Vec<usize>>,
    pub expanded: bool,
    pub loading: bool,
    /// The server's hierarchy item (sent back to expand the node); `None` for leaves.
    #[serde(skip)]
    pub item: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hierarchy {
    pub kind: HierarchyKind,
    pub direction: Direction,
    pub title: String,
    pub nodes: Vec<HierarchyNode>,
    pub roots: Vec<usize>,
    pub selected: Option<usize>,
    /// Shown instead of the tree ("No call hierarchy at the caret", …).
    pub message: Option<String>,
    /// Waiting for the roots.
    pub pending: bool,
    /// The server answers typeHierarchy (else subtypes come from implementations).
    pub server_types: bool,
    /// Bumped on every new hierarchy so late responses for an old one are dropped.
    #[serde(skip)]
    pub(crate) generation: u64,
}

impl Hierarchy {
    pub(crate) fn new(kind: HierarchyKind, title: String, generation: u64) -> Self {
        let direction = match kind {
            HierarchyKind::Call => Direction::Callers,
            HierarchyKind::Type | HierarchyKind::Method => Direction::Subtypes,
        };
        Self {
            kind,
            direction,
            title,
            nodes: Vec::new(),
            roots: Vec::new(),
            selected: None,
            message: None,
            pending: true,
            server_types: kind == HierarchyKind::Type,
            generation,
        }
    }

    /// The expanded tree flattened for display: `(node, depth)`.
    pub fn visible_rows(&self) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut stack: Vec<usize> = self.roots.iter().rev().copied().collect();
        while let Some(i) = stack.pop() {
            let Some(n) = self.nodes.get(i) else { continue };
            out.push((i, n.depth));
            if out.len() > MAX_NODES {
                break;
            }
            if n.expanded
                && let Some(children) = &n.children
            {
                stack.extend(children.iter().rev().copied());
            }
        }
        out
    }

    /// Add a node (capped). Returns its index.
    pub(crate) fn push(&mut self, node: HierarchyNode) -> Option<usize> {
        if self.nodes.len() >= MAX_NODES {
            self.message = Some(format!("Only the first {MAX_NODES} entries are shown"));
            return None;
        }
        self.nodes.push(node);
        Some(self.nodes.len() - 1)
    }

    /// Forget every loaded child (direction changed): roots stay, collapsed and unloaded.
    pub(crate) fn reset_children(&mut self) {
        let roots: Vec<HierarchyNode> = self.roots.iter().filter_map(|i| self.nodes.get(*i).cloned()).collect();
        self.nodes.clear();
        self.roots.clear();
        for mut r in roots {
            r.children = None;
            r.expanded = false;
            r.loading = false;
            if let Some(i) = self.push(r) {
                self.roots.push(i);
            }
        }
        self.selected = self.roots.first().copied();
        self.message = None;
    }
}

pub(crate) fn symbol_kind_label(k: u64) -> &'static str {
    match k {
        2 => "mod",
        5 => "struct",
        6 => "method",
        10 => "enum",
        11 => "trait",
        12 => "fn",
        13 => "variable",
        14 => "const",
        19 => "impl",
        22 => "variant",
        23 => "struct",
        26 => "type",
        _ => "symbol",
    }
}

/// Reads lines of files once per response (open documents first, then disk).
pub(crate) struct LineCache<'a> {
    read: &'a dyn Fn(&Path) -> Option<String>,
    files: HashMap<PathBuf, Vec<String>>,
}

impl<'a> LineCache<'a> {
    pub(crate) fn new(read: &'a dyn Fn(&Path) -> Option<String>) -> Self {
        Self { read, files: HashMap::new() }
    }

    pub(crate) fn line(&mut self, path: &Path, line: usize) -> String {
        let read = self.read;
        let lines = self
            .files
            .entry(path.to_path_buf())
            .or_insert_with(|| read(path).map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default());
        lines.get(line).cloned().unwrap_or_default()
    }
}

/// A node from an LSP CallHierarchyItem / TypeHierarchyItem (raw JSON).
pub(crate) fn node_from_item(item: &Value, depth: usize, lines: &mut LineCache<'_>) -> Option<HierarchyNode> {
    let name = item.get("name")?.as_str()?.to_string();
    let uri: lt::Uri = item.get("uri")?.as_str()?.parse().ok()?;
    let path = codecraft_lsp::uri_to_path(&uri)?;
    let range: lt::Range = serde_json::from_value(item.get("selectionRange")?.clone()).ok()?;
    let line = range.start.line as usize;
    let column = utf16_col_to_char(&lines.line(&path, line), range.start.character);
    Some(HierarchyNode {
        name,
        detail: item.get("detail").and_then(Value::as_str).map(str::to_string),
        kind: symbol_kind_label(item.get("kind").and_then(Value::as_u64).unwrap_or(0)).to_string(),
        path,
        line,
        column,
        sites: 0,
        depth,
        children: if depth >= MAX_DEPTH { Some(Vec::new()) } else { None },
        expanded: false,
        loading: false,
        item: Some(item.clone()),
    })
}

/// A leaf for a plain location (implementations fallback): the line's text names it.
pub(crate) fn node_from_location(
    path: &Path,
    range: &lt::Range,
    depth: usize,
    lines: &mut LineCache<'_>,
) -> HierarchyNode {
    let line = range.start.line as usize;
    let text = lines.line(path, line);
    let column = utf16_col_to_char(&text, range.start.character);
    let name = text.trim().trim_end_matches('{').trim().chars().take(200).collect::<String>();
    HierarchyNode {
        name: if name.is_empty() { path.display().to_string() } else { name },
        detail: None,
        kind: "impl".into(),
        path: path.to_path_buf(),
        line,
        column,
        sites: 0,
        depth,
        children: Some(Vec::new()),
        expanded: false,
        loading: false,
        item: None,
    }
}

/// Child items from a children response: `(item, call sites)`.
pub(crate) fn child_items(direction: Direction, value: &Value) -> Vec<(Value, usize)> {
    let Some(list) = value.as_array() else { return Vec::new() };
    list.iter()
        .filter_map(|v| match direction {
            Direction::Callers => {
                Some((v.get("from")?.clone(), v.get("fromRanges").and_then(Value::as_array).map_or(0, Vec::len)))
            }
            Direction::Callees => {
                Some((v.get("to")?.clone(), v.get("fromRanges").and_then(Value::as_array).map_or(0, Vec::len)))
            }
            Direction::Subtypes | Direction::Supertypes => Some((v.clone(), 0)),
        })
        .collect()
}

/// Locations from a definition-like response (Location, Location[], LocationLink[]).
pub(crate) fn locations(value: Value) -> Vec<(PathBuf, lt::Range)> {
    let resp: Option<lt::GotoDefinitionResponse> = serde_json::from_value(value).ok().flatten();
    let list: Vec<lt::Location> = match resp {
        Some(lt::GotoDefinitionResponse::Scalar(l)) => vec![l],
        Some(lt::GotoDefinitionResponse::Array(v)) => v,
        Some(lt::GotoDefinitionResponse::Link(links)) => {
            links.into_iter().map(|l| lt::Location { uri: l.target_uri, range: l.target_selection_range }).collect()
        }
        None => Vec::new(),
    };
    list.into_iter().filter_map(|l| codecraft_lsp::uri_to_path(&l.uri).map(|p| (p, l.range))).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    fn item(name: &str, path: &Path, line: u32, ch: u32) -> Value {
        let uri = codecraft_lsp::path_to_uri(path).unwrap();
        json!({
            "name": name, "kind": 12, "uri": uri.as_str(), "detail": "fn x()",
            "range": { "start": { "line": line, "character": 0 }, "end": { "line": line + 2, "character": 1 } },
            "selectionRange": { "start": { "line": line, "character": ch }, "end": { "line": line, "character": ch + 1 } },
        })
    }

    #[test]
    fn nodes_from_items_convert_utf16_columns() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.rs");
        let read = |_: &Path| Some("x\n/*😀*/ fn f() {}\n".to_string());
        let mut lines = LineCache::new(&read);
        // "/*😀*/ fn f" — `f` is at UTF-16 column 10, char column 9.
        let n = node_from_item(&item("f", &p, 1, 10), 0, &mut lines).unwrap();
        assert_eq!((n.line, n.column), (1, 9));
        assert_eq!(n.kind, "fn");
        assert_eq!(n.detail.as_deref(), Some("fn x()"));
        assert!(n.item.is_some() && n.children.is_none());
        assert!(node_from_item(&json!({ "name": "x" }), 0, &mut lines).is_none());
        assert!(node_from_item(&json!(null), 0, &mut lines).is_none());
    }

    #[test]
    fn children_and_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.rs");
        let incoming = json!([
            { "from": item("caller", &p, 0, 3), "fromRanges": [{ "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 1 } }, { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 1 } }] },
            { "bogus": true },
        ]);
        let kids = child_items(Direction::Callers, &incoming);
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].1, 2);
        assert!(child_items(Direction::Callees, &incoming).is_empty());
        assert!(child_items(Direction::Callers, &json!({})).is_empty());

        let read = |_: &Path| Some("fn caller() {}\n".to_string());
        let mut lines = LineCache::new(&read);
        let mut h = Hierarchy::new(HierarchyKind::Call, "Calls".into(), 1);
        let root = h.push(node_from_item(&item("root", &p, 0, 3), 0, &mut lines).unwrap()).unwrap();
        h.roots.push(root);
        let child = h.push(node_from_item(&kids[0].0, 1, &mut lines).unwrap()).unwrap();
        h.nodes[root].children = Some(vec![child]);
        assert_eq!(h.visible_rows(), vec![(root, 0)]);
        h.nodes[root].expanded = true;
        assert_eq!(h.visible_rows(), vec![(root, 0), (child, 1)]);
        h.reset_children();
        assert_eq!(h.nodes.len(), 1);
        assert_eq!(h.visible_rows(), vec![(0, 0)]);
        assert!(h.nodes[0].children.is_none());
    }

    #[test]
    fn locations_and_leaves() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.rs");
        let uri = codecraft_lsp::path_to_uri(&p).unwrap();
        let r = json!({ "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 4 } });
        let v = json!([{ "targetUri": uri.as_str(), "targetRange": r, "targetSelectionRange": r }]);
        let locs = locations(v);
        assert_eq!(locs.len(), 1);
        let read = |_: &Path| Some("struct A;\nimpl Display for A {\n".to_string());
        let mut lines = LineCache::new(&read);
        let n = node_from_location(&locs[0].0, &locs[0].1, 1, &mut lines);
        assert_eq!(n.name, "impl Display for A");
        assert_eq!(n.children, Some(Vec::new()));
        assert!(locations(json!("garbage")).is_empty());
        assert_eq!(Direction::parse("Incoming"), Some(Direction::Callers));
        assert_eq!(Direction::parse("sideways"), None);
    }

    #[test]
    fn node_cap() {
        let mut h = Hierarchy::new(HierarchyKind::Type, "T".into(), 1);
        let leaf = HierarchyNode {
            name: "x".into(),
            detail: None,
            kind: "impl".into(),
            path: PathBuf::from("/x"),
            line: 0,
            column: 0,
            sites: 0,
            depth: 0,
            children: Some(Vec::new()),
            expanded: false,
            loading: false,
            item: None,
        };
        for _ in 0..MAX_NODES {
            assert!(h.push(leaf.clone()).is_some());
        }
        assert!(h.push(leaf).is_none());
        assert!(h.message.is_some());
    }
}
