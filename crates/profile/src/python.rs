//! Python profiles written by CodeCraft's helper script (run by the project's interpreter):
//!
//! - cProfile statistics (`{"codecraft": "cprofile", "functions": [...]}`): per function its
//!   file, line, name, call counts, own time (`tt`) and cumulative time (`ct`), plus its callers
//!   with the per-edge times. cProfile measures a call graph, not stacks, so the call tree is
//!   reconstructed top-down from the roots, splitting each function's time between its callees
//!   by the edges' cumulative times (like snakeviz and gprof2dot). Recursion is cut.
//! - tracemalloc snapshots (`{"codecraft": "tracemalloc", "traces": [...]}`): the live
//!   allocations grouped by traceback (root first), with sizes and counts, plus (M15) the
//!   traced memory over time and the top allocation sites at the snapshot nearest the peak.

use std::collections::HashMap;

use serde_json::Value;

use crate::ProfileError;
use crate::model::{
    Allocation, Format, Frame, MAX_DEPTH, MAX_MEMORY_POINTS, MemoryPoint, MemoryTimeline, Profile, ProfileBuilder, Unit,
    scaled_weight,
};

/// Which helper document a JSON value is.
pub fn kind(v: &Value) -> Option<&str> {
    v.get("codecraft").and_then(Value::as_str)
}

/// Call tree nodes generated from cProfile statistics at most.
const MAX_NODES: usize = 400_000;
/// Functions read at most.
const MAX_FUNCTIONS: usize = 500_000;

struct Func {
    frame: u32,
    tt: f64,
    ct: f64,
    /// (callee index, edge own time, edge cumulative time)
    callees: Vec<(usize, f64, f64)>,
    has_callers: bool,
}

fn num(v: Option<&Value>) -> f64 {
    v.and_then(Value::as_f64).filter(|x| x.is_finite() && *x >= 0.0).unwrap_or(0.0)
}

fn python_frame(name: &str, file: &str, line: u64) -> Frame {
    // Built-ins are `('~', 0, '<built-in method time.sleep>')`.
    if file == "~" || file.is_empty() {
        return Frame { name: name.to_string(), module: Some("builtins".into()), ..Frame::default() };
    }
    Frame {
        name: name.to_string(),
        module: None,
        file: Some(file.to_string()),
        line: u32::try_from(line).ok().filter(|l| *l > 0),
    }
}

/// The cumulative time a function's calls of others took (recursion excluded).
fn out_time(f: &Func, index: usize) -> f64 {
    f.callees.iter().filter(|(c, _, _)| *c != index).map(|(_, _, ct)| ct).sum()
}

/// A call tree node to expand: (function, weight in seconds, own seconds, parent stack, path).
type Pending = (usize, f64, f64, Option<u32>, Vec<usize>);

/// Parse the helper's cProfile JSON.
pub fn parse_cprofile(v: &Value) -> Result<Profile, ProfileError> {
    let list = v
        .get("functions")
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileError::Format("no `functions` in the cProfile document".into()))?;
    let mut b = ProfileBuilder::new(Format::CProfile, Unit::Nanoseconds);
    let mut funcs: Vec<Func> = Vec::with_capacity(list.len().min(MAX_FUNCTIONS));
    for f in list.iter().take(MAX_FUNCTIONS) {
        let name = f.get("name").and_then(Value::as_str).unwrap_or("[unknown]");
        let file = f.get("file").and_then(Value::as_str).unwrap_or("");
        let line = f.get("line").and_then(Value::as_u64).unwrap_or(0);
        let frame = b.frame(python_frame(name, file, line))?;
        let calls = f.get("calls").and_then(Value::as_u64).unwrap_or(0);
        b.add_calls(frame, calls);
        funcs.push(Func {
            frame,
            tt: num(f.get("tt")),
            ct: num(f.get("ct")),
            callees: Vec::new(),
            has_callers: false,
        });
    }
    // Edges: each function lists its callers as [callerIndex, calls, primitiveCalls, tt, ct].
    for (callee, f) in list.iter().take(MAX_FUNCTIONS).enumerate() {
        let Some(callers) = f.get("callers").and_then(Value::as_array) else { continue };
        for c in callers.iter().take(10_000) {
            let Some(c) = c.as_array() else { continue };
            let Some(caller) = c.first().and_then(Value::as_u64).and_then(|i| usize::try_from(i).ok()) else {
                continue;
            };
            if caller >= funcs.len() {
                continue;
            }
            let (tt, ct) = (num(c.get(3)), num(c.get(4)));
            if caller != callee
                && let Some(f) = funcs.get_mut(callee)
            {
                f.has_callers = true;
            }
            if let Some(p) = funcs.get_mut(caller) {
                p.callees.push((callee, tt, ct));
            }
        }
    }
    // Roots: functions nobody called. Under `python -m cProfile` the program's top level is
    // `exec`, which the import machinery calls again for every module (a cycle), so it has
    // callers and only the profiler's own `disable` looked like a root (QA M14). When the roots
    // cover less than half of the longest cumulative time, the function with that time (the
    // outermost call: cProfile's cumulative time counts only primitive calls) is the top.
    let mut roots: Vec<(usize, f64)> =
        funcs.iter().enumerate().filter(|(_, f)| !f.has_callers).map(|(i, f)| (i, f.ct)).collect();
    let covered: f64 = roots.iter().map(|(_, w)| w).sum();
    let top = funcs
        .iter()
        .enumerate()
        .fold(None::<(usize, f64)>, |best, (i, f)| match best {
            Some((_, ct)) if ct >= f.ct => best,
            _ => Some((i, f.ct)),
        });
    if let Some((i, ct)) = top
        && ct > 0.0
        && covered < ct * 0.5
        && !roots.iter().any(|(r, _)| *r == i)
    {
        roots.push((i, ct));
    }
    let grand: f64 = roots.iter().map(|(_, w)| w).sum();
    let thread = b.thread("Main thread")?;
    let cutoff = grand * 1e-5;
    let mut nodes = 0usize;
    // (function, weight in seconds, own seconds, parent stack, path of function indices)
    let mut work: Vec<Pending> = Vec::new();
    for (r, w) in roots.iter().rev() {
        if let Some(f) = funcs.get(*r) {
            let own = if f.ct > 0.0 { f.tt * (w / f.ct).min(1.0) } else { f.tt };
            work.push((*r, *w, own, None, Vec::new()));
        }
    }
    while let Some((fi, weight, own, parent, mut path)) = work.pop() {
        let Some(f) = funcs.get(fi) else { continue };
        nodes += 1;
        if nodes > MAX_NODES {
            b.note("The cProfile call tree was cut (too many paths); the method list is complete");
            break;
        }
        let stack = b.stack(parent, f.frame)?;
        path.push(fi);
        let scale = if f.ct > 0.0 { weight / f.ct } else { 0.0 };
        let mut own = own;
        let deep = path.len() >= MAX_DEPTH;
        for (callee, tt, ct) in &f.callees {
            let child_weight = ct * scale;
            // cProfile can record a tiny cumulative time for a frame whose callees took long
            // (`<string>` → `run_module` under `-m`): keep it when what it calls is not small.
            let reach = funcs.get(*callee).map_or(0.0, |c| out_time(c, *callee) * scale);
            if path.contains(callee) || deep || (child_weight < cutoff && reach < cutoff) {
                // Recursion, too deep or too small: keep its time here.
                if !path.contains(callee) {
                    own += child_weight;
                }
                continue;
            }
            work.push((*callee, child_weight, tt * scale, Some(stack), path.clone()));
        }
        let w = scaled_weight(own, 1e9);
        if w > 0 {
            b.sample(thread, stack, w);
        }
    }
    if let Some(total) = v.get("totalTime").and_then(Value::as_f64).filter(|t| t.is_finite() && *t > 0.0) {
        b.note(format!("cProfile measured {total:.3} s of calls (deterministic; times include profiling overhead)"));
    } else {
        b.note("cProfile is deterministic: every call is timed, which adds overhead to small functions");
    }
    b.finish()
}

/// Parse the helper's tracemalloc JSON.
pub fn parse_tracemalloc(v: &Value) -> Result<Profile, ProfileError> {
    let traces = v
        .get("traces")
        .and_then(Value::as_array)
        .ok_or_else(|| ProfileError::Format("no `traces` in the tracemalloc document".into()))?;
    let mut b = ProfileBuilder::new(Format::Tracemalloc, Unit::Bytes);
    let thread = b.thread("Python heap")?;
    let mut path: Vec<u32> = Vec::new();
    let mut ids: HashMap<(String, String, u64), u32> = HashMap::new();
    for t in traces.iter().take(2_000_000) {
        let size = t.get("size").and_then(Value::as_u64).unwrap_or(0);
        if size == 0 {
            continue;
        }
        path.clear();
        for f in t.get("frames").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().take(MAX_DEPTH) {
            let name = f.get("name").and_then(Value::as_str).unwrap_or("<module>");
            let file = f.get("file").and_then(Value::as_str).unwrap_or("");
            let line = f.get("line").and_then(Value::as_u64).unwrap_or(0);
            let key = (name.to_string(), file.to_string(), line);
            let id = match ids.get(&key) {
                Some(id) => *id,
                None => {
                    let id = b.frame(python_frame(name, file, line))?;
                    ids.insert(key, id);
                    id
                }
            };
            path.push(id);
        }
        if let Some(s) = b.stack_of(&path)? {
            b.sample(thread, s, size);
        }
    }
    let fmt = |n: Option<u64>| n.map(|n| Unit::Bytes.format(n));
    if let (Some(current), Some(peak)) =
        (fmt(v.get("current").and_then(Value::as_u64)), fmt(v.get("peak").and_then(Value::as_u64)))
    {
        b.note(format!("Live Python allocations when the snapshot was taken: {current} (peak during the run: {peak})"));
    }
    if let Some(when) = v.get("when").and_then(Value::as_str) {
        b.note(format!("Snapshot taken {when}"));
    }
    let memory = memory_timeline(v);
    if let Some(m) = &memory {
        match (m.peak_snapshot_ms, m.peak_snapshot_bytes) {
            (Some(t), Some(bytes)) => b.note(format!(
                "Memory peaked at {} around {:.1} s; the snapshot nearest the peak ({:.1} s, {} live) is in `memory.atPeak`",
                Unit::Bytes.format(m.peak),
                m.peak_ms.unwrap_or(t) as f64 / 1000.0,
                t as f64 / 1000.0,
                Unit::Bytes.format(bytes)
            )),
            _ => b.note(format!("Memory peaked at {}", Unit::Bytes.format(m.peak))),
        }
    }
    let mut p = crate::model::fold_lines(b.finish()?);
    p.memory = memory;
    Ok(p)
}

/// The helper's memory timeline: `interval` (s), `timeline: [[ms, current, peak], …]` and
/// `peakSnapshot: {time, current, file, top: [{name, file, line, size, count, callers}]}`.
fn memory_timeline(v: &Value) -> Option<MemoryTimeline> {
    let points: Vec<MemoryPoint> = v
        .get("timeline")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|p| {
            let a = p.as_array()?;
            let n = |i: usize| a.get(i).and_then(Value::as_f64).map(|x| scaled_weight(x, 1.0));
            let current = n(1)?;
            Some(MemoryPoint { time_ms: n(0)?, current, peak: n(2).unwrap_or(current).max(current) })
        })
        .take(MAX_MEMORY_POINTS)
        .collect();
    let mut points = points;
    points.sort_by_key(|p| p.time_ms);
    let best = points.iter().max_by_key(|p| (p.peak, std::cmp::Reverse(p.time_ms))).copied();
    let peak = v.get("peak").and_then(Value::as_u64).unwrap_or(0).max(best.map_or(0, |b| b.peak));
    let snap = v.get("peakSnapshot").filter(|s| s.is_object());
    let at_peak: Vec<Allocation> = snap
        .and_then(|s| s.get("top"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(500)
        .filter_map(|a| {
            let size = a.get("size").and_then(Value::as_u64)?;
            Some(Allocation {
                name: a.get("name").and_then(Value::as_str).unwrap_or("<module>").to_string(),
                file: a.get("file").and_then(Value::as_str).filter(|f| !f.is_empty()).map(str::to_string),
                line: a.get("line").and_then(Value::as_u64).and_then(|l| u32::try_from(l).ok()).filter(|l| *l > 0),
                size,
                count: a.get("count").and_then(Value::as_u64).unwrap_or(0),
                callers: a
                    .get("callers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(8)
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect();
    let interval_ms = v.get("interval").and_then(Value::as_f64).map_or(0, |s| scaled_weight(s, 1000.0));
    Some(MemoryTimeline {
        interval_ms,
        points,
        peak,
        peak_ms: best.filter(|b| b.peak > 0).map(|b| b.time_ms),
        at_peak,
        peak_snapshot_ms: snap.and_then(|s| s.get("time")).and_then(Value::as_f64).map(|t| scaled_weight(t, 1.0)),
        peak_snapshot_bytes: snap.and_then(|s| s.get("current")).and_then(Value::as_u64),
        peak_snapshot_file: snap
            .and_then(|s| s.get("file"))
            .and_then(Value::as_str)
            // A file name next to the snapshot, never a path elsewhere.
            .filter(|f| !f.is_empty() && !f.contains('/') && !f.contains('\\') && *f != "..")
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn cprofile_doc() -> Value {
        // main calls work (twice) and io; work calls leaf; leaf recurses into itself.
        json!({
            "codecraft": "cprofile", "totalTime": 1.0,
            "functions": [
                { "name": "<module>", "file": "app.py", "line": 1, "calls": 1, "tt": 0.0, "ct": 1.0, "callers": [] },
                { "name": "main", "file": "app.py", "line": 10, "calls": 1, "tt": 0.1, "ct": 1.0, "callers": [[0, 1, 1, 0.1, 1.0]] },
                { "name": "work", "file": "app.py", "line": 20, "calls": 2, "tt": 0.2, "ct": 0.6, "callers": [[1, 2, 2, 0.2, 0.6]] },
                { "name": "leaf", "file": "app.py", "line": 30, "calls": 5, "tt": 0.4, "ct": 0.4,
                  "callers": [[2, 4, 4, 0.4, 0.4], [3, 1, 0, 0.0, 0.1]] },
                { "name": "<built-in method time.sleep>", "file": "~", "line": 0, "calls": 1, "tt": 0.3, "ct": 0.3,
                  "callers": [[1, 1, 1, 0.3, 0.3]] }
            ]
        })
    }

    #[test]
    fn cprofile_call_tree() {
        let p = parse_cprofile(&cprofile_doc()).unwrap();
        assert_eq!(p.format, Format::CProfile);
        assert_eq!(p.total(), 1_000_000_000);
        let a = crate::Analysis::new(&p, None);
        let get = |n: &str| a.methods.iter().find(|m| p.frame(m.frame).unwrap().name == n).copied().unwrap();
        assert_eq!(get("leaf").self_weight, 400_000_000);
        assert_eq!(get("work").total, 600_000_000);
        assert_eq!(get("main").self_weight, 100_000_000);
        let sleep = get("<built-in method time.sleep>");
        assert_eq!(sleep.self_weight, 300_000_000);
        assert_eq!(p.frame(sleep.frame).unwrap().module.as_deref(), Some("builtins"));
        assert_eq!(p.calls_of(get("leaf").frame), Some(5));
        assert_eq!(p.frame(get("work").frame).unwrap().location().as_deref(), Some("app.py:20"));
        assert!(!p.notes.is_empty());
    }

    /// `python -m cProfile -o out main.py`: the top level is `exec`, which the import machinery
    /// calls again for each module imported, so every function has a caller (a cycle) and only
    /// the profiler's own `disable` looked like a root (QA M14: the report showed nothing else).
    #[test]
    fn cprofile_top_level_exec_in_an_import_cycle() {
        let doc = json!({
            "codecraft": "cprofile", "totalTime": 0.088,
            "functions": [
                { "name": "<built-in method builtins.exec>", "file": "~", "line": 0, "calls": 3, "primcalls": 1,
                  "tt": 0.000002, "ct": 0.088, "callers": [[3, 2, 2, 0.000001, 0.000002]] },
                { "name": "<module>", "file": "main.py", "line": 1, "calls": 1, "tt": 0.000008, "ct": 0.088,
                  "callers": [[0, 1, 1, 0.000008, 0.088]] },
                { "name": "slow_primes", "file": "core.py", "line": 15, "calls": 1, "tt": 0.087, "ct": 0.087,
                  "callers": [[1, 1, 1, 0.087, 0.087]] },
                { "name": "_call_with_frames_removed", "file": "<frozen importlib._bootstrap>", "line": 241, "calls": 2,
                  "tt": 0.000001, "ct": 0.000003, "callers": [[1, 2, 2, 0.000001, 0.000003]] },
                { "name": "<method 'disable' of '_lsprof.Profiler' objects>", "file": "~", "line": 0, "calls": 1,
                  "tt": 0.0000005, "ct": 0.0000005, "callers": [] }
            ]
        });
        let p = parse_cprofile(&doc).unwrap();
        let a = crate::Analysis::new(&p, None);
        let get = |n: &str| a.methods.iter().find(|m| p.frame(m.frame).unwrap().name == n).copied().unwrap();
        let slow = get("slow_primes");
        assert!(slow.self_weight > p.total() * 9 / 10, "{} of {}", slow.self_weight, p.total());
        assert!(get("<module>").total > p.total() * 9 / 10);

        // `python -m cProfile -m pytest`: exec → <string> → run_module → _run_code → exec (the
        // edges' cumulative times exceed exec's own, as cProfile records them for recursion).
        let doc = json!({
            "codecraft": "cprofile", "totalTime": 0.316,
            "functions": [
                { "name": "<built-in method builtins.exec>", "file": "~", "line": 0, "calls": 27, "primcalls": 1,
                  "tt": 0.0002, "ct": 0.316, "callers": [[3, 1, 1, 0.000003, 0.222], [4, 25, 25, 0.0002, 0.151]] },
                { "name": "<module>", "file": "<string>", "line": 1, "calls": 1, "tt": 0.0, "ct": 0.0000002,
                  "callers": [[0, 1, 1, 0.0000002, 0.0000002]] },
                { "name": "run_module", "file": "runpy.py", "line": 199, "calls": 1, "tt": 0.000002, "ct": 0.3165,
                  "callers": [[1, 1, 1, 0.000002, 0.3165]] },
                { "name": "_run_code", "file": "runpy.py", "line": 86, "calls": 1, "tt": 0.000002, "ct": 0.2225,
                  "callers": [[2, 1, 1, 0.000002, 0.2225]] },
                { "name": "_call_with_frames_removed", "file": "<frozen importlib._bootstrap>", "line": 241, "calls": 25,
                  "tt": 0.00002, "ct": 0.151, "callers": [[2, 25, 25, 0.00002, 0.151]] },
                { "name": "<method 'disable' of '_lsprof.Profiler' objects>", "file": "~", "line": 0, "calls": 1,
                  "tt": 0.0000001, "ct": 0.0000001, "callers": [] },
                { "name": "<module>", "file": "pytest/__main__.py", "line": 1, "calls": 1, "tt": 0.00001, "ct": 0.2225,
                  "callers": [[0, 1, 1, 0.00001, 0.2225]] },
                { "name": "run_tests", "file": "pytest/main.py", "line": 10, "calls": 1, "tt": 0.2, "ct": 0.2,
                  "callers": [[6, 1, 1, 0.2, 0.2]] }
            ]
        });
        let p = parse_cprofile(&doc).unwrap();
        let a = crate::Analysis::new(&p, None);
        let hottest = a.methods.iter().max_by_key(|m| m.self_weight).unwrap();
        assert_eq!(p.frame(hottest.frame).unwrap().name, "run_tests", "the program is under the top-level exec");
    }

    #[test]
    fn tracemalloc_snapshot() {
        let doc = json!({
            "codecraft": "tracemalloc", "current": 3000, "peak": 9000, "when": "at exit",
            "traces": [
                { "size": 2000, "count": 2, "frames": [
                    { "name": "<module>", "file": "app.py", "line": 40 },
                    { "name": "main", "file": "app.py", "line": 12 },
                    { "name": "allocate", "file": "app.py", "line": 18 } ] },
                { "size": 1000, "count": 10, "frames": [
                    { "name": "<module>", "file": "app.py", "line": 40 },
                    { "name": "main", "file": "app.py", "line": 12 },
                    { "name": "allocate", "file": "app.py", "line": 20 } ] },
                { "size": 0, "frames": [] }
            ]
        });
        let p = parse_tracemalloc(&doc).unwrap();
        assert_eq!(p.unit, Unit::Bytes);
        assert_eq!(p.total(), 3000);
        let alloc = p.frames.iter().position(|f| f.name == "allocate").unwrap() as u32;
        let lines = p.lines_of(alloc);
        assert_eq!(lines.len(), 2);
        assert_eq!((lines[0].line, lines[0].self_weight), (18, 2000));
        // The function points at its heaviest line.
        assert_eq!(p.frames[alloc as usize].line, Some(18));
        assert_eq!(p.notes.len(), 2);
    }

    #[test]
    fn hostile_documents() {
        assert!(parse_cprofile(&json!({})).is_err());
        assert!(parse_cprofile(&json!({ "functions": [] })).is_err());
        let cyc = json!({ "functions": [
            { "name": "a", "file": "x", "line": 1, "tt": 1.0, "ct": 2.0, "callers": [[1, 1, 1, 1.0, 1e300], [9, 1, 1, 1, 1]] },
            { "name": "b", "file": "x", "line": 2, "tt": "x", "ct": -1.0, "callers": [[0, 1, 1, 1.0, 1.0], ["q"], 5] }
        ]});
        // A pure cycle has no root: nothing to show, but no panic.
        let _ = parse_cprofile(&cyc);
        assert!(parse_tracemalloc(&json!({ "traces": "x" })).is_err());
        assert!(parse_tracemalloc(&json!({ "traces": [{ "size": 5, "frames": [{}] }] })).is_ok());
    }

    /// The helper's periodic memory samples and the snapshot nearest the peak.
    #[test]
    fn tracemalloc_memory_timeline() {
        let doc = json!({
            "codecraft": "tracemalloc", "current": 1000, "peak": 50_000, "when": "when the program ended",
            "traces": [{ "size": 1000, "count": 1, "frames": [{ "name": "main", "file": "app.py", "line": 3 }] }],
            "interval": 0.1,
            "timeline": [[100.0, 2000, 2500], [200.0, 40_000, 50_000], [300.0, 1000, 40_000], ["x"], [400.0], 7],
            "peakSnapshot": {
                "time": 200.0, "current": 40_000, "file": "run.tracemalloc.peak.json",
                "top": [
                    { "name": "build", "file": "app.py", "line": 9, "size": 39_000, "count": 300, "callers": ["main (app.py:3)"] },
                    { "name": "<module>", "file": "", "line": 0, "size": 1000 },
                    { "size": "big" }
                ]
            }
        });
        let p = parse_tracemalloc(&doc).unwrap();
        let m = p.memory.as_ref().unwrap();
        assert_eq!(m.interval_ms, 100);
        assert_eq!(m.points.len(), 3);
        assert_eq!(m.peak, 50_000);
        assert_eq!(m.peak_ms, Some(200));
        assert_eq!(m.peak_snapshot_ms, Some(200));
        assert_eq!(m.peak_snapshot_bytes, Some(40_000));
        assert_eq!(m.peak_snapshot_file.as_deref(), Some("run.tracemalloc.peak.json"));
        assert_eq!(m.at_peak.len(), 2);
        assert_eq!((m.at_peak[0].name.as_str(), m.at_peak[0].line, m.at_peak[0].size), ("build", Some(9), 39_000));
        assert_eq!(m.at_peak[0].callers, vec!["main (app.py:3)".to_string()]);
        assert_eq!((m.at_peak[1].file.as_deref(), m.at_peak[1].line), (None, None));
        assert!(p.notes.iter().any(|n| n.contains("peaked at")), "{:?}", p.notes);
        // The live allocations at the end are the profile itself.
        assert_eq!(p.total(), 1000);
        // A peak file that points elsewhere is ignored; no timeline = no memory section.
        let mut evil = doc.clone();
        evil["peakSnapshot"]["file"] = json!("../../etc/passwd");
        assert_eq!(parse_tracemalloc(&evil).unwrap().memory.unwrap().peak_snapshot_file, None);
        let mut none = doc;
        none.as_object_mut().unwrap().remove("timeline");
        assert!(parse_tracemalloc(&none).unwrap().memory.is_none());
    }
}
