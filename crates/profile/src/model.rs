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
}

impl Thread {
    pub fn total(&self) -> u64 {
        self.weights.iter().fold(0u64, |a, (_, w)| a.saturating_add(*w))
    }
}

/// A loaded profile.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub format: Format,
    pub unit: Unit,
    pub frames: Vec<Frame>,
    pub stacks: Vec<StackNode>,
    pub threads: Vec<Thread>,
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
}

/// Builds a [`Profile`], interning frames and stacks so identical ones are shared.
#[derive(Debug, Default)]
pub struct ProfileBuilder {
    profile: Profile,
    frame_ids: HashMap<Frame, u32>,
    stack_ids: HashMap<(Option<u32>, u32), u32>,
    weights: Vec<HashMap<u32, u64>>,
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
