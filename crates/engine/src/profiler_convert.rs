//! Turning what a profiler wrote into something `codecraft-profile` reads (M14 track PF). Runs
//! on the snapshot loader's worker thread; every tool gets a timeout.
//!
//! - `perf.data` → `perf script` text;
//! - an Instruments `.trace` → `xctrace export` of the time-profile table (XML);
//! - a `.memgraph` (`leaks --atExit`) → `malloc_history -callTree`, with the program's own
//!   frames symbolicated to file:line by `atos`;
//! - cProfile statistics → JSON by CodeCraft's helper, run by the program's interpreter;
//! - a memray capture → `memray transform speedscope`;
//! - a heaptrack capture → `heaptrack_print` collapsed stacks.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use codecraft_profile::Profile;

/// How long a converter may run.
const TIMEOUT: Duration = Duration::from_secs(300);
/// Largest converter output read into memory.
const MAX_OUTPUT: u64 = 1 << 30;
/// Addresses symbolicated with one `atos` run.
const ATOS_BATCH: usize = 2000;

/// What to run on a recording's output before parsing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Convert {
    None,
    PerfScript { perf: PathBuf },
    Xctrace { xctrace: PathBuf },
    MallocHistory { program: Option<PathBuf> },
    PStats { python: String, helper: PathBuf, cwd: PathBuf },
    Memray { python: String, memray: Option<PathBuf>, cwd: PathBuf },
    Heaptrack { print: PathBuf },
}

/// A converted snapshot: a file to parse, or a profile parsed already (with symbolication).
pub enum Converted {
    File(PathBuf),
    Parsed(PathBuf, Profile),
}

/// Run a tool with a timeout; stdout goes to `stdout_to` (or is captured, up to a cap).
fn run(program: &Path, args: &[String], cwd: Option<&Path>, stdout_to: Option<&Path>) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args).stdin(Stdio::null()).stderr(Stdio::piped());
    if let Some(dir) = cwd.filter(|d| d.is_dir()) {
        cmd.current_dir(dir);
    }
    let file;
    match stdout_to {
        Some(path) => {
            file = std::fs::File::create(path).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            cmd.stdout(Stdio::from(file));
        }
        None => {
            cmd.stdout(Stdio::piped());
        }
    }
    let name = program.file_name().map_or_else(|| program.display().to_string(), |n| n.to_string_lossy().into_owned());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run {name}: {e}"))?;
    // Drain pipes on threads so a chatty tool can't block on a full pipe.
    let out_reader = child.stdout.take().map(|o| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = o.take(MAX_OUTPUT).read_to_end(&mut buf);
            buf
        })
    });
    let err_reader = child.stderr.take().map(|e| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = e.take(64 * 1024).read_to_end(&mut buf);
            buf
        })
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} took more than {} s", TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("{name}: {e}")),
        }
    };
    let out = out_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = err_reader.and_then(|h| h.join().ok()).unwrap_or_default();
    if !status.success() {
        let msg = String::from_utf8_lossy(&err).trim().lines().last().unwrap_or("").to_string();
        return Err(format!(
            "{name} failed ({status}){}",
            if msg.is_empty() { String::new() } else { format!(": {msg}") }
        ));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{suffix}", path.display()))
}

/// Convert a recording's output (on a worker thread).
pub fn convert(c: &Convert, input: &Path) -> Result<Converted, String> {
    match c {
        Convert::None => Ok(Converted::File(input.to_path_buf())),
        Convert::PerfScript { perf } => {
            let out = with_suffix(input, ".txt");
            run(perf, &["script".into(), "-i".into(), input.display().to_string()], None, Some(&out))?;
            if std::fs::metadata(&out).map_or(0, |m| m.len()) == 0 {
                return Err("perf script printed nothing (was anything recorded?)".into());
            }
            Ok(Converted::File(out))
        }
        Convert::Xctrace { xctrace } => {
            let out = with_suffix(input, ".time-profile.xml");
            let args = [
                "export".to_string(),
                "--input".into(),
                input.display().to_string(),
                "--xpath".into(),
                "/trace-toc/run[@number=\"1\"]/data/table[@schema=\"time-profile\"]".into(),
                "--output".into(),
                out.display().to_string(),
            ];
            run(xctrace, &args, None, None)?;
            if !out.is_file() {
                return Err("xctrace exported nothing (the trace has no Time Profiler table)".into());
            }
            Ok(Converted::File(out))
        }
        Convert::MallocHistory { program } => {
            let out = with_suffix(input, ".txt");
            run(Path::new("malloc_history"), &[input.display().to_string(), "-callTree".into()], None, Some(&out))?;
            let text = read_capped(&out)?;
            let lines = match program {
                Some(p) => atos_lines(&text, p),
                None => HashMap::new(),
            };
            let lookup = |_module: &str, addr: u64| lines.get(&addr).cloned();
            let profile = codecraft_profile::callgraph::parse_with(&text, &lookup).map_err(|e| e.to_string())?;
            Ok(Converted::Parsed(out, profile))
        }
        Convert::PStats { python, helper, cwd } => {
            let out = with_suffix(input, ".json");
            let args =
                [helper.display().to_string(), "pstats".into(), input.display().to_string(), out.display().to_string()];
            run(Path::new(python), &args, Some(cwd), None)?;
            Ok(Converted::File(out))
        }
        Convert::Memray { python, memray, cwd } => {
            let out = with_suffix(input, ".speedscope.json");
            let tail = [
                "transform".to_string(),
                "speedscope".into(),
                "-f".into(),
                "-o".into(),
                out.display().to_string(),
                input.display().to_string(),
            ];
            match memray {
                Some(m) => run(m, &tail, Some(cwd), None)?,
                None => {
                    let mut args = vec!["-m".to_string(), "memray".into()];
                    args.extend(tail);
                    run(Path::new(python), &args, Some(cwd), None)?
                }
            };
            Ok(Converted::File(out))
        }
        Convert::Heaptrack { print } => {
            let out = with_suffix(input, ".collapsed.txt");
            let args = [
                "-f".to_string(),
                input.display().to_string(),
                "--print-flamegraph".into(),
                out.display().to_string(),
                "--flamegraph-cost-type".into(),
                "peak".into(),
            ];
            run(print, &args, None, None)?;
            Ok(Converted::File(out))
        }
    }
}

fn read_capped(path: &Path) -> Result<String, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = Vec::new();
    f.take(MAX_OUTPUT).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// file:line of the program's own frames, via `atos -fullPath` (macOS).
fn atos_lines(text: &str, program: &Path) -> HashMap<u64, (String, u32)> {
    let mut out = HashMap::new();
    let Some((module, load)) = codecraft_profile::callgraph::main_image(text) else { return out };
    let addrs = codecraft_profile::callgraph::unresolved_addresses(text, &module);
    for chunk in addrs.chunks(ATOS_BATCH) {
        let mut args = vec![
            "-fullPath".to_string(),
            "-o".into(),
            program.display().to_string(),
            "-l".into(),
            format!("0x{load:x}"),
        ];
        args.extend(chunk.iter().map(|a| format!("0x{a:x}")));
        let Ok(text) = run(Path::new("atos"), &args, None, None) else { break };
        for (addr, line) in chunk.iter().zip(text.lines()) {
            if let Some(loc) = parse_atos_line(line) {
                out.insert(*addr, loc);
            }
        }
    }
    out
}

/// `big (in alloc) (/src/alloc.c:5)` → (`/src/alloc.c`, 5).
pub fn parse_atos_line(line: &str) -> Option<(String, u32)> {
    let line = line.trim();
    let inner = line.strip_suffix(')')?;
    let open = inner.rfind('(')?;
    let loc = inner.get(open + 1..)?;
    let (file, l) = loc.rsplit_once(':')?;
    let l = l.trim().parse::<u32>().ok()?;
    (!file.is_empty() && !file.starts_with("in ")).then(|| (file.to_string(), l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atos_lines_parse() {
        assert_eq!(parse_atos_line("big (in alloc) (/w/alloc.c:5)"), Some(("/w/alloc.c".into(), 5)));
        assert_eq!(parse_atos_line("0xdeadbeef"), None);
        assert_eq!(parse_atos_line("main (in alloc) + 12"), None);
        assert_eq!(parse_atos_line("f (in lib) (x)"), None);
        assert_eq!(parse_atos_line("()"), None);
    }

    #[test]
    fn tool_failures_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("x.data");
        std::fs::write(&input, "x").unwrap();
        let missing = Convert::PerfScript { perf: dir.path().join("no-perf") };
        assert!(convert(&missing, &input).is_err());
        let r = run(Path::new("/bin/sh"), &["-c".into(), "echo boom >&2; exit 3".into()], None, None);
        assert!(r.unwrap_err().contains("boom"));
        assert_eq!(run(Path::new("/bin/sh"), &["-c".into(), "echo hi".into()], None, None).unwrap(), "hi\n");
        assert!(matches!(convert(&Convert::None, &input), Ok(Converted::File(p)) if p == input));
    }
}
