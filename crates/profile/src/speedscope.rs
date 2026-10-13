//! speedscope JSON (`https://www.speedscope.app/file-format-schema.json`): shared frames plus
//! sampled profiles (stacks of frame indices, root first, with weights) and evented profiles
//! (open/close events at times). py-spy writes it with `--format speedscope`.

use serde_json::Value;

use crate::ProfileError;
use crate::model::{Format, Frame, MAX_DEPTH, Profile, ProfileBuilder, Unit, scaled_weight};

/// Whether a JSON document looks like a speedscope file.
pub fn looks_speedscope(v: &Value) -> bool {
    v.get("$schema").and_then(Value::as_str).is_some_and(|s| s.contains("speedscope"))
        || (v.get("profiles").is_some_and(Value::is_array) && v.pointer("/shared/frames").is_some())
}

/// Unit and the factor that turns its values into the unit's integer form.
fn unit_of(name: &str) -> (Unit, f64) {
    match name {
        "nanoseconds" => (Unit::Nanoseconds, 1.0),
        "microseconds" => (Unit::Nanoseconds, 1e3),
        "milliseconds" => (Unit::Nanoseconds, 1e6),
        "seconds" => (Unit::Nanoseconds, 1e9),
        "bytes" => (Unit::Bytes, 1.0),
        _ => (Unit::Samples, 1.0),
    }
}

pub fn parse(v: &Value) -> Result<Profile, ProfileError> {
    let mut b = ProfileBuilder::new(Format::Speedscope, Unit::Samples);
    let frames: Vec<Option<u32>> = v
        .pointer("/shared/frames")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|f| frame(f).map(|f| b.frame(f)).transpose())
        .collect::<Result<_, _>>()?;
    let profiles =
        v.get("profiles").and_then(Value::as_array).ok_or_else(|| ProfileError::Format("no `profiles`".into()))?;
    let mut unit: Option<Unit> = None;
    let mut path: Vec<u32> = Vec::new();
    for (i, prof) in profiles.iter().enumerate() {
        let (u, scale) = unit_of(prof.get("unit").and_then(Value::as_str).unwrap_or("none"));
        // Profiles in another unit than the first can't be summed with it.
        if unit.is_some_and(|first| first != u) {
            log::warn!("speedscope profile {i} is in another unit; skipped");
            continue;
        }
        let name =
            prof.get("name").and_then(Value::as_str).map_or_else(|| format!("Profile {}", i + 1), str::to_string);
        let thread = b.thread(name)?;
        let before = b.has_samples();
        match prof.get("type").and_then(Value::as_str) {
            Some("sampled") => {
                let samples = prof.get("samples").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
                let weights = prof.get("weights").and_then(Value::as_array);
                for (j, sample) in samples.iter().enumerate() {
                    let weight = match weights {
                        Some(w) => w.get(j).and_then(Value::as_f64).map_or(0, |x| scaled_weight(x, scale)),
                        None => 1,
                    };
                    if weight == 0 {
                        continue;
                    }
                    path.clear();
                    for idx in sample.as_array().map(Vec::as_slice).unwrap_or(&[]).iter().take(MAX_DEPTH * 4) {
                        if let Some(f) = index(idx).and_then(|k| frames.get(k).copied().flatten()) {
                            path.push(f);
                        }
                    }
                    if let Some(s) = b.stack_of(&path)? {
                        b.sample(thread, s, weight);
                    }
                }
            }
            Some("evented") => {
                let mut events: Vec<(f64, bool, Option<u32>)> = prof
                    .get("events")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|e| {
                        let at = e.get("at")?.as_f64().filter(|a| a.is_finite())?;
                        let open = match e.get("type")?.as_str()? {
                            "O" => true,
                            "C" => false,
                            _ => return None,
                        };
                        let f = index(e.get("frame")?).and_then(|k| frames.get(k).copied().flatten());
                        Some((at, open, f))
                    })
                    .collect();
                // Stable: events at the same time keep their file order.
                events.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut open: Vec<Option<u32>> = Vec::new();
                let mut last = events.first().map_or(0.0, |e| e.0);
                for (at, is_open, f) in events {
                    let span = scaled_weight(at - last, scale);
                    if span > 0 && !open.is_empty() {
                        path.clear();
                        path.extend(open.iter().flatten().copied());
                        if let Some(s) = b.stack_of(&path)? {
                            b.sample(thread, s, span);
                        }
                    }
                    last = at;
                    if is_open {
                        if open.len() < MAX_DEPTH {
                            open.push(f);
                        }
                    } else if let Some(pos) = open.iter().rposition(|o| *o == f) {
                        // Close the frame (and anything left open above it).
                        open.truncate(pos);
                    }
                }
            }
            _ => {}
        }
        if !before && b.has_samples() && unit.is_none() {
            unit = Some(u);
            b.set_unit(u);
        }
    }
    b.finish()
}

fn index(v: &Value) -> Option<usize> {
    usize::try_from(v.as_u64()?).ok()
}

fn frame(v: &Value) -> Option<Frame> {
    let name = v.get("name").and_then(Value::as_str).unwrap_or("[unknown]");
    let mut f = Frame::named(if name.is_empty() { "[unknown]" } else { name });
    f.file = v.get("file").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    f.line = v.get("line").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sampled_profile_like_py_spy() {
        let v = json!({
            "$schema": "https://www.speedscope.app/file-format-schema.json",
            "shared": { "frames": [
                { "name": "<module>", "file": "run.py", "line": 1 },
                { "name": "main", "file": "run.py", "line": 10 },
                { "name": "work", "file": "lib.py", "line": 5 }
            ]},
            "profiles": [
                { "type": "sampled", "name": "MainThread", "unit": "seconds", "startValue": 0, "endValue": 1,
                  "samples": [[0, 1, 2], [0, 1, 2], [0, 1]], "weights": [0.01, 0.01, 0.02] },
                { "type": "sampled", "name": "Worker", "unit": "seconds", "samples": [[0, 2]], "weights": [0.005] }
            ],
            "exporter": "py-spy@0.3.14"
        });
        assert!(looks_speedscope(&v));
        let p = parse(&v).unwrap();
        assert_eq!(p.unit, Unit::Nanoseconds);
        assert_eq!(p.threads.len(), 2);
        assert_eq!(p.threads[0].total(), 40_000_000);
        assert_eq!(p.threads[1].total(), 5_000_000);
        let work = p.frames.iter().find(|f| f.name == "work").unwrap();
        assert_eq!(work.location().as_deref(), Some("lib.py:5"));
    }

    #[test]
    fn evented_profile() {
        let v = json!({
            "shared": { "frames": [{ "name": "a" }, { "name": "b" }, { "name": "c" }] },
            "profiles": [{ "type": "evented", "name": "x", "unit": "milliseconds", "startValue": 0, "endValue": 10,
                "events": [
                    { "type": "O", "frame": 0, "at": 0 },
                    { "type": "O", "frame": 1, "at": 2 },
                    { "type": "C", "frame": 1, "at": 5 },
                    { "type": "O", "frame": 2, "at": 6 },
                    { "type": "C", "frame": 2, "at": 7 },
                    { "type": "C", "frame": 0, "at": 10 }
                ]}]
        });
        let p = parse(&v).unwrap();
        let t = crate::CallTree::build(&p, None);
        assert_eq!(t.total, 10_000_000);
        let a = t.node(t.roots[0]).unwrap();
        assert_eq!(a.self_weight, 6_000_000);
        let kids: Vec<u64> = a.children.iter().map(|c| t.node(*c).unwrap().total).collect();
        assert_eq!(kids, vec![3_000_000, 1_000_000]);
    }

    #[test]
    fn hostile_speedscope_never_panics() {
        let v = json!({
            "shared": { "frames": [{ "name": "" }, 5, { "name": "z", "line": -3 }] },
            "profiles": [
                { "type": "sampled", "samples": [[0, 99999999999u64, -1, "x"], 7, [1, 2]], "weights": [f64::MAX, "w", 2] },
                { "type": "evented", "unit": "nanoseconds", "events": [
                    { "type": "C", "frame": 0, "at": 1 }, { "type": "O", "frame": 9, "at": 1 },
                    { "type": "O", "frame": 2, "at": -1e308 }, { "type": "X", "at": 3 }, { "at": "x" } ] },
                { "type": "unknown" }, 5
            ]
        });
        // The evented profile is in another unit than the sampled one, so it is skipped.
        let p = parse(&v).unwrap();
        assert_eq!(p.threads.len(), 1);
        assert!(parse(&json!({ "shared": { "frames": [] }, "profiles": [] })).is_err());
        assert!(parse(&json!({ "shared": {} })).is_err());
    }
}
