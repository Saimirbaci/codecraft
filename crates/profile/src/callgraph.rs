//! macOS call graph reports: `sample <pid>` (CPU) and `malloc_history <memgraph> -callTree`
//! (live allocations, e.g. from `leaks --atExit --outputGraph`).
//!
//! ```text
//! Call graph:
//!     779 Thread_74254597   DispatchQueue_1: com.apple.main-thread  (serial)
//!       779 start  (in dyld) + 2360  [0x18d8620e0]
//!         720 main  (in hot) + 68  [0x102bebf20]  hot.c:21
//!         + 720 outer  (in hot) + 60  [0x102bebe38]  hot.c:10
//! ```
//!
//! Each line is `count [(size)] name  (in module) + offset  [addresses]  [file:line]`, nested by
//! indentation (`+`, `!`, `:` and `|` are guides). `malloc_history` adds the bytes in
//! parentheses after the count and starts with a `<< TOTAL >>` line. A node's self weight is its
//! weight minus its children's.

use crate::ProfileError;
use crate::model::{Format, Frame, MAX_DEPTH, Profile, ProfileBuilder, Unit};

/// Whether text looks like a `sample` / `malloc_history` report.
pub fn looks_callgraph(text: &str) -> bool {
    text.lines().take(400).any(|l| l.trim_end() == "Call graph:")
}

/// One parsed node line.
#[derive(Debug, PartialEq, Eq)]
struct Line<'a> {
    indent: usize,
    count: u64,
    size: Option<u64>,
    rest: &'a str,
}

fn node_line(line: &str) -> Option<Line<'_>> {
    let indent = line.find(|c: char| !matches!(c, ' ' | '+' | '!' | ':' | '|'))?;
    let body = line.get(indent..)?;
    let digits = body.find(|c: char| !c.is_ascii_digit()).unwrap_or(body.len());
    if digits == 0 {
        return None;
    }
    let count = body.get(..digits)?.parse::<u64>().ok()?;
    let mut rest = body.get(digits..)?.trim_start();
    let mut size = None;
    if let Some(inner) = rest.strip_prefix('(') {
        let close = inner.find(')')?;
        size = parse_size(inner.get(..close)?);
        if size.is_some() {
            rest = inner.get(close + 1..)?.trim_start();
        }
    }
    Some(Line { indent, count, size, rest })
}

/// `62.5M`, `16.0K`, `80 bytes`, `1.2G` → bytes.
pub fn parse_size(text: &str) -> Option<u64> {
    let t = text.trim();
    let (num, mult) = if let Some(n) = t.strip_suffix("bytes").or_else(|| t.strip_suffix("byte")) {
        (n.trim(), 1.0)
    } else if let Some(n) = t.strip_suffix('K').or_else(|| t.strip_suffix("KB")) {
        (n, 1024.0)
    } else if let Some(n) = t.strip_suffix('M').or_else(|| t.strip_suffix("MB")) {
        (n, 1024.0 * 1024.0)
    } else if let Some(n) = t.strip_suffix('G').or_else(|| t.strip_suffix("GB")) {
        (n, 1024.0 * 1024.0 * 1024.0)
    } else {
        (t, 1.0)
    };
    let v = num.trim().parse::<f64>().ok()?;
    Some(crate::model::scaled_weight(v, mult))
}

/// The frame of a node line's text, plus its first address.
fn frame_of(rest: &str) -> (Frame, Option<u64>) {
    let mut frame = Frame::default();
    let (name, after) = match rest.find("  (in ") {
        Some(i) => (rest.get(..i).unwrap_or(""), rest.get(i + 6..).unwrap_or("")),
        None => match rest.find("  [") {
            Some(i) => (rest.get(..i).unwrap_or(""), rest.get(i..).unwrap_or("")),
            None => (rest, ""),
        },
    };
    if let Some(close) = after.find(')')
        && rest.contains("  (in ")
    {
        frame.module = after.get(..close).map(str::to_string).filter(|m| !m.is_empty() && m != "<unknown binary>");
    }
    let mut address = None;
    if let Some(open) = after.find('[') {
        let tail = after.get(open + 1..).unwrap_or("");
        let close = tail.find(']').unwrap_or(tail.len());
        let first = tail.get(..close).unwrap_or("").split(',').next().unwrap_or("").trim();
        address = first.strip_prefix("0x").and_then(|h| u64::from_str_radix(h, 16).ok());
        let trailing = tail.get(close + 1..).unwrap_or("").trim();
        if let Some((file, line)) = trailing.rsplit_once(':')
            && let Ok(line) = line.trim().parse::<u32>()
            && !file.is_empty()
        {
            frame.file = Some(file.to_string());
            frame.line = Some(line).filter(|l| *l > 0);
        }
    }
    let name = name.trim();
    frame.name = if name.is_empty() || name == "???" {
        address.map_or_else(|| "[unknown]".to_string(), |a| format!("0x{a:x}"))
    } else {
        name.to_string()
    };
    (frame, address)
}

/// The main executable's name and load address (`Binary Images:` line marked `+`).
pub fn main_image(text: &str) -> Option<(String, u64)> {
    let mut in_images = false;
    for line in text.lines() {
        let t = line.trim();
        if t == "Binary Images:" {
            in_images = true;
            continue;
        }
        if !in_images {
            continue;
        }
        // `0x102be8000 -        0x102bebffb +hot (0) <UUID> /tmp/*/hot`
        let mut words = t.split_whitespace();
        let start = words.next()?.strip_prefix("0x").and_then(|h| u64::from_str_radix(h, 16).ok());
        let _dash = words.next();
        let _end = words.next();
        if let (Some(start), Some(name)) = (start, words.next())
            && let Some(name) = name.strip_prefix('+')
        {
            return Some((name.to_string(), start));
        }
    }
    None
}

/// The first addresses of the frames of `module` without a source line (for `atos`).
pub fn unresolved_addresses(text: &str, module: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for_each_node(text, |l| {
        let (f, addr) = frame_of(l.rest);
        if f.file.is_none()
            && f.module.as_deref() == Some(module)
            && let Some(a) = addr
            && out.len() < 100_000
        {
            out.push(a);
        }
    });
    out.sort_unstable();
    out.dedup();
    out
}

fn for_each_node(text: &str, mut f: impl FnMut(&Line)) {
    let mut started = false;
    for line in text.lines() {
        if !started {
            started = line.trim_end() == "Call graph:";
            continue;
        }
        if is_end(line) {
            break;
        }
        if let Some(l) = node_line(line) {
            f(&l);
        }
    }
}

fn is_end(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("Total number in stack") || t.starts_with("Binary Images:") || t.starts_with("Sort by top of stack")
}

/// Parse a report.
pub fn parse(text: &str) -> Result<Profile, ProfileError> {
    parse_with(text, &|_, _| None)
}

/// Resolves `(module, address)` to `(file, line)`.
pub type Lookup<'a> = dyn Fn(&str, u64) -> Option<(String, u32)> + 'a;

/// Parse a report, resolving frames without a source line through `lookup(module, address)`
/// (`atos` output: file and line).
pub fn parse_with(text: &str, lookup: &Lookup<'_>) -> Result<Profile, ProfileError> {
    if !looks_callgraph(text) {
        return Err(ProfileError::Format("no `Call graph:` section".into()));
    }
    struct Node {
        stack: Option<u32>,
        weight: u64,
        children: u64,
        thread: usize,
    }
    let mut b = ProfileBuilder::new(Format::CallGraph, Unit::Samples);
    let mut nodes: Vec<Node> = Vec::new();
    // (indent, node index or None for a thread header)
    let mut path: Vec<(usize, Option<usize>)> = Vec::new();
    let mut thread: Option<usize> = None;
    let mut bytes = false;
    let mut error = None;
    for_each_node(text, |l| {
        if error.is_some() || nodes.len() >= crate::model::MAX_STACKS {
            return;
        }
        while path.last().is_some_and(|(indent, _)| *indent >= l.indent) {
            path.pop();
        }
        if l.size.is_some() {
            bytes = true;
        }
        let weight = l.size.unwrap_or(l.count);
        let rest = l.rest.trim();
        if path.is_empty() && (rest.starts_with("Thread_") || rest.starts_with("<< TOTAL >>")) {
            let name = if rest.starts_with("<<") {
                "All allocations".to_string()
            } else {
                let n = rest.split_whitespace().collect::<Vec<_>>().join(" ");
                n.trim_end_matches("(serial)").trim_end_matches("(concurrent)").trim().chars().take(80).collect()
            };
            match b.thread_named(&name) {
                Ok(t) => thread = Some(t),
                Err(e) => error = Some(e),
            }
            path.push((l.indent, None));
            return;
        }
        let t = match thread {
            Some(t) => t,
            None => match b.thread_named("All samples") {
                Ok(t) => {
                    thread = Some(t);
                    t
                }
                Err(e) => {
                    error = Some(e);
                    return;
                }
            },
        };
        let (mut frame, address) = frame_of(rest);
        if frame.file.is_none()
            && let (Some(module), Some(addr)) = (frame.module.as_deref(), address)
            && let Some((file, line)) = lookup(module, addr)
        {
            frame.file = Some(file);
            frame.line = Some(line).filter(|l| *l > 0);
        }
        let parent = path.last().and_then(|(_, n)| *n);
        if let Some(p) = parent.and_then(|p| nodes.get_mut(p)) {
            p.children = p.children.saturating_add(weight);
        }
        let parent_stack = parent.and_then(|p| nodes.get(p)).and_then(|n| n.stack);
        let depth = path.len();
        let stack = if depth >= MAX_DEPTH {
            parent_stack
        } else {
            match b.frame(frame).and_then(|f| b.stack(parent_stack, f)) {
                Ok(s) => Some(s),
                Err(e) => {
                    error = Some(e);
                    return;
                }
            }
        };
        nodes.push(Node { stack, weight, children: 0, thread: t });
        path.push((l.indent, Some(nodes.len() - 1)));
    });
    if let Some(e) = error {
        return Err(e);
    }
    if bytes {
        b.set_unit(Unit::Bytes);
    }
    for n in &nodes {
        let own = n.weight.saturating_sub(n.children);
        if own > 0
            && let Some(s) = n.stack
        {
            b.sample(n.thread, s, own);
        }
    }
    if bytes {
        b.note("Memory still allocated when the report was taken (live allocations by call stack)");
    }
    let p = b.finish()?;
    Ok(crate::model::fold_lines(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!("../tests/fixtures/sample-callgraph.txt");
    const MALLOC: &str = include_str!("../tests/fixtures/malloc-history-calltree.txt");

    #[test]
    fn sample_report() {
        assert!(looks_callgraph(SAMPLE));
        let p = parse(SAMPLE).unwrap();
        assert_eq!(p.unit, Unit::Samples);
        assert_eq!(p.total(), 779);
        assert_eq!(p.threads.len(), 1);
        assert!(p.threads[0].name.starts_with("Thread_74254597"));
        let a = crate::Analysis::new(&p, None);
        let top = p.frame(a.methods[0].frame).unwrap();
        assert_eq!(top.name, "inner");
        assert_eq!(top.file.as_deref(), Some("hot.c"));
        assert_eq!(top.line, Some(5));
        assert_eq!(a.methods[0].self_weight, 779);
        assert_eq!(main_image(SAMPLE), Some(("hot".to_string(), 0x102be8000)));
    }

    #[test]
    fn malloc_history_with_atos_lookup() {
        let p = parse(MALLOC).unwrap();
        assert_eq!(p.unit, Unit::Bytes);
        assert_eq!(p.threads[0].name, "All allocations");
        let a = crate::Analysis::new(&p, None);
        let big = a.methods.iter().find(|m| p.frame(m.frame).unwrap().name == "big").unwrap();
        assert_eq!(big.total, (62.5 * 1024.0 * 1024.0) as u64);
        // `big` itself allocates nothing; malloc (its callee) does.
        assert_eq!(big.self_weight, 0);
        assert!(!p.notes.is_empty());
        let addrs = unresolved_addresses(MALLOC, "alloc");
        assert!(addrs.contains(&0x1006abe00), "{addrs:x?}");
        let lookup = |module: &str, addr: u64| {
            (module == "alloc" && addr == 0x1006abe00).then(|| ("/work/alloc.c".to_string(), 5))
        };
        let p = parse_with(MALLOC, &lookup).unwrap();
        let big = p.frames.iter().position(|f| f.name == "big").unwrap() as u32;
        assert_eq!(p.frames[big as usize].file.as_deref(), Some("/work/alloc.c"));
        assert_eq!(p.lines_of(big)[0].line, 5);
    }

    #[test]
    fn sizes_and_lines() {
        assert_eq!(parse_size("80 bytes"), Some(80));
        assert_eq!(parse_size("16.0K"), Some(16384));
        assert_eq!(parse_size("1G"), Some(1 << 30));
        assert_eq!(parse_size("x"), None);
        let l = node_line("      + ! 1000 (62.5M) big  (in alloc) + 28  [0x1006abe00]").unwrap();
        assert_eq!((l.indent, l.count, l.size.is_some()), (10, 1000, true));
        assert!(node_line("Thread_1").is_none());
        let (f, a) = frame_of("???  (in <unknown binary>)  [0x10]");
        assert_eq!((f.name.as_str(), f.module, a), ("0x10", None, Some(16)));
        let (f, _) = frame_of("inner  (in hot) + 28,108,...  [0x1,0x2,...]  hot.c:5");
        assert_eq!((f.name.as_str(), f.file.as_deref(), f.line), ("inner", Some("hot.c"), Some(5)));
    }

    #[test]
    fn hostile_reports() {
        for bad in [
            "",
            "Call graph:\n",
            "Call graph:\n  x y z\n",
            "Call graph:\n 5 (\n",
            "Call graph:\n    18446744073709551616 a\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let deep: String = (0..5000).map(|i| format!("{}1 f{i}  (in a)  [0x1]\n", " ".repeat(i + 1))).collect();
        let p = parse(&format!("Call graph:\n{deep}")).unwrap();
        assert_eq!(p.total(), 1);
        // Children heavier than their parent don't underflow.
        let p = parse("Call graph:\n  1 a  (in m)\n    5 b  (in m)\n").unwrap();
        assert_eq!(p.total(), 5);
    }
}
