//! Views of a profile: the top-down call tree, the flame graph layout and the method list.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::model::Profile;

/// One node of the call tree: a frame reached through a particular path from a root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallNode {
    pub frame: u32,
    pub parent: Option<u32>,
    /// Sorted by total weight, heaviest first (then by name, for a stable order).
    pub children: Vec<u32>,
    /// Weight of the samples whose stack passes through this node.
    pub total: u64,
    /// Weight of the samples whose stack ends here.
    pub self_weight: u64,
    pub depth: u32,
}

/// The top-down call tree of a profile (all threads or one), merged across identical paths.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallTree {
    /// Parents come before their children.
    pub nodes: Vec<CallNode>,
    /// Sorted like [`CallNode::children`].
    pub roots: Vec<u32>,
    pub total: u64,
}

impl CallTree {
    /// The call tree of one thread (`Some(index)`) or of all threads merged (`None`).
    pub fn build(p: &Profile, thread: Option<usize>) -> Self {
        let n = p.stacks.len();
        let mut weight = vec![0u64; n];
        let threads: Vec<&crate::model::Thread> = match thread {
            Some(i) => p.threads.get(i).into_iter().collect(),
            None => p.threads.iter().collect(),
        };
        for t in threads {
            for (stack, w) in &t.weights {
                if let Some(slot) = weight.get_mut(*stack as usize) {
                    *slot = slot.saturating_add(*w);
                }
            }
        }
        let self_weight = weight.clone();
        // Children have larger indices than their parents: one reverse pass sums the totals.
        for i in (0..n).rev() {
            let Some(parent) = p.stacks.get(i).and_then(|s| s.parent) else { continue };
            let Some(w) = weight.get(i).copied() else { continue };
            if let Some(slot) = weight.get_mut(parent as usize) {
                *slot = slot.saturating_add(w);
            }
        }
        let mut node_of: Vec<Option<u32>> = vec![None; n];
        let mut tree = CallTree::default();
        for (i, s) in p.stacks.iter().enumerate() {
            let total = weight.get(i).copied().unwrap_or(0);
            if total == 0 {
                continue;
            }
            let parent = s.parent.and_then(|pi| node_of.get(pi as usize).copied().flatten());
            let depth = parent.and_then(|pi| tree.nodes.get(pi as usize)).map_or(0, |pn| pn.depth.saturating_add(1));
            let Ok(id) = u32::try_from(tree.nodes.len()) else { break };
            tree.nodes.push(CallNode {
                frame: s.frame,
                parent,
                children: Vec::new(),
                total,
                self_weight: self_weight.get(i).copied().unwrap_or(0),
                depth,
            });
            if let Some(slot) = node_of.get_mut(i) {
                *slot = Some(id);
            }
            match parent.and_then(|pi| tree.nodes.get_mut(pi as usize)) {
                Some(pn) => pn.children.push(id),
                None => {
                    tree.roots.push(id);
                    tree.total = tree.total.saturating_add(total);
                }
            }
        }
        let names: Vec<&str> = p.frames.iter().map(|f| f.name.as_str()).collect();
        let order = |nodes: &[CallNode], a: &u32, b: &u32| -> Ordering {
            let (Some(x), Some(y)) = (nodes.get(*a as usize), nodes.get(*b as usize)) else { return a.cmp(b) };
            y.total
                .cmp(&x.total)
                .then_with(|| names.get(x.frame as usize).cmp(&names.get(y.frame as usize)))
                .then_with(|| a.cmp(b))
        };
        for i in 0..tree.nodes.len() {
            let Some(node) = tree.nodes.get_mut(i) else { continue };
            let mut children = std::mem::take(&mut node.children);
            children.sort_by(|a, b| order(&tree.nodes, a, b));
            if let Some(node) = tree.nodes.get_mut(i) {
                node.children = children;
            }
        }
        let mut roots = std::mem::take(&mut tree.roots);
        roots.sort_by(|a, b| order(&tree.nodes, a, b));
        tree.roots = roots;
        tree
    }

    pub fn node(&self, id: u32) -> Option<&CallNode> {
        self.nodes.get(id as usize)
    }

    /// The nodes from a root down to `id` (inclusive).
    pub fn path(&self, id: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = Some(id);
        while let Some(i) = cur {
            let Some(n) = self.nodes.get(i as usize) else { break };
            out.push(i);
            cur = n.parent.filter(|p| *p < i);
        }
        out.reverse();
        out
    }

    /// Nodes in depth-first order, children in their sorted order (for "expand all", dumps).
    pub fn preorder(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<u32> = self.roots.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            out.push(id);
            if let Some(n) = self.nodes.get(id as usize) {
                stack.extend(n.children.iter().rev().copied());
            }
        }
        out
    }

    /// Share of the tree total, in percent.
    pub fn percent(&self, weight: u64) -> f64 {
        if self.total == 0 { 0.0 } else { weight as f64 * 100.0 / self.total as f64 }
    }
}

/// One rectangle of the flame graph: a call node at its depth, spanning `[start, start + width)`
/// in weight units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlameRect {
    pub node: u32,
    pub start: u64,
    pub width: u64,
}

/// The flame graph layout: one row per depth, rectangles sorted by `start` in each row. Siblings
/// are ordered alphabetically (like flamegraph.pl) so the picture is stable between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlameGraph {
    pub rows: Vec<Vec<FlameRect>>,
    pub total: u64,
    /// `(depth, start)` per call node.
    pub position: Vec<(u32, u64)>,
}

impl FlameGraph {
    pub fn build(p: &Profile, tree: &CallTree) -> Self {
        let name = |id: u32| tree.node(id).and_then(|n| p.frame(n.frame)).map_or("", |f| f.name.as_str());
        let sorted = |ids: &[u32]| {
            let mut v = ids.to_vec();
            v.sort_by(|a, b| name(*a).cmp(name(*b)).then_with(|| a.cmp(b)));
            v
        };
        let mut g = FlameGraph { total: tree.total, position: vec![(0, 0); tree.nodes.len()], ..Self::default() };
        // Iterative depth-first walk: (node, start).
        let mut work: Vec<(u32, u64)> = Vec::new();
        let mut x = 0u64;
        for r in sorted(&tree.roots) {
            work.push((r, x));
            x = x.saturating_add(tree.node(r).map_or(0, |n| n.total));
        }
        work.reverse();
        while let Some((id, start)) = work.pop() {
            let Some(n) = tree.node(id) else { continue };
            let depth = n.depth as usize;
            while g.rows.len() <= depth {
                g.rows.push(Vec::new());
            }
            if let Some(row) = g.rows.get_mut(depth) {
                row.push(FlameRect { node: id, start, width: n.total });
            }
            if let Some(slot) = g.position.get_mut(id as usize) {
                *slot = (n.depth, start);
            }
            let mut cx = start;
            let mut kids = Vec::new();
            for c in sorted(&n.children) {
                kids.push((c, cx));
                cx = cx.saturating_add(tree.node(c).map_or(0, |cn| cn.total));
            }
            work.extend(kids.into_iter().rev());
        }
        for row in &mut g.rows {
            row.sort_by_key(|r| (r.start, r.node));
        }
        g
    }

    /// The rectangles of a row that overlap `[from, to)`.
    pub fn visible(&self, depth: usize, from: u64, to: u64) -> &[FlameRect] {
        let Some(row) = self.rows.get(depth) else { return &[] };
        // Rects in a row don't overlap, so ends are sorted too.
        let first = row.partition_point(|r| r.start.saturating_add(r.width) <= from);
        let last = row.partition_point(|r| r.start < to);
        row.get(first..last.max(first)).unwrap_or(&[])
    }

    pub fn depth(&self) -> usize {
        self.rows.len()
    }

    /// `(start, width)` of a node.
    pub fn span(&self, tree: &CallTree, node: u32) -> Option<(u64, u64)> {
        let (_, start) = self.position.get(node as usize)?;
        Some((*start, tree.node(node)?.total))
    }
}

/// One function of the method list: its self time and its total time (samples where it is on
/// the stack at least once, so recursion is not counted twice).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodStat {
    pub frame: u32,
    pub self_weight: u64,
    pub total: u64,
}

/// The bottom-up method list, heaviest self weight first.
pub fn methods(p: &Profile, tree: &CallTree) -> Vec<MethodStat> {
    let mut by_frame: HashMap<u32, MethodStat> = HashMap::new();
    for_outermost(tree, |id, outermost| {
        let Some(n) = tree.node(id) else { return };
        let e = by_frame.entry(n.frame).or_insert(MethodStat { frame: n.frame, self_weight: 0, total: 0 });
        e.self_weight = e.self_weight.saturating_add(n.self_weight);
        if outermost {
            e.total = e.total.saturating_add(n.total);
        }
    });
    let mut v: Vec<MethodStat> = by_frame.into_values().collect();
    let name = |f: u32| p.frame(f).map_or("", |f| f.name.as_str());
    v.sort_by(|a, b| {
        b.self_weight.cmp(&a.self_weight).then(b.total.cmp(&a.total)).then_with(|| name(a.frame).cmp(name(b.frame)))
    });
    v
}

/// Walk the tree depth first, telling for each node whether it is the outermost occurrence of
/// its frame on its path (no ancestor has the same frame).
fn for_outermost(tree: &CallTree, mut f: impl FnMut(u32, bool)) {
    let mut on_path: HashMap<u32, u32> = HashMap::new();
    // (node, entering)
    let mut work: Vec<(u32, bool)> = tree.roots.iter().rev().map(|r| (*r, true)).collect();
    while let Some((id, entering)) = work.pop() {
        let Some(n) = tree.node(id) else { continue };
        if entering {
            let count = on_path.entry(n.frame).or_insert(0);
            f(id, *count == 0);
            *count = count.saturating_add(1);
            work.push((id, false));
            work.extend(n.children.iter().rev().map(|c| (*c, true)));
        } else if let Some(count) = on_path.get_mut(&n.frame) {
            *count = count.saturating_sub(1);
        }
    }
}

/// Who calls a function, or what it calls, with the weight through each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// `None` for "the root" (callers of a root frame).
    pub frame: Option<u32>,
    pub weight: u64,
}

/// The callers of `frame` (by its outermost occurrences), heaviest first.
pub fn callers(tree: &CallTree, frame: u32) -> Vec<Edge> {
    let mut acc: HashMap<Option<u32>, u64> = HashMap::new();
    for_outermost(tree, |id, outermost| {
        let Some(n) = tree.node(id) else { return };
        if outermost && n.frame == frame {
            let caller = n.parent.and_then(|p| tree.node(p)).map(|pn| pn.frame);
            let e = acc.entry(caller).or_insert(0);
            *e = e.saturating_add(n.total);
        }
    });
    sorted_edges(acc)
}

/// The callees of `frame` (by its outermost occurrences), heaviest first.
pub fn callees(tree: &CallTree, frame: u32) -> Vec<Edge> {
    let mut acc: HashMap<Option<u32>, u64> = HashMap::new();
    for_outermost(tree, |id, outermost| {
        let Some(n) = tree.node(id) else { return };
        if outermost && n.frame == frame {
            for c in &n.children {
                if let Some(cn) = tree.node(*c) {
                    let e = acc.entry(Some(cn.frame)).or_insert(0);
                    *e = e.saturating_add(cn.total);
                }
            }
        }
    });
    sorted_edges(acc)
}

fn sorted_edges(acc: HashMap<Option<u32>, u64>) -> Vec<Edge> {
    let mut v: Vec<Edge> = acc.into_iter().map(|(frame, weight)| Edge { frame, weight }).collect();
    v.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.frame.cmp(&b.frame)));
    v
}

/// Frames whose name contains `query` (case-insensitive), as a per-frame flag; empty for an
/// empty query.
pub fn search(p: &Profile, query: &str) -> Vec<bool> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    p.frames
        .iter()
        .map(|f| f.name.to_lowercase().contains(&q) || f.module.as_ref().is_some_and(|m| m.to_lowercase().contains(&q)))
        .collect()
}

/// The weight of the samples that pass through at least one matching frame.
pub fn matched_weight(tree: &CallTree, hits: &[bool]) -> u64 {
    let mut total = 0u64;
    let mut work: Vec<u32> = tree.roots.clone();
    while let Some(id) = work.pop() {
        let Some(n) = tree.node(id) else { continue };
        if hits.get(n.frame as usize).copied().unwrap_or(false) {
            total = total.saturating_add(n.total);
        } else {
            work.extend(n.children.iter().copied());
        }
    }
    total
}

/// Everything the Profiler tool window shows for one thread selection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Analysis {
    pub thread: Option<usize>,
    pub tree: CallTree,
    pub flame: FlameGraph,
    pub methods: Vec<MethodStat>,
}

impl Analysis {
    pub fn new(p: &Profile, thread: Option<usize>) -> Self {
        let tree = CallTree::build(p, thread);
        let flame = FlameGraph::build(p, &tree);
        let methods = methods(p, &tree);
        Self { thread, tree, flame, methods }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collapsed;

    fn profile(text: &str) -> Profile {
        collapsed::parse(text).unwrap()
    }

    fn name(p: &Profile, t: &CallTree, id: u32) -> String {
        p.frame(t.node(id).unwrap().frame).unwrap().name.clone()
    }

    #[test]
    fn call_tree_totals_and_self() {
        let p = profile("main;a;b 3\nmain;a 2\nmain;c 5\nmain;a;b 1\n");
        let t = CallTree::build(&p, None);
        assert_eq!(t.total, 11);
        assert_eq!(t.roots.len(), 1);
        let main = t.node(t.roots[0]).unwrap();
        assert_eq!((main.total, main.self_weight), (11, 0));
        // Children sorted by total: a (6) before c (5).
        let kids: Vec<String> = main.children.iter().map(|c| name(&p, &t, *c)).collect();
        assert_eq!(kids, vec!["a", "c"]);
        let a = t.node(main.children[0]).unwrap();
        assert_eq!((a.total, a.self_weight), (6, 2));
        let b = t.node(a.children[0]).unwrap();
        assert_eq!((b.total, b.self_weight, b.depth), (4, 4, 2));
        assert_eq!(t.path(a.children[0]).len(), 3);
        assert_eq!(t.preorder().len(), t.nodes.len());
    }

    #[test]
    fn flame_graph_layout_is_alphabetical_and_nested() {
        let p = profile("main;z 1\nmain;a 3\nmain;a;x 2\nother 4\n");
        let t = CallTree::build(&p, None);
        let g = FlameGraph::build(&p, &t);
        assert_eq!(g.total, 10);
        assert_eq!(g.depth(), 3);
        let row0: Vec<(String, u64, u64)> =
            g.rows[0].iter().map(|r| (name(&p, &t, r.node), r.start, r.width)).collect();
        assert_eq!(row0, vec![("main".into(), 0, 6), ("other".into(), 6, 4)]);
        let row1: Vec<(String, u64, u64)> =
            g.rows[1].iter().map(|r| (name(&p, &t, r.node), r.start, r.width)).collect();
        assert_eq!(row1, vec![("a".into(), 0, 5), ("z".into(), 5, 1)]);
        assert_eq!(g.visible(0, 7, 8).len(), 1);
        assert_eq!(g.visible(0, 0, 10).len(), 2);
        assert_eq!(g.visible(1, 6, 10).len(), 0);
        assert_eq!(g.visible(9, 0, 10).len(), 0);
        assert_eq!(g.visible(0, 8, 2).len(), 0);
        let x = g.rows[2][0].node;
        assert_eq!(g.span(&t, x), Some((0, 2)));
    }

    #[test]
    fn methods_count_recursion_once() {
        let p = profile("main;f;f;g 4\nmain;g 1\n");
        let t = CallTree::build(&p, None);
        let m = methods(&p, &t);
        let get = |n: &str| *m.iter().find(|s| p.frame(s.frame).unwrap().name == n).unwrap();
        assert_eq!((get("f").self_weight, get("f").total), (0, 4));
        assert_eq!((get("g").self_weight, get("g").total), (5, 5));
        assert_eq!(get("main").total, 5);
        // Heaviest self first.
        assert_eq!(p.frame(m[0].frame).unwrap().name, "g");
        let g = get("g").frame;
        let callers = callers(&t, g);
        assert_eq!(callers.len(), 2);
        assert_eq!(callers[0].weight, 4);
        let f = get("f").frame;
        let callees = callees(&t, f);
        assert_eq!(callees.iter().map(|e| e.weight).sum::<u64>(), 4);
        let main = get("main").frame;
        assert_eq!(super::callers(&t, main), vec![Edge { frame: None, weight: 5 }]);
    }

    /// 200k samples over ~50k distinct stacks: parse, analyse and query stay fast and consistent.
    #[test]
    fn large_profiles() {
        let mut text = String::new();
        for i in 0..200_000u32 {
            let (a, b, c) = (i % 13, (i / 13) % 61, (i / 7) % 64);
            text.push_str(&format!("main;mod{a}::run;work{b};leaf{c} 1\n"));
        }
        let start = std::time::Instant::now();
        let p = profile(&text);
        let a = Analysis::new(&p, None);
        assert!(start.elapsed().as_secs() < 30);
        assert_eq!(a.tree.total, 200_000);
        assert!(a.tree.nodes.len() > 40_000, "{}", a.tree.nodes.len());
        let leaves: u64 = a.flame.rows.last().unwrap().iter().map(|r| r.width).sum();
        assert_eq!(leaves, 200_000);
        let self_sum: u64 = a.methods.iter().map(|m| m.self_weight).sum();
        assert_eq!(self_sum, 200_000);
        // A narrow window of the widest row returns only what overlaps it.
        let row = &a.flame.rows[3];
        let mid = row[row.len() / 2];
        assert_eq!(a.flame.visible(3, mid.start, mid.start + 1), &[mid]);
    }

    #[test]
    fn thread_filter_and_search() {
        let p = crate::perf::parse(
            "app 10/11 1.0: 1 cycles:\n\t1 work+0x1 (/bin/app)\n\t2 main+0x2 (/bin/app)\n\n\
             app 10/12 1.1: 1 cycles:\n\t3 idle+0x1 (/bin/app)\n\n",
        )
        .unwrap();
        assert_eq!(p.threads.len(), 2);
        let all = Analysis::new(&p, None);
        assert_eq!(all.tree.total, 2);
        let one = Analysis::new(&p, Some(1));
        assert_eq!(one.tree.total, 1);
        assert_eq!(name(&p, &one.tree, one.tree.roots[0]), "idle");
        let hits = search(&p, "WORK");
        assert_eq!(matched_weight(&all.tree, &hits), 1);
        assert!(search(&p, " ").is_empty());
        let none = Analysis::new(&p, Some(99));
        assert_eq!(none.tree.total, 0);
        assert!(none.flame.rows.is_empty());
    }
}
