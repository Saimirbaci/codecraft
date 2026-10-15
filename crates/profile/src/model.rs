//! The profile model: interned frames, a prefix tree of stacks and per-thread sample weights.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::ProfileError;

/// Frames per stack kept (deeper stacks are cut at the root side).
pub const MAX_DEPTH: usize = 2048;
/// Distinct frames kept.
pub const MAX_FRAMES: usize = 4_000_000;
/// Distinct stacks kept.
pub const MAX_STACKS: usize = 16_000_000;
/// Threads kept.
pub const MAX_THREADS: usize = 10_000;
/// Buckets of a thread's activity timeline.
pub const TIMELINE_BUCKETS: usize = 200;
/// Timed samples kept for the timeline (later ones still count, but not on the timeline).
pub const MAX_TIMED_SAMPLES: usize = 4_000_000;

/// What sample weights count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Unit {
    /// Sample counts.
    #[default]
    Samples,
    /// Time in nanoseconds.
    Nanoseconds,
    /// Allocated bytes.
    Bytes,
}

impl Unit {
    /// A weight for display (`1,234 samples`, `12.5 ms`, `3.2 MB`).
    pub fn format(self, value: u64) -> String {
        match self {
            Self::Samples => format!("{} {}", group_digits(value), if value == 1 { "sample" } else { "samples" }),
            Self::Nanoseconds => {
                let v = value as f64;
                if value >= 1_000_000_000 {
                    format!("{:.2} s", v / 1e9)
                } else if value >= 1_000_000 {
                    format!("{:.1} ms", v / 1e6)
                } else if value >= 1_000 {
                    format!("{:.1} µs", v / 1e3)
                } else {
                    format!("{value} ns")
                }
            }
            Self::Bytes => {
                let v = value as f64;
                if value >= 1 << 30 {
                    format!("{:.1} GB", v / f64::from(1u32 << 30))
                } else if value >= 1 << 20 {
                    format!("{:.1} MB", v / f64::from(1u32 << 20))
                } else if value >= 1 << 10 {
                    format!("{:.1} KB", v / 1024.0)
                } else {
                    format!("{value} B")
                }
            }
        }
    }
}

fn group_digits(v: u64) -> String {
    let digits = v.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The file format a profile came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Format {
    /// Firefox Profiler "processed" JSON (what samply saves).
    #[default]
    Processed,
    /// Firefox Gecko profile JSON (schema/data tables).
    Gecko,
    /// speedscope JSON (py-spy `--format speedscope`).
    Speedscope,
    /// Collapsed stacks (`a;b;c 42`).
    Collapsed,
    /// `perf script` output.
    PerfScript,
    /// DTrace stack aggregation output.
    Dtrace,
    /// `xctrace export` of an Instruments Time Profiler table (macOS).
    Xctrace,
    /// macOS `sample` / `malloc_history -callTree` call graph text.
    CallGraph,
    /// Python cProfile statistics (converted by CodeCraft's helper).
    CProfile,
    /// Python tracemalloc snapshot (written by CodeCraft's helper).
    Tracemalloc,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Self::Processed => "Firefox Profiler (samply)",
            Self::Gecko => "Gecko profile",
            Self::Speedscope => "speedscope",
            Self::Collapsed => "collapsed stacks",
            Self::PerfScript => "perf script",
            Self::Dtrace => "DTrace aggregation",
            Self::Xctrace => "Instruments Time Profiler (xctrace)",
            Self::CallGraph => "call graph (sample / malloc_history)",
            Self::CProfile => "Python cProfile",
            Self::Tracemalloc => "Python tracemalloc",
        }
    }
}

/// One function (or unsymbolicated address) in a stack.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Frame {
    /// The function name, or `0x…` for an unsymbolicated address.
    pub name: String,
    /// The library or module (`libc.so.6`, `app`).
    pub module: Option<String>,
    /// The source file, when known.
    pub file: Option<String>,
    /// The 1-based source line, when known.
    pub line: Option<u32>,
}

impl Frame {
    pub fn named(name: impl Into<String>) -> Self {
        Self { name: name.into(), ..Self::default() }
    }

    /// `name` plus ` (module)` when there is one.
    pub fn label(&self) -> String {
        match &self.module {
            Some(m) if !m.is_empty() => format!("{} ({m})", self.name),
            _ => self.name.clone(),
        }
    }

    /// `file:line` when the source location is known.
    pub fn location(&self) -> Option<String> {
        let file = self.file.as_ref()?;
        Some(match self.line {
            Some(l) => format!("{file}:{l}"),
            None => file.clone(),
        })
    }
}

/// A stack: a frame on top of its parent stack (`None` = a root frame).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackNode {
    /// Always a smaller index than this node's own.
    pub parent: Option<u32>,
    pub frame: u32,
}

/// A thread (or a speedscope profile, or a whole collapsed file).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub name: String,
    /// Merged sample weights per stack, by stack index (identical stacks are summed).
    pub weights: Vec<(u32, u64)>,
    /// Samples read (before merging).
    pub samples: u64,
    /// Weight per time bucket over [`Profile::duration_ns`] ([`TIMELINE_BUCKETS`] buckets), when
    /// the format has sample times; empty otherwise.
    #[serde(default)]
    pub activity: Vec<u64>,
}

impl Thread {
    pub fn total(&self) -> u64 {
        self.weights.iter().fold(0u64, |a, (_, w)| a.saturating_add(*w))
    }
}

/// Samples attributed to one source line of a function (from formats that record the line
/// each sample was at: xctrace, py-spy, tracemalloc, memray).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineStat {
    pub frame: u32,
    /// 1-based.
    pub line: u32,
    /// Weight of samples whose leaf is at this line.
    pub self_weight: u64,
    /// Weight of samples with this line anywhere on the stack (counted once per sample).
    pub total: u64,
}

/// A loaded profile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub format: Format,
    pub unit: Unit,
    pub frames: Vec<Frame>,
    pub stacks: Vec<StackNode>,
    pub threads: Vec<Thread>,
    /// Per-line weights, sorted by `(frame, line)`; empty when the format has no sample lines.
    #[serde(default)]
    pub lines: Vec<LineStat>,
    /// Call counts per frame (cProfile), indexed like `frames`; empty when unknown.
    #[serde(default)]
    pub calls: Vec<u64>,
    /// Time from the first to the last sample, when the format has sample times.
    #[serde(default)]
    pub duration_ns: Option<u64>,
    /// Facts worth telling the reader (peak memory, what the numbers mean).
    #[serde(default)]
    pub notes: Vec<String>,
}

impl Profile {
    pub fn frame(&self, id: u32) -> Option<&Frame> {
        self.frames.get(id as usize)
    }

    /// The frames of a stack, root first.
    pub fn stack_frames(&self, stack: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = Some(stack);
        while let Some(id) = cur {
            let Some(node) = self.stacks.get(id as usize) else { break };
            out.push(node.frame);
            // Parents always have smaller indices, so this terminates; the cap is belt and braces.
            cur = node.parent.filter(|p| *p < id);
            if out.len() > MAX_DEPTH {
                break;
            }
        }
        out.reverse();
        out
    }

    /// Total weight over all threads.
    pub fn total(&self) -> u64 {
        self.threads.iter().fold(0u64, |a, t| a.saturating_add(t.total()))
    }

    /// Samples read over all threads.
    pub fn sample_count(&self) -> u64 {
        self.threads.iter().fold(0u64, |a, t| a.saturating_add(t.samples))
    }

    /// The line statistics of one frame (a slice of [`Profile::lines`]).
    pub fn lines_of(&self, frame: u32) -> &[LineStat] {
        let start = self.lines.partition_point(|l| l.frame < frame);
        let end = self.lines.partition_point(|l| l.frame <= frame);
        self.lines.get(start..end.max(start)).unwrap_or(&[])
    }

    /// Calls of a frame (cProfile), when known.
    pub fn calls_of(&self, frame: u32) -> Option<u64> {
        self.calls.get(frame as usize).copied().filter(|c| *c > 0)
    }

    /// Weight of samples whose leaf frame has no symbol (a bare `0x…` address).
    pub fn unsymbolicated_weight(&self) -> u64 {
        let bare: Vec<bool> = self.frames.iter().map(|f| is_address(&f.name)).collect();
        if !bare.iter().any(|b| *b) {
            return 0;
        }
        let mut total = 0u64;
        for t in &self.threads {
            for (stack, w) in &t.weights {
                let leaf = self.stacks.get(*stack as usize).map(|s| s.frame);
                if leaf.and_then(|f| bare.get(f as usize).copied()).unwrap_or(false) {
                    total = total.saturating_add(*w);
                }
            }
        }
        total
    }
}

/// Whether a frame name is a bare address (`0x1a2b`), i.e. not symbolicated.
pub fn is_address(name: &str) -> bool {
    name.strip_prefix("0x").is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Merge frames that differ only by line (the line a sample was at) into one frame per
/// function, moving the per-sample lines into [`Profile::lines`]. A merged frame's line becomes
/// its hottest line (most total weight), so Jump to Source lands where the time went. Frames
/// without a file are kept as they are; thread activity, notes and the duration are kept.
pub fn fold_lines(p: Profile) -> Profile {
    if !p.frames.iter().any(|f| f.file.is_some() && f.line.is_some()) {
        return p;
    }
    type Key = (String, Option<String>, Option<String>);
    let mut func_ids: HashMap<Key, u32> = HashMap::new();
    let mut frames: Vec<Frame> = Vec::new();
    let mut map: Vec<u32> = Vec::with_capacity(p.frames.len());
    let mut line_of: Vec<Option<u32>> = Vec::with_capacity(p.frames.len());
    for f in &p.frames {
        let line = if f.file.is_some() { f.line.filter(|l| *l > 0) } else { None };
        let key = (f.name.clone(), f.module.clone(), f.file.clone());
        let id = match func_ids.get(&key) {
            Some(id) => *id,
            None => {
                let Ok(id) = u32::try_from(frames.len()) else { break };
                let keep_line = if f.file.is_some() { None } else { f.line };
                frames.push(Frame { line: keep_line, ..f.clone() });
                func_ids.insert(key, id);
                id
            }
        };
        map.push(id);
        line_of.push(line);
    }
    // Stacks, rebuilt in index order (parents first).
    let mut stack_ids: HashMap<(Option<u32>, u32), u32> = HashMap::new();
    let mut stacks: Vec<StackNode> = Vec::new();
    let mut smap: Vec<u32> = Vec::with_capacity(p.stacks.len());
    for (i, s) in p.stacks.iter().enumerate() {
        let parent = s.parent.filter(|q| (*q as usize) < i).and_then(|q| smap.get(q as usize).copied());
        let frame = map.get(s.frame as usize).copied().unwrap_or(0);
        let id = match stack_ids.get(&(parent, frame)) {
            Some(id) => *id,
            None => {
                let Ok(id) = u32::try_from(stacks.len()) else { break };
                stacks.push(StackNode { parent, frame });
                stack_ids.insert((parent, frame), id);
                id
            }
        };
        smap.push(id);
    }
    // Line statistics and remapped weights.
    let mut lines: HashMap<(u32, u32), (u64, u64)> = HashMap::new();
    let mut seen: Vec<(u32, u32)> = Vec::new();
    let mut threads = Vec::with_capacity(p.threads.len());
    for t in &p.threads {
        let mut merged: HashMap<u32, u64> = HashMap::new();
        for (stack, w) in &t.weights {
            let Some(new) = smap.get(*stack as usize).copied() else { continue };
            let e = merged.entry(new).or_insert(0);
            *e = e.saturating_add(*w);
            seen.clear();
            let mut leaf = true;
            let mut cur = Some(*stack);
            let mut depth = 0;
            while let Some(id) = cur {
                let Some(node) = p.stacks.get(id as usize) else { break };
                if let (Some(Some(line)), Some(func)) = (line_of.get(node.frame as usize), map.get(node.frame as usize))
                {
                    let key = (*func, *line);
                    let e = lines.entry(key).or_insert((0, 0));
                    if leaf {
                        e.0 = e.0.saturating_add(*w);
                    }
                    if !seen.contains(&key) {
                        e.1 = e.1.saturating_add(*w);
                        seen.push(key);
                    }
                }
                leaf = false;
                cur = node.parent.filter(|q| *q < id);
                depth += 1;
                if depth > MAX_DEPTH {
                    break;
                }
            }
        }
        let mut weights: Vec<(u32, u64)> = merged.into_iter().collect();
        weights.sort_unstable();
        threads.push(Thread { weights, ..t.clone() });
    }
    let mut lines: Vec<LineStat> = lines
        .into_iter()
        .map(|((frame, line), (self_weight, total))| LineStat { frame, line, self_weight, total })
        .collect();
    lines.sort_unstable_by_key(|l| (l.frame, l.line));
    // A merged frame points at its hottest line.
    let mut best: HashMap<u32, (u64, u32)> = HashMap::new();
    for l in &lines {
        let e = best.entry(l.frame).or_insert((0, l.line));
        if l.total > e.0 {
            *e = (l.total, l.line);
        }
    }
    for (frame, (_, line)) in best {
        if let Some(f) = frames.get_mut(frame as usize) {
            f.line = Some(line);
        }
    }
    let mut calls = Vec::new();
    if !p.calls.is_empty() {
        calls = vec![0u64; frames.len()];
        for (old, c) in p.calls.iter().enumerate() {
            if let Some(slot) = map.get(old).and_then(|n| calls.get_mut(*n as usize)) {
                *slot = slot.saturating_add(*c);
            }
        }
    }
    Profile { frames, stacks, threads, lines, calls, ..p }
}

/// Builds a [`Profile`], interning frames and stacks so identical ones are shared.
#[derive(Debug, Default)]
pub struct ProfileBuilder {
    profile: Profile,
    frame_ids: HashMap<Frame, u32>,
    stack_ids: HashMap<(Option<u32>, u32), u32>,
    weights: Vec<HashMap<u32, u64>>,
    /// (thread, time in ns, weight) of timed samples.
    times: Vec<(u32, u64, u64)>,
}

impl ProfileBuilder {
    pub fn new(format: Format, unit: Unit) -> Self {
        Self { profile: Profile { format, unit, ..Profile::default() }, ..Self::default() }
    }

    pub fn set_unit(&mut self, unit: Unit) {
        self.profile.unit = unit;
    }

    pub fn unit(&self) -> Unit {
        self.profile.unit
    }

    /// The id of a frame (added on first use).
    pub fn frame(&mut self, frame: Frame) -> Result<u32, ProfileError> {
        if let Some(id) = self.frame_ids.get(&frame) {
            return Ok(*id);
        }
        if self.profile.frames.len() >= MAX_FRAMES {
            return Err(ProfileError::TooLarge(format!("more than {MAX_FRAMES} distinct frames")));
        }
        let id = u32::try_from(self.profile.frames.len()).map_err(|_| ProfileError::TooLarge("frames".into()))?;
        self.profile.frames.push(frame.clone());
        self.frame_ids.insert(frame, id);
        Ok(id)
    }

    /// The id of `frame` on top of `parent` (added on first use).
    pub fn stack(&mut self, parent: Option<u32>, frame: u32) -> Result<u32, ProfileError> {
        if let Some(id) = self.stack_ids.get(&(parent, frame)) {
            return Ok(*id);
        }
        if self.profile.stacks.len() >= MAX_STACKS {
            return Err(ProfileError::TooLarge(format!("more than {MAX_STACKS} distinct stacks")));
        }
        let id = u32::try_from(self.profile.stacks.len()).map_err(|_| ProfileError::TooLarge("stacks".into()))?;
        // Only known parents (smaller ids) are kept, which keeps the table acyclic.
        let parent = parent.filter(|p| *p < id);
        self.profile.stacks.push(StackNode { parent, frame });
        self.stack_ids.insert((parent, frame), id);
        Ok(id)
    }

    /// The stack for frames listed root first (`None` for an empty list). Cut to [`MAX_DEPTH`].
    pub fn stack_of(&mut self, frames_root_first: &[u32]) -> Result<Option<u32>, ProfileError> {
        let skip = frames_root_first.len().saturating_sub(MAX_DEPTH);
        let mut cur = None;
        for f in frames_root_first.iter().skip(skip) {
            cur = Some(self.stack(cur, *f)?);
        }
        Ok(cur)
    }

    /// Add a thread; returns its index.
    pub fn thread(&mut self, name: impl Into<String>) -> Result<usize, ProfileError> {
        if self.profile.threads.len() >= MAX_THREADS {
            return Err(ProfileError::TooLarge(format!("more than {MAX_THREADS} threads")));
        }
        self.profile.threads.push(Thread { name: name.into(), ..Thread::default() });
        self.weights.push(HashMap::new());
        Ok(self.profile.threads.len() - 1)
    }

    /// The thread named `name`, added if missing.
    pub fn thread_named(&mut self, name: &str) -> Result<usize, ProfileError> {
        match self.profile.threads.iter().position(|t| t.name == name) {
            Some(i) => Ok(i),
            None => self.thread(name),
        }
    }

    /// Record one sample (or an aggregated count) of `stack` on `thread`.
    pub fn sample(&mut self, thread: usize, stack: u32, weight: u64) {
        let (Some(t), Some(w)) = (self.profile.threads.get_mut(thread), self.weights.get_mut(thread)) else { return };
        if (stack as usize) >= self.profile.stacks.len() {
            return;
        }
        t.samples = t.samples.saturating_add(1);
        let e = w.entry(stack).or_insert(0);
        *e = e.saturating_add(weight);
    }

    /// Record a sample taken at `time_ns` (for the threads timeline).
    pub fn sample_at(&mut self, thread: usize, stack: u32, weight: u64, time_ns: u64) {
        let before = self.profile.threads.get(thread).map_or(0, |t| t.samples);
        self.sample(thread, stack, weight);
        let counted = self.profile.threads.get(thread).is_some_and(|t| t.samples > before);
        if counted
            && self.times.len() < MAX_TIMED_SAMPLES
            && let Ok(t) = u32::try_from(thread)
        {
            self.times.push((t, time_ns, weight));
        }
    }

    /// Add a note for the reader.
    pub fn note(&mut self, text: impl Into<String>) {
        if self.profile.notes.len() < 32 {
            self.profile.notes.push(text.into());
        }
    }

    /// Add to a frame's call count (cProfile).
    pub fn add_calls(&mut self, frame: u32, calls: u64) {
        let i = frame as usize;
        if i >= self.profile.frames.len() {
            return;
        }
        if self.profile.calls.len() < self.profile.frames.len() {
            self.profile.calls.resize(self.profile.frames.len(), 0);
        }
        if let Some(slot) = self.profile.calls.get_mut(i) {
            *slot = slot.saturating_add(calls);
        }
    }

    pub fn has_samples(&self) -> bool {
        self.weights.iter().any(|w| !w.is_empty())
    }

    /// The profile. Threads without samples are dropped; an empty profile is an error.
    pub fn finish(mut self) -> Result<Profile, ProfileError> {
        for (t, w) in self.profile.threads.iter_mut().zip(self.weights) {
            let mut v: Vec<(u32, u64)> = w.into_iter().filter(|(_, w)| *w > 0).collect();
            v.sort_unstable();
            t.weights = v;
        }
        if !self.profile.calls.is_empty() {
            self.profile.calls.resize(self.profile.frames.len(), 0);
        }
        let first = self.times.iter().map(|t| t.1).min();
        let last = self.times.iter().map(|t| t.1).max();
        if let (Some(first), Some(last)) = (first, last) {
            let span = u128::from(last.saturating_sub(first)) + 1;
            self.profile.duration_ns = Some(last.saturating_sub(first));
            for (thread, time, weight) in &self.times {
                let Some(t) = self.profile.threads.get_mut(*thread as usize) else { continue };
                if t.activity.is_empty() {
                    t.activity = vec![0; TIMELINE_BUCKETS];
                }
                let offset = u128::from(time.saturating_sub(first));
                let bucket = usize::try_from(offset * TIMELINE_BUCKETS as u128 / span).unwrap_or(0);
                if let Some(slot) = t.activity.get_mut(bucket.min(TIMELINE_BUCKETS - 1)) {
                    *slot = slot.saturating_add(*weight);
                }
            }
        }
        self.profile.threads.retain(|t| !t.weights.is_empty());
        if self.profile.threads.is_empty() {
            return Err(ProfileError::Empty);
        }
        Ok(self.profile)
    }
}

/// A weight given as a JSON number (float or integer) as a non-negative integer, scaled.
pub(crate) fn scaled_weight(v: f64, scale: f64) -> u64 {
    let x = v * scale;
    if x.is_nan() || x <= 0.0 {
        0
    } else if x >= u64::MAX as f64 {
        u64::MAX
    } else {
        x.round() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_format() {
        assert_eq!(Unit::Samples.format(1), "1 sample");
        assert_eq!(Unit::Samples.format(1234567), "1,234,567 samples");
        assert_eq!(Unit::Nanoseconds.format(1_500_000), "1.5 ms");
        assert_eq!(Unit::Nanoseconds.format(12), "12 ns");
        assert_eq!(Unit::Bytes.format(2048), "2.0 KB");
    }

    #[test]
    fn builder_interns_frames_and_stacks() {
        let mut b = ProfileBuilder::new(Format::Collapsed, Unit::Samples);
        let a = b.frame(Frame::named("a")).unwrap();
        let a2 = b.frame(Frame::named("a")).unwrap();
        assert_eq!(a, a2);
        let c = b.frame(Frame::named("c")).unwrap();
        let s1 = b.stack_of(&[a, c]).unwrap().unwrap();
        let s2 = b.stack_of(&[a, c]).unwrap().unwrap();
        assert_eq!(s1, s2);
        let t = b.thread("main").unwrap();
        b.sample(t, s1, 2);
        b.sample(t, s2, 3);
        b.sample(t, 999, 3);
        let p = b.finish().unwrap();
        assert_eq!(p.threads[0].weights, vec![(s1, 5)]);
        assert_eq!(p.threads[0].samples, 2);
        assert_eq!(p.stack_frames(s1), vec![a, c]);
    }

    #[test]
    fn timeline_buckets_and_fold() {
        let mut b = ProfileBuilder::new(Format::Xctrace, Unit::Nanoseconds);
        let at = |line: u32| Frame { name: "f".into(), file: Some("a.c".into()), line: Some(line), module: None };
        let main =
            b.frame(Frame { name: "main".into(), file: Some("a.c".into()), line: Some(1), module: None }).unwrap();
        let f5 = b.frame(at(5)).unwrap();
        let f6 = b.frame(at(6)).unwrap();
        let s5 = b.stack_of(&[main, f5]).unwrap().unwrap();
        let s6 = b.stack_of(&[main, f6]).unwrap().unwrap();
        let t = b.thread("t").unwrap();
        let idle = b.thread("idle").unwrap();
        b.sample_at(t, s5, 10, 1_000);
        b.sample_at(t, s5, 10, 1_010);
        b.sample_at(t, s6, 10, 11_000);
        b.sample_at(99, s6, 10, 50_000);
        b.note("n");
        let _ = idle;
        let p = b.finish().unwrap();
        assert_eq!(p.duration_ns, Some(10_000));
        assert_eq!(p.threads.len(), 1);
        let act = &p.threads[0].activity;
        assert_eq!(act.len(), TIMELINE_BUCKETS);
        assert_eq!(act[0], 20);
        assert_eq!(act[TIMELINE_BUCKETS - 1], 10);
        let p = fold_lines(p);
        assert_eq!(p.frames.len(), 2);
        let f = p.frames.iter().position(|x| x.name == "f").unwrap() as u32;
        assert_eq!(p.frames[f as usize].line, Some(5));
        let lines = p.lines_of(f);
        assert_eq!(
            lines.iter().map(|l| (l.line, l.self_weight, l.total)).collect::<Vec<_>>(),
            vec![(5, 20, 20), (6, 10, 10)]
        );
        let m = p.frames.iter().position(|x| x.name == "main").unwrap() as u32;
        assert_eq!(p.lines_of(m), &[LineStat { frame: m, line: 1, self_weight: 0, total: 30 }]);
        assert_eq!(p.total(), 30);
        assert_eq!(p.threads[0].activity[0], 20);
        assert_eq!(p.notes, vec!["n".to_string()]);
        assert!(p.lines_of(99).is_empty());
        assert!(is_address("0x1f") && !is_address("0x") && !is_address("main"));
    }

    #[test]
    fn empty_profile_is_an_error() {
        let mut b = ProfileBuilder::new(Format::Collapsed, Unit::Samples);
        b.thread("t").unwrap();
        assert!(matches!(b.finish(), Err(ProfileError::Empty)));
    }

    #[test]
    fn weights_are_clamped() {
        assert_eq!(scaled_weight(-1.0, 1.0), 0);
        assert_eq!(scaled_weight(f64::NAN, 1.0), 0);
        assert_eq!(scaled_weight(1e300, 1e9), u64::MAX);
        assert_eq!(scaled_weight(0.25, 1e9), 250_000_000);
    }
}
