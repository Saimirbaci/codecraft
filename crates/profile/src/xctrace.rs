//! Instruments Time Profiler samples as `xctrace export` writes them:
//!
//! ```text
//! xctrace export --input run.trace \
//!     --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]'
//! ```
//!
//! The export is a `<trace-query-result>` with one `<row>` per sample: `<sample-time>`,
//! `<thread fmt="Main Thread 0x… (app, pid: 1)">`, `<weight>` (nanoseconds) and a
//! `<backtrace>` of `<frame name=… addr=…>` elements, leaf first, each with an optional
//! `<binary name=… path=…>` and `<source line=…><path>…</path></source>`. Any element with an
//! `id` may be repeated later as `<tag ref="id"/>`, so values are remembered by id.
//!
//! Frames carry the line each sample was at; [`crate::fold_lines`] turns them into functions
//! plus per-line statistics.

use std::collections::HashMap;

use crate::ProfileError;
use crate::model::{Format, Frame, MAX_DEPTH, Profile, ProfileBuilder, Unit};
use crate::xml::{Event, Reader};

/// Whether text looks like an `xctrace export` table.
pub fn looks_xctrace(text: &str) -> bool {
    let head = text.get(..text.len().min(4096)).unwrap_or(text);
    head.contains("<trace-query-result") || head.contains("<trace-toc")
}

/// Default weight of a sample without a `<weight>` (1 ms, the Time Profiler's interval).
const DEFAULT_WEIGHT_NS: u64 = 1_000_000;
/// Remembered ids at most (a hostile file can't grow the maps without bound).
const MAX_IDS: usize = 20_000_000;

#[derive(Default)]
struct FrameDraft {
    id: Option<u64>,
    name: String,
    binary: Option<String>,
    file: Option<String>,
    line: Option<u32>,
}

#[derive(Default)]
struct Row {
    time: Option<u64>,
    thread: Option<usize>,
    weight: Option<u64>,
    backtrace: Option<Vec<u32>>,
}

/// What text is being collected for.
enum Collect {
    None,
    Time(Option<u64>),
    Weight(Option<u64>),
    Path(Option<u64>),
}

/// Parse a Time Profiler export (and fold per-line frames into functions).
pub fn parse(text: &str) -> Result<Profile, ProfileError> {
    if text.contains("<schema name=\"time-sample\"") && !text.contains("<schema name=\"time-profile\"") {
        return Err(ProfileError::Format(
            "this is the time-sample table (raw addresses); export the time-profile table instead".into(),
        ));
    }
    let mut b = ProfileBuilder::new(Format::Xctrace, Unit::Nanoseconds);
    let mut threads: HashMap<u64, usize> = HashMap::new();
    let mut weights: HashMap<u64, u64> = HashMap::new();
    let mut times: HashMap<u64, u64> = HashMap::new();
    let mut backtraces: HashMap<u64, Vec<u32>> = HashMap::new();
    let mut frames: HashMap<u64, u32> = HashMap::new();
    let mut binaries: HashMap<u64, String> = HashMap::new();
    let mut paths: HashMap<u64, String> = HashMap::new();
    let mut remembered = 0usize;

    let mut row: Option<Row> = None;
    let mut bt: Option<(Option<u64>, Vec<u32>)> = None;
    let mut frame: Option<FrameDraft> = None;
    let mut collect = Collect::None;
    let mut text_buf = String::new();
    // Depth inside a <thread> element (its children aren't row columns).
    let mut in_thread = 0usize;
    let mut rows = 0u64;

    let id_of = |e: &Event, key: &str| e.attr(key).and_then(|v| v.trim().parse::<u64>().ok());

    for ev in Reader::new(text) {
        match &ev {
            Event::Start { name, empty, .. } => {
                let (id, rf) = (id_of(&ev, "id"), id_of(&ev, "ref"));
                if in_thread > 0 {
                    if !empty {
                        in_thread += 1;
                    }
                    continue;
                }
                match *name {
                    "row" => {
                        row = Some(Row::default());
                        bt = None;
                        frame = None;
                    }
                    "sample-time" if row.is_some() => match rf {
                        Some(r) => {
                            if let Some(rw) = row.as_mut() {
                                rw.time = times.get(&r).copied();
                            }
                        }
                        None if !empty => {
                            collect = Collect::Time(id);
                            text_buf.clear();
                        }
                        None => {}
                    },
                    "weight" if row.is_some() && bt.is_none() => match rf {
                        Some(r) => {
                            if let Some(rw) = row.as_mut() {
                                rw.weight = weights.get(&r).copied();
                            }
                        }
                        None if !empty => {
                            collect = Collect::Weight(id);
                            text_buf.clear();
                        }
                        None => {}
                    },
                    "thread" if row.is_some() => {
                        let idx = match rf {
                            Some(r) => threads.get(&r).copied(),
                            None => {
                                let label = thread_label(ev.attr("fmt").unwrap_or("Thread"));
                                let idx = b.thread_named(&label)?;
                                if let Some(id) = id
                                    && remembered < MAX_IDS
                                {
                                    threads.insert(id, idx);
                                    remembered += 1;
                                }
                                Some(idx)
                            }
                        };
                        if let Some(rw) = row.as_mut() {
                            rw.thread = idx;
                        }
                        if !empty {
                            in_thread = 1;
                        }
                    }
                    "backtrace" if row.is_some() => match rf {
                        Some(r) => {
                            if let Some(rw) = row.as_mut() {
                                rw.backtrace = backtraces.get(&r).cloned();
                            }
                        }
                        None if !empty => bt = Some((id, Vec::new())),
                        None => {}
                    },
                    "frame" if bt.is_some() => match rf {
                        Some(r) => {
                            if let (Some(f), Some((_, list))) = (frames.get(&r), bt.as_mut())
                                && list.len() < MAX_DEPTH
                            {
                                list.push(*f);
                            }
                        }
                        None => {
                            let draft = FrameDraft {
                                id,
                                name: ev.attr("name").or_else(|| ev.attr("addr")).unwrap_or("[unknown]").to_string(),
                                ..FrameDraft::default()
                            };
                            if *empty {
                                finish_frame(&mut b, draft, &mut frames, &mut bt, &mut remembered)?;
                            } else {
                                frame = Some(draft);
                            }
                        }
                    },
                    "binary" if frame.is_some() => {
                        let name = match rf {
                            Some(r) => binaries.get(&r).cloned(),
                            None => {
                                let n = ev.attr("name").map(str::to_string);
                                if let (Some(id), Some(n)) = (id, &n)
                                    && remembered < MAX_IDS
                                {
                                    binaries.insert(id, n.clone());
                                    remembered += 1;
                                }
                                n
                            }
                        };
                        if let Some(f) = frame.as_mut() {
                            f.binary = name.filter(|n| !n.is_empty());
                        }
                    }
                    "source" if frame.is_some() => {
                        let line = ev.attr("line").and_then(|l| l.trim().parse::<u32>().ok()).filter(|l| *l > 0);
                        if let Some(f) = frame.as_mut() {
                            f.line = line;
                        }
                    }
                    "path" if frame.is_some() => match rf {
                        Some(r) => {
                            if let Some(f) = frame.as_mut() {
                                f.file = paths.get(&r).cloned();
                            }
                        }
                        None if !empty => {
                            collect = Collect::Path(id);
                            text_buf.clear();
                        }
                        None => {}
                    },
                    _ => {}
                }
            }
            Event::Text(t) => {
                if !matches!(collect, Collect::None) && text_buf.len() < 64 * 1024 {
                    text_buf.push_str(t);
                }
            }
            Event::End { name } => {
                if in_thread > 0 {
                    in_thread -= 1;
                    continue;
                }
                match (*name, &collect) {
                    ("sample-time", Collect::Time(id)) => {
                        let v = text_buf.trim().parse::<u64>().ok();
                        if let (Some(id), Some(v)) = (id, v)
                            && remembered < MAX_IDS
                        {
                            times.insert(*id, v);
                            remembered += 1;
                        }
                        if let Some(rw) = row.as_mut() {
                            rw.time = v;
                        }
                        collect = Collect::None;
                    }
                    ("weight", Collect::Weight(id)) => {
                        let v = text_buf.trim().parse::<u64>().ok();
                        if let (Some(id), Some(v)) = (id, v)
                            && remembered < MAX_IDS
                        {
                            weights.insert(*id, v);
                            remembered += 1;
                        }
                        if let Some(rw) = row.as_mut() {
                            rw.weight = v;
                        }
                        collect = Collect::None;
                    }
                    ("path", Collect::Path(id)) => {
                        let v = text_buf.trim().to_string();
                        if let Some(id) = id
                            && remembered < MAX_IDS
                        {
                            paths.insert(*id, v.clone());
                            remembered += 1;
                        }
                        if let Some(f) = frame.as_mut() {
                            f.file = Some(v).filter(|v| !v.is_empty());
                        }
                        collect = Collect::None;
                    }
                    ("frame", _) => {
                        if let Some(draft) = frame.take() {
                            finish_frame(&mut b, draft, &mut frames, &mut bt, &mut remembered)?;
                        }
                    }
                    ("backtrace", _) => {
                        if let Some((id, list)) = bt.take() {
                            if let Some(id) = id
                                && remembered < MAX_IDS
                            {
                                backtraces.insert(id, list.clone());
                                remembered += 1;
                            }
                            if let Some(rw) = row.as_mut() {
                                rw.backtrace = Some(list);
                            }
                        }
                    }
                    ("row", _) => {
                        if let Some(rw) = row.take() {
                            rows += 1;
                            if let (Some(thread), Some(stack)) = (rw.thread, rw.backtrace) {
                                let root_first: Vec<u32> = stack.into_iter().rev().collect();
                                if let Some(s) = b.stack_of(&root_first)? {
                                    let weight = rw.weight.unwrap_or(DEFAULT_WEIGHT_NS);
                                    match rw.time {
                                        Some(t) => b.sample_at(thread, s, weight, t),
                                        None => b.sample(thread, s, weight),
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if !b.has_samples() {
        return Err(if rows == 0 && !looks_xctrace(text) {
            ProfileError::Format("no `<row>` samples in the xctrace export".into())
        } else {
            ProfileError::Format(
                "the trace has no Time Profiler samples (Instruments takes about half a second to start sampling, so a program that ends sooner leaves none)".into(),
            )
        });
    }
    let p = b.finish()?;
    let mut p = crate::model::fold_lines(p);
    let bare = p.unsymbolicated_weight();
    let total = p.total();
    if total > 0 && bare.saturating_mul(10) > total {
        p.notes.push(format!(
            "{:.0}% of the time is in frames without symbols (bare addresses); build with debug info and keep the dSYM / debug files next to the binary",
            bare as f64 * 100.0 / total as f64
        ));
    }
    Ok(p)
}

fn finish_frame(
    b: &mut ProfileBuilder,
    draft: FrameDraft,
    frames: &mut HashMap<u64, u32>,
    bt: &mut Option<(Option<u64>, Vec<u32>)>,
    remembered: &mut usize,
) -> Result<(), ProfileError> {
    let file = draft.file.filter(|f| !f.is_empty());
    let frame = Frame {
        name: if draft.name.trim().is_empty() { "[unknown]".into() } else { draft.name },
        module: draft.binary,
        line: if file.is_some() { draft.line } else { None },
        file,
    };
    let id = b.frame(frame)?;
    if let Some(fid) = draft.id
        && *remembered < MAX_IDS
    {
        frames.insert(fid, id);
        *remembered += 1;
    }
    if let Some((_, list)) = bt.as_mut()
        && list.len() < MAX_DEPTH
    {
        list.push(id);
    }
    Ok(())
}

/// `Main Thread  0x46ba832 (hot, pid: 71629)` → `Main Thread 0x46ba832`.
fn thread_label(fmt: &str) -> String {
    let base = match fmt.rfind(" (") {
        Some(i) if fmt.ends_with(')') && fmt.get(i..).is_some_and(|t| t.contains("pid")) => fmt.get(..i).unwrap_or(fmt),
        _ => fmt,
    };
    let label = base.split_whitespace().collect::<Vec<_>>().join(" ");
    if label.is_empty() { "Thread".into() } else { label }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/xctrace-time-profile.xml");

    #[test]
    fn parses_a_real_export() {
        assert!(looks_xctrace(FIXTURE));
        let p = parse(FIXTURE).unwrap();
        assert_eq!(p.format, Format::Xctrace);
        assert_eq!(p.unit, Unit::Nanoseconds);
        assert_eq!(p.threads.len(), 1);
        assert_eq!(p.threads[0].name, "Main Thread 0x46bb7a8");
        // Frames are functions; lines are statistics.
        let inner = p.frames.iter().position(|f| f.name == "inner").unwrap() as u32;
        let f = &p.frames[inner as usize];
        assert!(f.file.as_deref().unwrap().ends_with("hot.c"));
        assert_eq!(f.module.as_deref(), Some("hot"));
        assert_eq!(f.line, Some(5));
        assert!(!p.lines_of(inner).is_empty());
        assert!(p.duration_ns.is_some());
        assert_eq!(p.threads[0].activity.len(), crate::model::TIMELINE_BUCKETS);
        let a = crate::Analysis::new(&p, None);
        let top = p.frame(a.methods[0].frame).unwrap();
        assert_eq!(top.name, "inner");
    }

    #[test]
    fn refs_and_hostile_input() {
        let doc = r#"<?xml version="1.0"?><trace-query-result><node><schema name="time-profile"/>
<row><sample-time id="1">10</sample-time><thread id="2" fmt="T1 (a, pid: 1)"><tid id="3">7</tid></thread>
<weight id="4">2000</weight><backtrace id="5"><frame id="6" name="leaf" addr="0x1"><binary id="7" name="app"/>
<source line="12"><path id="8">/src/a.c</path></source></frame><frame id="9" name="main" addr="0x2"><binary ref="7"/>
<source line="3"><path ref="8"/></source></frame></backtrace></row>
<row><sample-time id="10">20</sample-time><thread ref="2"/><weight ref="4"/><backtrace ref="5"/></row>
<row><sample-time ref="999"/><thread ref="77"/><backtrace ref="5"/></row>
<row><thread ref="2"/><backtrace id="11"><frame ref="9"/></backtrace></row>
</node></trace-query-result>"#;
        let p = parse(doc).unwrap();
        assert_eq!(p.total(), 2000 + 2000 + DEFAULT_WEIGHT_NS);
        assert_eq!(p.threads[0].name, "T1");
        let leaf = p.frames.iter().position(|f| f.name == "leaf").unwrap() as u32;
        assert_eq!(p.lines_of(leaf)[0].line, 12);
        assert_eq!(p.lines_of(leaf)[0].self_weight, 4000);
        for bad in [
            "",
            "<trace-query-result>",
            "<trace-query-result><row><thread fmt='x'><backtrace><frame name='a'>",
            "<trace-query-result><row><backtrace ref='x'/></row></trace-query-result>",
            "<row><frame/><frame ref='1'/></row>",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let time_sample = "<trace-query-result><schema name=\"time-sample\"/></trace-query-result>";
        assert!(matches!(parse(time_sample), Err(ProfileError::Format(_))));
        assert_eq!(thread_label("Main Thread  0x1 (hot, pid: 5)"), "Main Thread 0x1");
        assert_eq!(thread_label(""), "Thread");
    }
}
