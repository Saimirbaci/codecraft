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
    /// A samply / Firefox Profiler JSON whose native frames may have only addresses: looked up
    /// in samply's presymbolication sidecar, then with `atos` (macOS) or `addr2line`.
    Samply { atos: Option<PathBuf>, addr2line: Option<PathBuf> },
}

/// A converted snapshot: a file to parse, or a profile parsed already (with symbolication).
pub enum Converted {
    File(PathBuf),
    Parsed(PathBuf, Box<Profile>),
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
            Ok(Converted::Parsed(out, Box::new(profile)))
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
        Convert::Samply { atos, addr2line } => samply(input, atos.as_deref(), addr2line.as_deref()),
    }
}

/// A samply snapshot, symbolicated (see [`Convert::Samply`]). Anything that isn't a Firefox
/// Profiler profile is parsed as it is.
fn samply(input: &Path, atos: Option<&Path>, addr2line: Option<&Path>) -> Result<Converted, String> {
    use codecraft_profile::gecko;
    let bytes = codecraft_profile::read_snapshot(input).map_err(|e| e.to_string())?;
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        // Not JSON after all: the loader detects the format (or says why it can't).
        Err(_) => return Ok(Converted::File(input.to_path_buf())),
    };
    if !gecko::looks_gecko(&v) {
        let p = codecraft_profile::parse(&bytes).map_err(|e| e.to_string())?;
        return Ok(Converted::Parsed(input.to_path_buf(), Box::new(p)));
    }
    let libs = gecko::unresolved_libs(&v);
    if libs.is_empty() {
        let p = gecko::parse(&v).map_err(|e| e.to_string())?;
        return Ok(Converted::Parsed(input.to_path_buf(), Box::new(p)));
    }
    let wanted: usize = libs.iter().map(|l| l.addresses.len()).sum();
    let mut symbols = gecko::Symbols::new();
    let mut how: Vec<&str> = Vec::new();
    for sidecar in sidecar_paths(input) {
        let Ok(text) = read_capped(&sidecar) else { continue };
        if let Ok(sv) = serde_json::from_str::<serde_json::Value>(&text) {
            let found = gecko::presymbolicated(&sv, &libs);
            if !found.is_empty() {
                how.push("samply's symbol file");
            }
            symbols.extend(found);
        }
    }
    let mut used_tool = None;
    for lib in &libs {
        let missing: Vec<u64> =
            lib.addresses.iter().copied().filter(|a| !symbols.contains_key(&(lib.key.clone(), *a))).collect();
        if missing.is_empty() {
            continue;
        }
        let Some(binary) = [lib.path.as_deref(), lib.debug_path.as_deref()]
            .into_iter()
            .flatten()
            .map(PathBuf::from)
            .find(|p| p.is_file())
        else {
            continue;
        };
        let found = match (atos, addr2line) {
            (Some(atos), _) => {
                used_tool = Some("atos");
                atos_symbols(atos, &binary, &missing)
            }
            (None, Some(a2l)) => {
                used_tool = Some("addr2line");
                addr2line_symbols(a2l, &binary, &missing)
            }
            (None, None) => HashMap::new(),
        };
        for (addr, sym) in found {
            symbols.insert((lib.key.clone(), addr), sym);
        }
    }
    how.extend(used_tool);
    let resolved = symbols.len();
    let (mut p, _) = gecko::parse_with_symbols(&v, &symbols).map_err(|e| e.to_string())?;
    if resolved > 0 {
        p.notes.push(format!("Symbolicated {resolved} of {wanted} frame addresses with {}", how.join(" and ")));
    } else {
        p.notes.push(format!(
            "{wanted} frame addresses have no symbols: {}",
            if atos.is_none() && addr2line.is_none() {
                "no symbolizer (atos / addr2line) was found"
            } else {
                "the libraries' files were not found or have no debug info"
            }
        ));
    }
    Ok(Converted::Parsed(input.to_path_buf(), Box::new(p)))
}

/// Where samply's `--unstable-presymbolicate` puts its symbols: `<name>.syms.json` next to the
/// profile (`profile.json.gz` → `profile.json.syms.json`, `profile.json` → `profile.syms.json`).
fn sidecar_paths(input: &Path) -> Vec<PathBuf> {
    let mut out = vec![input.with_extension("syms.json"), with_suffix(input, ".syms.json")];
    out.dedup();
    out.retain(|p| p.is_file());
    out
}

/// A load address for `atos -l` (any non-zero base works: addresses are given relative to it).
const ATOS_BASE: u64 = 0x1_0000_0000;

/// Symbols of library-relative addresses with `atos -o binary -l base` (macOS).
fn atos_symbols(atos: &Path, binary: &Path, addrs: &[u64]) -> HashMap<u64, codecraft_profile::gecko::Symbol> {
    let mut out = HashMap::new();
    for chunk in addrs.chunks(ATOS_BATCH) {
        let mut args = vec![
            "-fullPath".to_string(),
            "-o".into(),
            binary.display().to_string(),
            "-l".into(),
            format!("0x{ATOS_BASE:x}"),
        ];
        args.extend(chunk.iter().map(|a| format!("0x{:x}", a.saturating_add(ATOS_BASE))));
        let Ok(text) = run(atos, &args, None, None) else { break };
        for (addr, line) in chunk.iter().zip(text.lines()) {
            if let Some(sym) = parse_atos_symbol(line) {
                out.insert(*addr, sym);
            }
        }
    }
    out
}

/// `fib (in hot) (/src/hot.c:5)` / `main (in hot) + 12` → the symbol; bare addresses → none.
pub fn parse_atos_symbol(line: &str) -> Option<codecraft_profile::gecko::Symbol> {
    let line = line.trim();
    let (name, _) = line.split_once(" (in ")?;
    let name = name.trim();
    if name.is_empty() || codecraft_profile::is_address(name) {
        return None;
    }
    let (file, line_no) = match parse_atos_line(line) {
        Some((f, l)) => (Some(f), Some(l)),
        None => (None, None),
    };
    Some(codecraft_profile::gecko::Symbol { name: name.to_string(), file, line: line_no })
}

/// Symbols with `addr2line -f -C -e binary` (two lines per address: function, file:line).
fn addr2line_symbols(a2l: &Path, binary: &Path, addrs: &[u64]) -> HashMap<u64, codecraft_profile::gecko::Symbol> {
    let mut out = HashMap::new();
    for chunk in addrs.chunks(ATOS_BATCH) {
        let mut args = vec!["-f".to_string(), "-C".into(), "-e".into(), binary.display().to_string()];
        args.extend(chunk.iter().map(|a| format!("0x{a:x}")));
        let Ok(text) = run(a2l, &args, None, None) else { break };
        let lines: Vec<&str> = text.lines().collect();
        for (addr, pair) in chunk.iter().zip(lines.chunks(2)) {
            if let Some(sym) = parse_addr2line(pair.first().copied().unwrap_or(""), pair.get(1).copied().unwrap_or(""))
            {
                out.insert(*addr, sym);
            }
        }
    }
    out
}

/// One `addr2line -f` answer: `fib` + `/src/hot.c:5` (`??` / `??:0` when unknown).
pub fn parse_addr2line(function: &str, location: &str) -> Option<codecraft_profile::gecko::Symbol> {
    let name = function.trim();
    if name.is_empty() || name == "??" {
        return None;
    }
    let loc = location.trim();
    // `file:line (discriminator 2)`
    let loc = loc.split(" (").next().unwrap_or(loc);
    let (file, line) = match loc.rsplit_once(':') {
        Some((f, l)) if f != "??" && !f.is_empty() => (Some(f.to_string()), l.trim().parse::<u32>().ok().filter(|l| *l > 0)),
        _ => (None, None),
    };
    Some(codecraft_profile::gecko::Symbol { name: name.to_string(), file: file.filter(|_| line.is_some()), line })
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
    fn symbolizer_output_parses() {
        let s = parse_atos_symbol("fib (in hot) (/w/hot.c:5)").unwrap();
        assert_eq!((s.name.as_str(), s.file.as_deref(), s.line), ("fib", Some("/w/hot.c"), Some(5)));
        let s = parse_atos_symbol("main (in hot) + 12").unwrap();
        assert_eq!((s.name.as_str(), s.file, s.line), ("main", None, None));
        assert!(parse_atos_symbol("0x0000000100000010 (in hot)").is_none());
        assert!(parse_atos_symbol("garbage").is_none());
        assert!(parse_atos_symbol(" (in x)").is_none());
        let s = parse_addr2line("hot::fib", "/w/src/main.rs:12 (discriminator 3)").unwrap();
        assert_eq!((s.name.as_str(), s.file.as_deref(), s.line), ("hot::fib", Some("/w/src/main.rs"), Some(12)));
        let s = parse_addr2line("work", "??:0").unwrap();
        assert_eq!((s.file, s.line), (None, None));
        assert!(parse_addr2line("??", "??:?").is_none());
        assert!(parse_addr2line("", "").is_none());
        assert_eq!(parse_addr2line("f", "a.c:x").unwrap().file, None);
    }

    fn samply_fixture(dir: &Path, binary: &Path) -> PathBuf {
        let v = serde_json::json!({
            "meta": { "preprocessedProfileVersion": 47, "interval": 1.0 },
            "libs": [{ "name": "hot", "path": binary, "debugPath": binary }],
            "shared": { "stringArray": ["0x3f70", "0x3f10", "hot"] },
            "threads": [{
                "name": "main", "tid": 1,
                "samples": { "stack": [1, 1, 0], "time": [0.0, 1.0, 2.0] },
                "stackTable": { "prefix": [null, 0], "frame": [0, 1] },
                "frameTable": { "func": [0, 1], "address": [16240, 16144] },
                "funcTable": { "name": [0, 1], "resource": [0, 0] },
                "resourceTable": { "lib": [0], "name": [2] }
            }]
        });
        let path = dir.join("p.json");
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();
        path
    }

    /// samply's `--save-only` snapshot: addresses looked up with `addr2line` (a fake here).
    #[test]
    fn samply_snapshots_are_symbolicated_with_addr2line() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("hot");
        std::fs::write(&binary, "ELF").unwrap();
        let a2l = dir.path().join("addr2line");
        std::fs::write(
            &a2l,
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in 0x3f70) echo main; echo /src/hot.c:6;; 0x3f10) echo fib; echo /src/hot.c:2;; 0x*) echo '??'; echo '??:0';; esac; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&a2l, std::fs::Permissions::from_mode(0o755)).unwrap();
        let input = samply_fixture(dir.path(), &binary);
        let Converted::Parsed(_, p) = convert(&Convert::Samply { atos: None, addr2line: Some(a2l) }, &input).unwrap()
        else {
            panic!("expected a parsed profile");
        };
        let fib = p.frames.iter().find(|f| f.name == "fib").unwrap();
        assert_eq!(fib.location().as_deref(), Some("/src/hot.c:2"));
        assert!(p.frames.iter().any(|f| f.name == "main"));
        assert!(p.notes.iter().any(|n| n.contains("Symbolicated 2 of 2")), "{:?}", p.notes);
        assert!(p.has_sample_times());
        // samply's own symbol file wins over the tool, and no tool at all is reported.
        let sidecar = dir.path().join("p.syms.json");
        std::fs::write(
            &sidecar,
            r#"{"string_table":["side_main","side_fib"],"data":[{"debug_name":"hot","symbol_table":[{"rva":16128,"size":96,"symbol":1},{"rva":16240,"size":16,"symbol":0}]}]}"#,
        )
        .unwrap();
        let Converted::Parsed(_, p) = convert(&Convert::Samply { atos: None, addr2line: None }, &input).unwrap() else {
            panic!("expected a parsed profile");
        };
        assert!(p.frames.iter().any(|f| f.name == "side_fib"), "{:?}", p.frames);
        std::fs::remove_file(&sidecar).unwrap();
        let Converted::Parsed(_, p) = convert(&Convert::Samply { atos: None, addr2line: None }, &input).unwrap() else {
            panic!("expected a parsed profile");
        };
        assert!(p.notes.iter().any(|n| n.contains("no symbolizer")), "{:?}", p.notes);
        // Other JSON passes through; garbage is left to the loader.
        let other = dir.path().join("o.json");
        std::fs::write(&other, r#"{"codecraft":"tracemalloc","traces":[{"size":5,"frames":[{"name":"f"}]}]}"#).unwrap();
        assert!(matches!(convert(&Convert::Samply { atos: None, addr2line: None }, &other), Ok(Converted::Parsed(..))));
        std::fs::write(&other, "main;a 1\n").unwrap();
        assert!(matches!(convert(&Convert::Samply { atos: None, addr2line: None }, &other), Ok(Converted::File(_))));
    }

    /// The real `atos` (macOS) on a binary built here with clang: samply's library-relative
    /// addresses become function, file and line.
    #[test]
    fn samply_snapshots_are_symbolicated_with_atos() {
        let (Some(atos), Some(clang)) = (codecraft_lsp::which("atos"), codecraft_lsp::which("clang")) else {
            eprintln!("skipping: atos or clang not found");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("hot.c");
        std::fs::write(&src, "int fib(int n) {\n  if (n < 2) return n;\n  return fib(n - 1) + fib(n - 2);\n}\nint main(void) {\n  return fib(20) > 0 ? 0 : 1;\n}\n").unwrap();
        let binary = dir.path().join("hot");
        let built = Command::new(clang).args(["-g", "-O0"]).arg(&src).arg("-o").arg(&binary).status();
        if !built.is_ok_and(|s| s.success()) {
            eprintln!("skipping: clang failed");
            return;
        }
        // Library-relative addresses (from __TEXT) of fib and main, from `nm`.
        let nm = Command::new("nm").arg(&binary).output().unwrap();
        let text = String::from_utf8_lossy(&nm.stdout);
        let addr = |sym: &str| {
            text.lines()
                .find(|l| l.ends_with(&format!(" _{sym}")))
                .and_then(|l| u64::from_str_radix(l.split_whitespace().next()?, 16).ok())
                .map(|a| a - 0x1_0000_0000)
        };
        let (Some(fib), Some(main)) = (addr("fib"), addr("main")) else {
            eprintln!("skipping: nm found no symbols");
            return;
        };
        let v = serde_json::json!({
            "meta": { "preprocessedProfileVersion": 47 },
            "libs": [{ "name": "hot", "path": binary }],
            "shared": { "stringArray": [format!("0x{main:x}"), format!("0x{:x}", fib + 16), "hot"] },
            "threads": [{
                "name": "main",
                "samples": { "stack": [1, 1, 0] },
                "stackTable": { "prefix": [null, 0], "frame": [0, 1] },
                "frameTable": { "func": [0, 1], "address": [main + 8, fib + 16] },
                "funcTable": { "name": [0, 1], "resource": [0, 0] },
                "resourceTable": { "lib": [0], "name": [2] }
            }]
        });
        let input = dir.path().join("s.json");
        std::fs::write(&input, serde_json::to_vec(&v).unwrap()).unwrap();
        let Converted::Parsed(_, p) = convert(&Convert::Samply { atos: Some(atos), addr2line: None }, &input).unwrap()
        else {
            panic!("expected a parsed profile");
        };
        let f = p.frames.iter().find(|f| f.name == "fib").unwrap_or_else(|| panic!("{:?}", p.frames));
        assert!(f.file.as_deref().is_some_and(|x| x.ends_with("hot.c")), "{f:?}");
        assert!(matches!(f.line, Some(1..=3)), "{f:?}");
        assert!(p.frames.iter().any(|f| f.name == "main"));
        assert_eq!(p.total(), 3);
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
