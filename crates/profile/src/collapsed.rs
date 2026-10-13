//! Collapsed stacks (`main;a;b 42`: perf script | stackcollapse, py-spy `--format raw`,
//! flamegraph tools) and DTrace stack aggregations (`ustack()` blocks ending in a count).

use crate::ProfileError;
use crate::model::{Format, Frame, Profile, ProfileBuilder, Unit, scaled_weight};

/// Parse collapsed stacks: one `frame;frame;… count` per line, root first.
pub fn parse(text: &str) -> Result<Profile, ProfileError> {
    let mut b = ProfileBuilder::new(Format::Collapsed, Unit::Samples);
    let thread = b.thread("All samples")?;
    let mut bad = 0usize;
    let mut frames: Vec<u32> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((stack, count)) = line.rsplit_once(|c: char| c.is_whitespace()) else {
            bad += 1;
            continue;
        };
        let Some(weight) = parse_count(count) else {
            bad += 1;
            continue;
        };
        frames.clear();
        for part in stack.trim_end().split(';') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            frames.push(b.frame(parse_frame(part))?);
        }
        if let Some(s) = b.stack_of(&frames)? {
            b.sample(thread, s, weight);
        }
    }
    if !b.has_samples() {
        return Err(if bad > 0 {
            ProfileError::Format("no `frame;frame;… count` lines".into())
        } else {
            ProfileError::Empty
        });
    }
    b.finish()
}

/// Whether text looks like collapsed stacks (the first content line ends in a count).
pub fn looks_collapsed(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .and_then(|l| l.rsplit_once(|c: char| c.is_whitespace()))
        .is_some_and(|(_, count)| parse_count(count).is_some())
}

fn parse_count(s: &str) -> Option<u64> {
    if let Ok(n) = s.parse::<u64>() {
        return Some(n);
    }
    let f = s.parse::<f64>().ok()?;
    Some(scaled_weight(f, 1.0))
}

/// A frame as the collapsing tools write it:
/// - `func (file.py:12)` (py-spy) → name, file and line;
/// - `lib.so`func+0x1a` (DTrace) → module and name;
/// - `func_[k]` / `_[j]` / `_[i]` (perf annotations) → kernel, JIT, inlined;
/// - `func+0x1a` → the offset is dropped.
pub fn parse_frame(text: &str) -> Frame {
    let mut name = text.trim();
    let mut frame = Frame::default();
    if let Some(open) = name.rfind(" (")
        && name.ends_with(')')
    {
        let inner = name.get(open + 2..name.len() - 1).unwrap_or("");
        if let Some((file, line)) = split_line(inner) {
            frame.file = Some(file.to_string());
            frame.line = Some(line);
            name = name.get(..open).unwrap_or(name);
        } else if let Some(lib) = inner.strip_prefix("in ") {
            frame.module = Some(lib.to_string());
            name = name.get(..open).unwrap_or(name);
        }
    }
    for (suffix, module) in [("_[k]", "[kernel]"), ("_[j]", "[jit]"), ("_[i]", ""), ("_[w]", "")] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped;
            if !module.is_empty() {
                frame.module = Some(module.to_string());
            }
        }
    }
    if let Some((module, rest)) = name.split_once('`')
        && !module.is_empty()
        && !rest.is_empty()
    {
        frame.module = Some(module.to_string());
        name = rest;
    }
    name = strip_offset(name);
    frame.name = if name.is_empty() { "[unknown]".to_string() } else { name.to_string() };
    frame
}

/// `file:line` or `file:line:column` → (file, line).
fn split_line(text: &str) -> Option<(&str, u32)> {
    let (rest, last) = text.rsplit_once(':')?;
    let last = last.parse::<u32>().ok()?;
    let (file, line) = match rest.rsplit_once(':') {
        Some((file, line)) if line.parse::<u32>().is_ok() => (file, line.parse::<u32>().ok()?),
        _ => (rest, last),
    };
    (!file.is_empty()).then_some((file, line))
}

/// `func+0x1a` → `func` (only a hexadecimal offset is dropped).
pub(crate) fn strip_offset(name: &str) -> &str {
    if let Some((base, off)) = name.rsplit_once('+')
        && !base.is_empty()
        && off.strip_prefix("0x").is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return base;
    }
    name
}

/// Whether text looks like a DTrace aggregation: indented frame lines followed by an indented count.
pub fn looks_dtrace(text: &str) -> bool {
    let mut saw_frame = false;
    for line in text.lines().take(200) {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if t.parse::<u64>().is_ok() {
            if saw_frame {
                return true;
            }
        } else if line.starts_with(char::is_whitespace) {
            saw_frame = true;
        }
    }
    false
}

/// Parse DTrace `ustack()`/`stack()` aggregation output: blocks of frames (leaf first), each
/// ended by a line holding only the count. Lines before the first indented line (the `CPU ID`
/// banner and the like) are skipped.
pub fn parse_dtrace(text: &str) -> Result<Profile, ProfileError> {
    let mut b = ProfileBuilder::new(Format::Dtrace, Unit::Samples);
    let thread = b.thread("All samples")?;
    let mut block: Vec<u32> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            block.clear();
            continue;
        }
        if let Ok(count) = t.parse::<u64>() {
            block.reverse();
            if let Some(s) = b.stack_of(&block)? {
                b.sample(thread, s, count);
            }
            block.clear();
            continue;
        }
        if block.len() < crate::model::MAX_DEPTH {
            block.push(b.frame(parse_frame(t))?);
        }
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_collapsed_lines() {
        let p = parse("# comment\nmain;a;b 3\n\nmain;a 2\nmain;a;b 1\nnot a count line\n").unwrap();
        assert_eq!(p.format, Format::Collapsed);
        assert_eq!(p.total(), 6);
        assert_eq!(p.frames.len(), 3);
        // Identical stacks merged: (main;a;b) and (main;a).
        assert_eq!(p.threads[0].weights.len(), 2);
        assert_eq!(p.sample_count(), 3);
    }

    #[test]
    fn py_spy_and_dtrace_frames() {
        let f = parse_frame("process_item (app/worker.py:42)");
        assert_eq!((f.name.as_str(), f.file.as_deref(), f.line), ("process_item", Some("app/worker.py"), Some(42)));
        let f = parse_frame("libc.so.1`memcpy+0x1a");
        assert_eq!((f.name.as_str(), f.module.as_deref()), ("memcpy", Some("libc.so.1")));
        let f = parse_frame("do_syscall_64_[k]");
        assert_eq!((f.name.as_str(), f.module.as_deref()), ("do_syscall_64", Some("[kernel]")));
        let f = parse_frame("foo (in libbar.dylib)");
        assert_eq!((f.name.as_str(), f.module.as_deref()), ("foo", Some("libbar.dylib")));
        assert_eq!(parse_frame("a+b").name, "a+b");
        assert_eq!(parse_frame("x (y)").name, "x (y)");
        let f = parse_frame("run (app.js:7:3)");
        assert_eq!((f.file.as_deref(), f.line), (Some("app.js"), Some(7)));
        assert_eq!(parse_frame("y (:3)").name, "y (:3)");
        assert_eq!(parse_frame("").name, "[unknown]");
    }

    #[test]
    fn py_spy_raw_output() {
        let text = "<module> (run.py:3);main (run.py:10);work (lib.py:5) 7\n<module> (run.py:3);main (run.py:11) 2\n";
        assert!(looks_collapsed(text));
        let p = parse(text).unwrap();
        assert_eq!(p.total(), 9);
        let work = p.frames.iter().find(|f| f.name == "work").unwrap();
        assert_eq!(work.file.as_deref(), Some("lib.py"));
    }

    #[test]
    fn dtrace_aggregation() {
        let text = [
            "CPU     ID                    FUNCTION:NAME",
            "",
            "              libsystem_kernel.dylib`write+0xa",
            "              app`main+0x20",
            "              5",
            "",
            "              app`work+0x4",
            "              app`main+0x10",
            "             12",
        ]
        .join("\n");
        let text = text.as_str();
        assert!(looks_dtrace(text));
        assert!(!looks_collapsed(text));
        let p = parse_dtrace(text).unwrap();
        assert_eq!(p.total(), 17);
        let s = p.threads[0].weights.iter().find(|(_, w)| *w == 12).unwrap().0;
        let names: Vec<&str> = p.stack_frames(s).iter().map(|f| p.frames[*f as usize].name.as_str()).collect();
        assert_eq!(names, vec!["main", "work"]);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(parse("hello world\n;;; x\n").is_err());
        assert!(parse("").is_err());
        assert!(parse_dtrace("   \n  12\n").is_err());
        let p = parse(";;;; 5\na;;b 18446744073709551615\na;;b 1\n").unwrap();
        assert_eq!(p.total(), u64::MAX);
        let deep = format!("{} 1\n", vec!["f"; 10_000].join(";"));
        let p = parse(&deep).unwrap();
        assert_eq!(p.stacks.len(), crate::model::MAX_DEPTH);
    }
}
