//! Compact, self-explanatory JSON about profiles for agents (M14 track PF): the hot functions
//! (self and total share, file:line), the hottest source lines with the code on them, a focus
//! on some functions, a comparison of two snapshots, per-line weights of a file and the thread
//! timeline.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_profile::{Analysis, FunctionDelta, HotspotOrder, MethodStat, Profile, Unit};

/// Source files read for line excerpts at most (per report).
const MAX_EXCERPT_FILES: usize = 12;
/// Largest source file read for an excerpt.
const MAX_SOURCE_BYTES: u64 = 4 << 20;

/// What to put in a report.
#[derive(Clone, Debug)]
pub struct ReportOptions {
    pub top: usize,
    pub lines: usize,
    pub focus: Option<String>,
    /// Resolve relative source paths (and read excerpts) against this directory.
    pub root: Option<PathBuf>,
    /// A Cargo debug build was profiled.
    pub debug_build: bool,
    /// The order of `top` (fixed, whatever the Method List shows).
    pub sort: HotspotOrder,
}

impl Default for ReportOptions {
    fn default() -> Self {
        Self { top: 15, lines: 10, focus: None, root: None, debug_build: false, sort: HotspotOrder::SelfWeight }
    }
}

/// How hotspot lists are ordered, for the reader.
pub fn order_text(sort: HotspotOrder) -> &'static str {
    match sort {
        HotspotOrder::SelfWeight => "selfPercent desc, then totalPercent desc, then name, location, module",
        HotspotOrder::Total => "totalPercent desc, then selfPercent desc, then name, location, module",
    }
}

/// A percentage with one decimal.
pub fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 * 1000.0 / whole as f64).round() / 10.0
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

fn unit_name(u: Unit) -> &'static str {
    match u {
        Unit::Samples => "samples",
        Unit::Nanoseconds => "nanoseconds",
        Unit::Bytes => "bytes",
    }
}

fn function_json(p: &Profile, m: &MethodStat, total: u64) -> Value {
    let mut v = serde_json::Map::new();
    if let Some(f) = p.frame(m.frame) {
        v.insert("name".into(), json!(f.name));
        if let Some(module) = &f.module {
            v.insert("module".into(), json!(module));
        }
        if let Some(loc) = f.location() {
            v.insert("location".into(), json!(loc));
        }
    }
    v.insert("selfPercent".into(), json!(pct(m.self_weight, total)));
    v.insert("totalPercent".into(), json!(pct(m.total, total)));
    v.insert("self".into(), json!(m.self_weight));
    v.insert("total".into(), json!(m.total));
    if let Some(c) = p.calls_of(m.frame) {
        v.insert("calls".into(), json!(c));
    }
    Value::Object(v)
}

/// Reads source lines for excerpts, a few files at most.
struct Sources {
    root: Option<PathBuf>,
    files: HashMap<String, Option<Vec<String>>>,
}

impl Sources {
    fn new(root: Option<PathBuf>) -> Self {
        Self { root, files: HashMap::new() }
    }

    fn line(&mut self, file: &str, line: u32) -> Option<String> {
        if !self.files.contains_key(file) {
            if self.files.len() >= MAX_EXCERPT_FILES {
                return None;
            }
            let mut path = PathBuf::from(file);
            if path.is_relative()
                && let Some(r) = &self.root
            {
                path = r.join(path);
            }
            let text = std::fs::metadata(&path)
                .ok()
                .filter(|m| m.is_file() && m.len() <= MAX_SOURCE_BYTES)
                .and_then(|_| std::fs::read(&path).ok())
                .map(|b| String::from_utf8_lossy(&b).lines().map(str::to_string).collect());
            self.files.insert(file.to_string(), text);
        }
        let lines = self.files.get(file)?.as_ref()?;
        let text = lines.get((line as usize).checked_sub(1)?)?.trim();
        Some(if text.chars().count() > 160 {
            format!("{}…", text.chars().take(160).collect::<String>())
        } else {
            text.to_string()
        })
    }
}

/// The report of one profile.
pub fn report(p: &Profile, a: &Analysis, path: &Path, opts: &ReportOptions) -> Value {
    let total = a.tree.total;
    let top = opts.top.clamp(1, 200);
    let mut out = serde_json::Map::new();
    out.insert("snapshot".into(), json!(path));
    out.insert("format".into(), json!(p.format));
    out.insert("unit".into(), json!(unit_name(p.unit)));
    out.insert("total".into(), json!(total));
    out.insert("totalText".into(), json!(p.unit.format(total)));
    out.insert("samples".into(), json!(p.sample_count()));
    if let Some(d) = p.duration_ns {
        out.insert("durationMs".into(), json!(d / 1_000_000));
    }
    // Thread shares within the selected time range (of all threads' weight there).
    let totals = p.thread_totals(a.range);
    let all: u64 = totals.iter().fold(0u64, |x, y| x.saturating_add(*y));
    let mut threads: Vec<(usize, &str, u64)> =
        p.threads.iter().zip(&totals).enumerate().map(|(i, (t, w))| (i, t.name.as_str(), *w)).collect();
    threads.sort_by(|x, y| y.2.cmp(&x.2).then(x.0.cmp(&y.0)));
    out.insert(
        "threads".into(),
        json!(
            threads
                .iter()
                .take(8)
                .map(|(i, n, w)| json!({ "index": i, "name": n, "percent": pct(*w, all) }))
                .collect::<Vec<_>>()
        ),
    );
    if p.threads.len() > 8 {
        out.insert("moreThreads".into(), json!(p.threads.len() - 8));
    }
    if a.thread.is_some() || a.range.is_some() {
        out.insert(
            "selection".into(),
            json!({
                "thread": a.thread.and_then(|t| p.threads.get(t)).map(|t| t.name.clone()),
                "threadIndex": a.thread,
                "range": a.range.map(|r| json!({
                    "startMs": crate::profiler_cmds::ns_ms(r.0),
                    "endMs": crate::profiler_cmds::ns_ms(r.1),
                })),
                "samples": a.samples,
            }),
        );
    }
    // A fixed order, independent of the Method List's sort in the IDE (QA M14 #25).
    let ordered = codecraft_profile::hotspots(p, &a.methods, opts.sort);
    out.insert("sort".into(), json!(opts.sort.name()));
    out.insert("order".into(), json!(order_text(opts.sort)));
    out.insert("top".into(), json!(ordered.iter().take(top).map(|m| function_json(p, m, total)).collect::<Vec<_>>()));
    let by_total = codecraft_profile::hotspots(p, &a.methods, HotspotOrder::Total);
    let top_total: Vec<Value> = by_total
        .iter()
        .take(top)
        .map(|m| {
            let f = p.frame(m.frame);
            json!({
                "name": f.map(|f| f.name.clone()),
                "location": f.and_then(codecraft_profile::Frame::location),
                "totalPercent": pct(m.total, total),
            })
        })
        .collect();
    out.insert("topByTotal".into(), json!(top_total));
    let mut sources = Sources::new(opts.root.clone());
    out.insert("hotLines".into(), json!(hot_lines(p, a, total, opts.lines, &mut sources, None)));
    if let Some(q) = opts.focus.as_deref().filter(|q| !q.trim().is_empty()) {
        out.insert("focus".into(), focus(p, a, q, top, &mut sources));
    }
    if let Some(m) = memory(p, path, 50, top.min(20)) {
        out.insert("memory".into(), m);
    }
    let mut notes: Vec<String> = p.notes.clone();
    let bare = p.unsymbolicated_weight();
    if total > 0 && bare.saturating_mul(10) > total && !notes.iter().any(|n| n.contains("without symbols")) {
        notes.push(format!(
            "{:.0}% of the weight is in frames without symbols (0x… addresses): build with debug info",
            bare as f64 * 100.0 / total as f64
        ));
    }
    if p.lines.is_empty() {
        notes.push(format!("{} has no per-line samples; hotLines is empty", p.format.label()));
    }
    if a.range.is_some() {
        notes.push("Restricted to the samples of the selected time range: percentages are of that range".into());
    }
    if opts.debug_build {
        notes.push("A Cargo debug build was profiled: timings differ from release builds (profile with --release and debug = true in [profile.release])".into());
    }
    if p.unit == Unit::Bytes {
        notes.push("Weights are bytes: self = allocated by the function itself, total = by it and its callees".into());
    }
    out.insert("notes".into(), json!(notes));
    Value::Object(out)
}

/// The heaviest source lines of the selection by self weight (`only`: frames to restrict to).
fn hot_lines(
    p: &Profile,
    a: &Analysis,
    total: u64,
    n: usize,
    sources: &mut Sources,
    only: Option<&[bool]>,
) -> Vec<Value> {
    let mut lines: Vec<&codecraft_profile::LineStat> = a
        .lines(p)
        .iter()
        .filter(|l| l.self_weight > 0)
        .filter(|l| only.is_none_or(|o| o.get(l.frame as usize).copied().unwrap_or(false)))
        .collect();
    lines.sort_by(|x, y| {
        y.self_weight.cmp(&x.self_weight).then(y.total.cmp(&x.total)).then((x.frame, x.line).cmp(&(y.frame, y.line)))
    });
    lines
        .iter()
        .take(n.min(100))
        .filter_map(|l| {
            let f = p.frame(l.frame)?;
            let file = f.file.as_deref()?;
            let mut v = json!({
                "location": format!("{file}:{}", l.line),
                "function": f.name,
                "selfPercent": pct(l.self_weight, total),
                "totalPercent": pct(l.total, total),
            });
            if let Some(code) = sources.line(file, l.line)
                && let Some(o) = v.as_object_mut()
            {
                o.insert("code".into(), json!(code));
            }
            Some(v)
        })
        .collect()
}

/// The functions matching `query` and what happens inside them.
fn focus(p: &Profile, a: &Analysis, query: &str, top: usize, sources: &mut Sources) -> Value {
    let total = a.tree.total;
    let hits = codecraft_profile::search(p, query);
    let roots = codecraft_profile::outermost_matches(&a.tree, &hits);
    let matched: u64 = roots.iter().filter_map(|r| a.tree.node(*r)).map(|n| n.total).sum();
    let mut names: Vec<String> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        if *h
            && let Some(f) = p.frames.get(i)
            && !names.contains(&f.name)
            && names.len() < 10
        {
            names.push(f.name.clone());
        }
    }
    if roots.is_empty() {
        return json!({ "query": query, "matched": names, "percent": 0.0, "note": "no samples in functions matching the query" });
    }
    let inside = codecraft_profile::subtree_methods(p, &a.tree, &roots);
    let inside_json: Vec<Value> = inside
        .iter()
        .take(top)
        .map(|m| {
            let mut v = function_json(p, m, total);
            if let Some(o) = v.as_object_mut() {
                o.insert("focusSelfPercent".into(), json!(pct(m.self_weight, matched)));
            }
            v
        })
        .collect();
    // Callers and callees of the heaviest matching function.
    let main = roots.first().and_then(|r| a.tree.node(*r)).map(|n| n.frame);
    let edge_json = |e: &codecraft_profile::Edge| match e.frame.and_then(|f| p.frame(f)) {
        Some(f) => json!({ "name": f.name, "location": f.location(), "percent": pct(e.weight, total) }),
        None => json!({ "name": "[root]", "percent": pct(e.weight, total) }),
    };
    let (callers, callees) = match main {
        Some(f) => (
            codecraft_profile::callers(&a.tree, f).iter().take(10).map(edge_json).collect::<Vec<_>>(),
            codecraft_profile::callees(&a.tree, f).iter().take(10).map(edge_json).collect::<Vec<_>>(),
        ),
        None => (Vec::new(), Vec::new()),
    };
    let lines = hot_lines(p, a, total, 10, sources, Some(&inside_frames(p, &inside)));
    json!({
        "query": query,
        "matched": names,
        "percent": pct(matched, total),
        "function": main.and_then(|f| p.frame(f)).map(|f| f.name.clone()),
        "inside": inside_json,
        "callers": callers,
        "callees": callees,
        "hotLines": lines,
    })
}

fn inside_frames(p: &Profile, inside: &[MethodStat]) -> Vec<bool> {
    let mut v = vec![false; p.frames.len()];
    for m in inside {
        if let Some(slot) = v.get_mut(m.frame as usize) {
            *slot = true;
        }
    }
    v
}

/// What changed from `a` (baseline) to `b`.
pub fn compare(a: (&Path, &Profile, &Analysis), b: (&Path, &Profile, &Analysis), top: usize) -> Value {
    let (ap, apr, aa) = a;
    let (bp, bpr, ba) = b;
    let (at, bt) = (aa.tree.total, ba.tree.total);
    let deltas = codecraft_profile::compare(apr, &aa.methods, bpr, &ba.methods);
    let top = top.clamp(1, 100);
    let row = |d: &FunctionDelta| {
        json!({
            "name": d.name,
            "location": d.location,
            "aSelf": d.a_self, "bSelf": d.b_self,
            "aSelfPercent": pct(d.a_self, at), "bSelfPercent": pct(d.b_self, bt),
            "deltaSelfPercent": round1(pct(d.b_self, bt) - pct(d.a_self, at)),
            "aTotalPercent": pct(d.a_total, at), "bTotalPercent": pct(d.b_total, bt),
        })
    };
    let delta = |d: &FunctionDelta| i128::from(d.b_self) - i128::from(d.a_self);
    let mut slower: Vec<&FunctionDelta> = deltas.iter().filter(|d| delta(d) > 0).collect();
    slower.sort_by_key(|x| std::cmp::Reverse(delta(x)));
    let mut faster: Vec<&FunctionDelta> = deltas.iter().filter(|d| delta(d) < 0).collect();
    faster.sort_by_key(|x| delta(x));
    let only = |f: &dyn Fn(&FunctionDelta) -> bool| -> Vec<String> {
        let mut v: Vec<&FunctionDelta> = deltas.iter().filter(|d| f(d)).collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.a_total.max(x.b_total)));
        v.iter().take(top).map(|d| d.name.clone()).collect()
    };
    let unit = bpr.unit;
    let change = if at == 0 { None } else { Some(round1((bt as f64 - at as f64) * 100.0 / at as f64)) };
    let mut notes = Vec::new();
    if apr.unit != bpr.unit {
        notes.push(format!(
            "The snapshots measure different things ({} vs {}); compare percentages only",
            unit_name(apr.unit),
            unit_name(bpr.unit)
        ));
    }
    notes.push(
        "slower/faster are sorted by the change in self weight (b - a); percentages are of each snapshot's own total"
            .to_string(),
    );
    json!({
        "a": { "snapshot": ap, "total": at, "totalText": apr.unit.format(at) },
        "b": { "snapshot": bp, "total": bt, "totalText": unit.format(bt) },
        "unit": unit_name(unit),
        "totalChangePercent": change,
        "slower": slower.iter().take(top).map(|d| row(d)).collect::<Vec<_>>(),
        "faster": faster.iter().take(top).map(|d| row(d)).collect::<Vec<_>>(),
        "added": only(&|d| d.a_total == 0 && d.b_total > 0),
        "removed": only(&|d| d.b_total == 0 && d.a_total > 0),
        "notes": notes,
    })
}

/// Per-line weights of one file.
pub fn line_hits(p: &Profile, total: u64, file: &Path, hits: &[crate::profiler::LineHit]) -> Value {
    let lines: Vec<Value> = hits
        .iter()
        .map(|h| {
            json!({
                "line": h.line, "self": h.self_weight, "total": h.total,
                "selfPercent": pct(h.self_weight, total), "totalPercent": pct(h.total, total),
            })
        })
        .collect();
    json!({ "file": file, "unit": unit_name(p.unit), "lines": lines })
}

/// Thread activity over time (`buckets` per thread, each the weight in that time slice).
pub fn timeline(p: &Profile, buckets: usize) -> Value {
    let buckets = buckets.clamp(1, codecraft_profile::TIMELINE_BUCKETS);
    let threads: Vec<Value> = p
        .threads
        .iter()
        .map(|t| {
            let mut v = vec![0u64; if t.activity.is_empty() { 0 } else { buckets }];
            let n = t.activity.len().max(1);
            for (i, w) in t.activity.iter().enumerate() {
                if let Some(slot) = v.get_mut(i * buckets / n) {
                    *slot = slot.saturating_add(*w);
                }
            }
            json!({ "name": t.name, "total": t.total(), "activity": v })
        })
        .collect();
    json!({
        "durationMs": p.duration_ns.map(|d| d / 1_000_000),
        "unit": unit_name(p.unit),
        "buckets": buckets,
        "threads": threads,
        "note": if p.duration_ns.is_none() { "this format has no sample times" } else { "activity[i] = weight sampled in the i-th time slice" },
    })
}

/// Memory over time and the top allocation sites at the peak (tracemalloc with periodic
/// snapshots); `None` when the profile has no memory timeline.
pub fn memory(p: &Profile, path: &Path, points: usize, top: usize) -> Option<Value> {
    let m = p.memory.as_ref()?;
    let n = points.clamp(2, 1000);
    // Downsampled to `n` points, each the highest of the ones it covers (peaks survive).
    let step = m.points.len().div_ceil(n).max(1);
    let timeline: Vec<Value> = m
        .points
        .chunks(step)
        .filter_map(|c| {
            let last = c.last()?;
            let peak = c.iter().map(|x| x.peak).max().unwrap_or(last.peak);
            Some(json!({ "ms": last.time_ms, "current": last.current, "peak": peak }))
        })
        .collect();
    let at_peak: Vec<Value> = m
        .at_peak
        .iter()
        .take(top.clamp(1, 200))
        .map(|a| {
            let mut v = json!({
                "name": a.name,
                "size": a.size,
                "sizeText": Unit::Bytes.format(a.size),
                "count": a.count,
                "percentOfPeak": pct(a.size, m.peak_snapshot_bytes.unwrap_or(m.peak)),
            });
            if let (Some(o), Some(file)) = (v.as_object_mut(), &a.file) {
                let loc = match a.line {
                    Some(l) => format!("{file}:{l}"),
                    None => file.clone(),
                };
                o.insert("location".into(), json!(loc));
                if !a.callers.is_empty() {
                    o.insert("callers".into(), json!(a.callers));
                }
            }
            v
        })
        .collect();
    Some(json!({
        "peak": m.peak,
        "peakText": Unit::Bytes.format(m.peak),
        "peakMs": m.peak_ms,
        "intervalMs": m.interval_ms,
        "points": m.points.len(),
        "timeline": timeline,
        "peakSnapshotMs": m.peak_snapshot_ms,
        "peakSnapshotBytes": m.peak_snapshot_bytes,
        "atPeak": at_peak,
        "peakSnapshot": crate::profiler::peak_snapshot_path(path, p),
        "note": "timeline[i] = live traced memory (current) and the highest value since the previous point (peak); \
                 atPeak = allocation sites live in the snapshot nearest the peak (taken when memory reached a new high)",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> (Profile, Analysis) {
        let p = codecraft_profile::parse(text.as_bytes()).unwrap();
        let a = Analysis::new(&p, None);
        (p, a)
    }

    #[test]
    fn report_with_lines_excerpts_and_focus() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("app.py"),
            "import x\n\ndef work():\n    total = sum(range(10))\n    return total\n",
        )
        .unwrap();
        let (p, a) =
            load("main (app.py:1);work (app.py:4) 7\nmain (app.py:1);work (app.py:5) 1\nmain (app.py:1);idle 2\n");
        let opts = ReportOptions {
            top: 2,
            focus: Some("work".into()),
            root: Some(dir.path().to_path_buf()),
            ..ReportOptions::default()
        };
        let r = report(&p, &a, Path::new("/s.txt"), &opts);
        assert_eq!(r["top"].as_array().unwrap().len(), 2);
        assert_eq!(r["top"][0]["name"], "work");
        assert_eq!(r["top"][0]["selfPercent"], 80.0);
        assert_eq!(r["top"][0]["location"], "app.py:4");
        assert_eq!(r["hotLines"][0]["location"], "app.py:4");
        assert_eq!(r["hotLines"][0]["code"], "total = sum(range(10))");
        assert_eq!(r["topByTotal"][0]["name"], "main");
        assert_eq!(r["focus"]["percent"], 80.0);
        assert_eq!(r["focus"]["callers"][0]["name"], "main");
        assert_eq!(r["focus"]["inside"][0]["focusSelfPercent"], 100.0);
        let none =
            report(&p, &a, Path::new("/s"), &ReportOptions { focus: Some("zzz".into()), ..ReportOptions::default() });
        assert_eq!(none["focus"]["percent"], 0.0);
        let (p2, a2) = load("main;f 3\n");
        let r2 = report(&p2, &a2, Path::new("/s"), &ReportOptions { debug_build: true, ..ReportOptions::default() });
        assert!(r2["notes"].as_array().unwrap().len() >= 2);
    }

    #[test]
    fn compare_two_snapshots() {
        let (pa, aa) = load("main;parse 50\nmain;render 50\n");
        let (pb, ab) = load("main;parse 20\nmain;render 50\nmain;cache 10\n");
        let c = compare((Path::new("/a"), &pa, &aa), (Path::new("/b"), &pb, &ab), 5);
        assert_eq!(c["faster"][0]["name"], "parse");
        assert_eq!(c["faster"][0]["aSelf"], 50);
        assert_eq!(c["slower"][0]["name"], "cache");
        assert_eq!(c["added"][0], "cache");
        assert_eq!(c["totalChangePercent"], -20.0);
        let t = timeline(&pa, 10);
        assert_eq!(t["threads"][0]["activity"].as_array().unwrap().len(), 0);
        assert_eq!(pct(1, 3), 33.3);
        assert_eq!(pct(1, 0), 0.0);
    }
}
