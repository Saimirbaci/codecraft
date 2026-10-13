//! `perf script` output: one event per block, a header line (`comm pid/tid … :`) followed by
//! indented frames, leaf first (`addr symbol+off (dso)`), blocks separated by blank lines.

use std::path::Path;

use crate::ProfileError;
use crate::collapsed::strip_offset;
use crate::model::{Format, Frame, MAX_DEPTH, Profile, ProfileBuilder, Unit};

/// Whether text looks like `perf script` output: an unindented header followed by an indented
/// line starting with a hexadecimal address.
pub fn looks_perf_script(text: &str) -> bool {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#'));
    let (Some(header), Some(frame)) = (lines.next(), lines.next()) else { return false };
    !header.starts_with(char::is_whitespace)
        && frame.starts_with(char::is_whitespace)
        && frame.split_whitespace().next().is_some_and(|a| a.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Parse `perf script` output; each `comm (tid)` becomes a thread, each event weighs one sample.
pub fn parse(text: &str) -> Result<Profile, ProfileError> {
    let mut b = ProfileBuilder::new(Format::PerfScript, Unit::Samples);
    let mut thread: Option<usize> = None;
    let mut block: Vec<u32> = Vec::new();
    let flush = |b: &mut ProfileBuilder, thread: Option<usize>, block: &mut Vec<u32>| -> Result<(), ProfileError> {
        if let Some(t) = thread
            && !block.is_empty()
        {
            block.reverse();
            if let Some(s) = b.stack_of(block)? {
                b.sample(t, s, 1);
            }
        }
        block.clear();
        Ok(())
    };
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if line.trim().is_empty() {
            flush(&mut b, thread, &mut block)?;
            thread = None;
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            flush(&mut b, thread, &mut block)?;
            thread = Some(b.thread_named(&thread_name(line))?);
            continue;
        }
        if thread.is_some() && block.len() < MAX_DEPTH {
            block.push(b.frame(parse_frame(line.trim()))?);
        }
    }
    flush(&mut b, thread, &mut block)?;
    b.finish()
}

/// `comm pid/tid [cpu] time: …` → `comm (tid)`; comm may contain spaces.
fn thread_name(header: &str) -> String {
    let tokens: Vec<&str> = header.split_whitespace().collect();
    let is_id = |t: &str| {
        let (a, b) = t.split_once('/').unwrap_or((t, "0"));
        !a.is_empty() && a.chars().all(|c| c.is_ascii_digit()) && b.chars().all(|c| c.is_ascii_digit())
    };
    match tokens.iter().position(|t| is_id(t)) {
        Some(k) if k > 0 => {
            let comm = tokens.get(..k).map(|c| c.join(" ")).unwrap_or_default();
            let tid = tokens.get(k).map(|t| t.rsplit('/').next().unwrap_or(t)).unwrap_or("");
            format!("{comm} ({tid})")
        }
        _ => tokens.first().copied().unwrap_or("?").to_string(),
    }
}

/// `7f0a12 func+0x10 (/usr/lib/libc.so.6)` → `func` in `libc.so.6`.
fn parse_frame(line: &str) -> Frame {
    let (addr, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let rest = rest.trim();
    let (symbol, dso) = match rest.rfind(" (") {
        Some(i) if rest.ends_with(')') => (rest.get(..i).unwrap_or(""), rest.get(i + 2..rest.len() - 1)),
        _ if rest.starts_with('(') && rest.ends_with(')') => ("", rest.get(1..rest.len() - 1)),
        _ => (rest, None),
    };
    let module = dso
        .filter(|d| !d.is_empty() && *d != "[unknown]")
        .map(|d| Path::new(d).file_name().map_or_else(|| d.to_string(), |n| n.to_string_lossy().into_owned()));
    let symbol = strip_offset(symbol.trim());
    let name = if symbol.is_empty() || symbol == "[unknown]" { format!("0x{addr}") } else { symbol.to_string() };
    Frame { name, module, file: None, line: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &str = "# ========\n# captured on: x\n\
app 1234/1234 [000] 100.000001:     250000 cpu-clock:u: \n\
\t    55d4c5a0 compute+0x10 (/home/u/app/target/release/app)\n\
\t    55d4c5b0 main+0x20 (/home/u/app/target/release/app)\n\
\t    7f00aa00 __libc_start_main+0xf3 (/usr/lib/libc.so.6)\n\
\n\
tokio worker 1234/1240 [001] 100.000002:     250000 cpu-clock:u: \n\
\t    deadbeef [unknown] (/home/u/app/target/release/app)\n\
\t    55d4c5b0 main+0x20 (/home/u/app/target/release/app)\n\
\n\
app 1234/1234 [000] 100.000003:     250000 cpu-clock:u: \n\
\t    55d4c5a4 compute+0x14 (/home/u/app/target/release/app)\n\
\t    55d4c5b0 main+0x20 (/home/u/app/target/release/app)\n\
\t    7f00aa00 __libc_start_main+0xf3 (/usr/lib/libc.so.6)\n";

    #[test]
    fn parses_events_into_threads() {
        assert!(looks_perf_script(SCRIPT));
        let p = parse(SCRIPT).unwrap();
        assert_eq!(p.format, Format::PerfScript);
        let names: Vec<&str> = p.threads.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["app (1234)", "tokio worker (1240)"]);
        assert_eq!(p.threads[0].total(), 2);
        assert_eq!(p.threads[0].weights.len(), 1, "compute+0x10 and +0x14 are one function");
        let unknown = p.frames.iter().find(|f| f.name.starts_with("0x")).unwrap();
        assert_eq!(unknown.name, "0xdeadbeef");
        assert_eq!(unknown.module.as_deref(), Some("app"));
        let libc = p.frames.iter().find(|f| f.name == "__libc_start_main").unwrap();
        assert_eq!(libc.module.as_deref(), Some("libc.so.6"));
        // Root first.
        let s = p.threads[0].weights[0].0;
        let names: Vec<&str> = p.stack_frames(s).iter().map(|f| p.frames[*f as usize].name.as_str()).collect();
        assert_eq!(names, vec!["__libc_start_main", "main", "compute"]);
    }

    #[test]
    fn odd_lines_never_panic() {
        assert!(!looks_perf_script("main;a 1\n"));
        assert!(parse("\t  orphan frame\n").is_err());
        let p = parse("x\n\t1\n\t2 (\n\t3 ()\n\t4 sym (\n").unwrap();
        assert_eq!(p.total(), 1);
        assert_eq!(thread_name("   "), "?");
        assert_eq!(thread_name("123 456"), "123");
    }
}
