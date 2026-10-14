//! Analyze Stack Trace or Thread Dump…: pasted text → lines with clickable file locations.
//!
//! Recognized: Rust panics and backtraces (`panicked at src/main.rs:12:5:`, `at ./src/main.rs:4:5`),
//! Python tracebacks (`File "x.py", line 3, in f`), Zig and sanitizer traces
//! (`/p/main.zig:10:5: 0x… in main`, `#0 0x… in main /p/a.c:5:3`), and JavaScript stacks
//! (`at f (/p/x.js:10:15)`, `file:///p/x.mjs:3:1`). Frames outside the project are dimmed.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Lines kept from a pasted trace.
pub const MAX_LINES: usize = 20_000;
/// Longest line kept (longer lines are cut).
pub const MAX_LINE_CHARS: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TraceLanguage {
    Rust,
    Python,
    Zig,
    /// AddressSanitizer / UndefinedBehaviorSanitizer / ThreadSanitizer reports (C, C++, Rust).
    Sanitizer,
    JavaScript,
    Unknown,
}

impl TraceLanguage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rust => "Rust panic",
            Self::Python => "Python traceback",
            Self::Zig => "Zig stack trace",
            Self::Sanitizer => "Sanitizer report",
            Self::JavaScript => "JavaScript stack",
            Self::Unknown => "Stack trace",
        }
    }
}

/// A file location in a line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameLink {
    /// As written in the trace.
    pub written: String,
    /// Resolved against the project (absolute).
    pub path: PathBuf,
    /// Zero-based line and char column.
    pub line: usize,
    pub column: usize,
    /// Byte range of the location within the line's text.
    pub start: usize,
    pub end: usize,
    /// The file exists (the link can be opened).
    pub exists: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceLine {
    pub text: String,
    pub link: Option<FrameLink>,
    /// A stack frame (or its location line).
    pub frame: bool,
    /// A frame in the project's own code (others are dimmed).
    pub project: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackTrace {
    pub language: TraceLanguage,
    pub lines: Vec<TraceLine>,
    /// The first project frame (where the problem most likely is).
    pub first_project_frame: Option<usize>,
    pub truncated: bool,
}

fn re(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

/// `path.ext:line[:col]` for the languages we know.
fn location_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    re(
        &RE,
        r#"(?:file://)?((?:[A-Za-z]:[\\/])?[^\s:'"()<>\[\]]+\.(?:rs|py|pyi|zig|c|cc|cpp|cxx|h|hh|hpp|hxx|m|mm|js|mjs|cjs|jsx|ts|mts|cts|tsx|go|java|kt)):(\d+)(?::(\d+))?"#,
    )
}

fn python_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    re(&RE, r#"File "([^"]+)", line (\d+)"#)
}

/// A Rust backtrace frame header: `  12: app::main` (the location follows on the next line).
fn rust_frame_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    re(&RE, r"^\s*\d+:\s+(\S.*)$")
}

pub fn detect(text: &str) -> TraceLanguage {
    if text.contains("Traceback (most recent call last)") || text.contains("File \"") && text.contains("\", line ") {
        TraceLanguage::Python
    } else if text.contains("AddressSanitizer")
        || text.contains("ThreadSanitizer")
        || text.contains("LeakSanitizer")
        || text.contains("runtime error:")
        || text.contains("UndefinedBehaviorSanitizer")
        || text.contains("MemorySanitizer")
    {
        TraceLanguage::Sanitizer
    } else if text.contains("panicked at") || text.contains("stack backtrace:") || text.contains(".rs:") {
        TraceLanguage::Rust
    } else if text.contains(".zig:") {
        TraceLanguage::Zig
    } else if text.contains(".js:") || text.contains(".ts:") || text.contains(".mjs:") || text.contains(".tsx:") {
        TraceLanguage::JavaScript
    } else {
        TraceLanguage::Unknown
    }
}

/// Resolve a path from a trace: absolute as is, else against the root (`./` stripped).
fn resolve(root: Option<&Path>, written: &str) -> PathBuf {
    let w = written.strip_prefix("file://").unwrap_or(written);
    let p = Path::new(w);
    if p.is_absolute() {
        return crate::normalize_path(p);
    }
    match root {
        Some(r) => crate::normalize_path(&r.join(p)),
        None => p.to_path_buf(),
    }
}

/// Symbols of the standard library and the runtime (dimmed even without a location).
fn is_library_symbol(sym: &str) -> bool {
    let sym = sym.trim_start_matches('<');
    [
        "std::", "core::", "alloc::", "__rust", "rust_begin_unwind", "rust_panic", "_start", "__libc", "start_thread",
        "clone", "main_thread", "__pthread", "tokio::runtime", "<unknown>",
    ]
    .iter()
    .any(|p| sym.starts_with(p))
}

fn cut(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        line.trim_end_matches('\r').to_string()
    } else {
        line.chars().take(MAX_LINE_CHARS).collect()
    }
}

/// Analyze a pasted trace. `exists` checks a resolved path (tests pass a fake file system).
pub fn analyze(text: &str, root: Option<&Path>, exists: &dyn Fn(&Path) -> bool) -> StackTrace {
    let language = detect(text);
    let total = text.lines().count();
    let mut lines: Vec<TraceLine> = Vec::new();
    for raw in text.lines().take(MAX_LINES) {
        let text = cut(raw);
        let mut link = None;
        if let Some(c) = python_re().and_then(|r| r.captures(&text))
            && let (Some(file), Some(ln)) = (c.get(1), c.get(2))
        {
            let n: usize = ln.as_str().parse().unwrap_or(0);
            let path = resolve(root, file.as_str());
            link = Some(FrameLink {
                written: file.as_str().to_string(),
                exists: exists(&path),
                path,
                line: n.saturating_sub(1),
                column: 0,
                start: c.get(0).map_or(0, |m| m.start()),
                end: ln.end(),
            });
        } else if let Some(c) = location_re().and_then(|r| r.captures(&text))
            && let (Some(file), Some(ln)) = (c.get(1), c.get(2))
        {
            let n: usize = ln.as_str().parse().unwrap_or(0);
            let col: usize = c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
            let path = resolve(root, file.as_str());
            link = Some(FrameLink {
                written: file.as_str().to_string(),
                exists: exists(&path),
                path,
                line: n.saturating_sub(1),
                column: col.saturating_sub(1),
                start: c.get(0).map_or(0, |m| m.start()),
                end: c.get(0).map_or(0, |m| m.end()),
            });
        }
        let in_project = |l: &FrameLink| l.exists && root.is_some_and(|r| l.path.starts_with(r)) && !in_dependency(&l.path);
        let project = link.as_ref().is_some_and(in_project);
        let trimmed = text.trim_start();
        let frame = link.is_some()
            || (language == TraceLanguage::Rust && rust_frame_re().is_some_and(|r| r.is_match(&text)))
            || trimmed.starts_with("at ")
            || trimmed.starts_with('#');
        lines.push(TraceLine { text, link, frame, project });
    }
    // A Rust frame header takes the project flag of its `at` line; library symbols stay dim.
    if language == TraceLanguage::Rust {
        for i in 0..lines.len() {
            let header = lines.get(i).is_some_and(|l| l.link.is_none() && l.frame);
            if !header {
                continue;
            }
            let sym = lines.get(i).and_then(|l| rust_frame_re()?.captures(&l.text)?.get(1).map(|m| m.as_str().to_string()));
            let next_project = lines.get(i + 1).filter(|n| n.link.is_some() && n.text.trim_start().starts_with("at ")).map(|n| n.project);
            if let Some(l) = lines.get_mut(i) {
                l.project = match next_project {
                    Some(p) => p,
                    None => sym.as_deref().is_some_and(|s| !is_library_symbol(s)),
                };
            }
        }
    }
    let first_project_frame = lines.iter().position(|l| l.project && l.link.is_some());
    StackTrace { language, lines, first_project_frame, truncated: total > MAX_LINES }
}

/// Library code inside the project tree (vendored crates, virtualenvs, node_modules).
fn in_dependency(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("node_modules" | "site-packages" | "dist-packages" | ".venv" | "venv" | "target" | "vendor" | ".cargo")
        )
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const ROOT: &str = "/work/app";

    fn fs(p: &Path) -> bool {
        [
            "/work/app/src/main.rs",
            "/work/app/src/lib.rs",
            "/work/app/tool.py",
            "/work/app/pkg/util.py",
            "/work/app/src/main.zig",
            "/work/app/a.c",
            "/work/app/web/x.js",
            "/work/app/web/node_modules/lib/index.js",
            "/usr/lib/python3.12/json/decoder.py",
        ]
        .iter()
        .any(|f| Path::new(f) == p)
    }

    fn run(text: &str) -> StackTrace {
        analyze(text, Some(Path::new(ROOT)), &fs)
    }

    #[test]
    fn rust_panic_with_backtrace() {
        let t = run("thread 'main' panicked at src/main.rs:12:5:\nattempt to divide by zero\nstack backtrace:\n   0: rust_begin_unwind\n             at /rustc/90b35a6/library/std/src/panicking.rs:665:5\n   1: core::panicking::panic\n             at /rustc/90b35a6/library/core/src/panicking.rs:148:5\n   2: app::div\n             at ./src/lib.rs:3:5\n   3: app::main\n             at ./src/main.rs:12:5\n   4: std::rt::lang_start\nnote: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.\n");
        assert_eq!(t.language, TraceLanguage::Rust);
        let l0 = t.lines[0].link.as_ref().unwrap();
        assert_eq!((l0.path.as_path(), l0.line, l0.column), (Path::new("/work/app/src/main.rs"), 11, 4));
        assert_eq!(&t.lines[0].text[l0.start..l0.end], "src/main.rs:12:5");
        assert!(t.lines[0].project);
        // std frames: links that do not exist, dimmed.
        let std = &t.lines[4];
        assert!(std.link.as_ref().is_some_and(|l| !l.exists));
        assert!(!std.project && std.frame);
        assert!(!t.lines[3].project, "rust_begin_unwind header");
        assert!(t.lines[7].project, "app::div header takes its at-line's flag");
        assert_eq!(t.lines[8].link.as_ref().unwrap().path, PathBuf::from("/work/app/src/lib.rs"));
        assert!(!t.lines[11].project, "std::rt::lang_start without a location");
        assert_eq!(t.first_project_frame, Some(0));
        // The old panic format.
        let t = run("thread 'main' panicked at 'boom', src/main.rs:2:5");
        assert_eq!(t.lines[0].link.as_ref().unwrap().line, 1);
    }

    #[test]
    fn python_traceback() {
        let t = run("Traceback (most recent call last):\n  File \"/work/app/tool.py\", line 10, in <module>\n    main()\n  File \"pkg/util.py\", line 3, in main\n    json.loads(s)\n  File \"/usr/lib/python3.12/json/decoder.py\", line 353, in raw_decode\n    obj, end = self.scan_once(s, idx)\njson.decoder.JSONDecodeError: Expecting value: line 1 column 1 (char 0)\n");
        assert_eq!(t.language, TraceLanguage::Python);
        let l = t.lines[1].link.as_ref().unwrap();
        assert_eq!((l.path.as_path(), l.line), (Path::new("/work/app/tool.py"), 9));
        assert_eq!(&t.lines[1].text[l.start..l.end], "File \"/work/app/tool.py\", line 10");
        assert!(t.lines[1].project);
        assert_eq!(t.lines[3].link.as_ref().unwrap().path, PathBuf::from("/work/app/pkg/util.py"));
        let lib = &t.lines[5];
        assert!(lib.link.as_ref().unwrap().exists && !lib.project, "library frames are dimmed");
        assert!(t.lines[7].link.is_none());
        assert_eq!(t.first_project_frame, Some(1));
    }

    #[test]
    fn zig_and_sanitizer_traces() {
        let t = run("thread 1 panic: index out of bounds: index 5, len 3\n/work/app/src/main.zig:7:14: 0x1048b3c in main (main)\n    return a[5];\n             ^\n/opt/zig/lib/std/start.zig:524:37: 0x1048d00 in main (main)\n");
        assert_eq!(t.language, TraceLanguage::Zig);
        assert!(t.lines[1].project);
        assert_eq!(t.lines[1].link.as_ref().unwrap().column, 13);
        assert!(!t.lines[4].project);

        let t = run("==4242==ERROR: AddressSanitizer: heap-buffer-overflow on address 0x602\n    #0 0x4f6b1a in main /work/app/a.c:5:3\n    #1 0x7f0 in __libc_start_main (/lib/x86_64-linux-gnu/libc.so.6+0x21b96)\nSUMMARY: AddressSanitizer: heap-buffer-overflow /work/app/a.c:5:3 in main\n");
        assert_eq!(t.language, TraceLanguage::Sanitizer);
        assert!(t.lines[1].project && t.lines[1].frame);
        assert!(t.lines[2].link.is_none() && t.lines[2].frame && !t.lines[2].project);
    }

    #[test]
    fn javascript_stack() {
        let t = run("TypeError: Cannot read properties of undefined (reading 'x')\n    at f (/work/app/web/x.js:10:15)\n    at file:///work/app/web/node_modules/lib/index.js:3:1\n    at node:internal/main:1:1\n");
        assert_eq!(t.language, TraceLanguage::JavaScript);
        let l = t.lines[1].link.as_ref().unwrap();
        assert_eq!((l.line, l.column), (9, 14));
        assert!(t.lines[1].project);
        let nm = &t.lines[2];
        assert_eq!(nm.link.as_ref().unwrap().path, PathBuf::from("/work/app/web/node_modules/lib/index.js"));
        assert!(!nm.project, "node_modules is not project code");
        assert!(t.lines[3].link.is_none() && t.lines[3].frame);
    }

    #[test]
    fn hostile_input() {
        let long = "x".repeat(MAX_LINE_CHARS * 3) + "src/main.rs:1:1";
        let t = run(&long);
        assert_eq!(t.lines[0].text.chars().count(), MAX_LINE_CHARS);
        assert!(t.lines[0].link.is_none());
        let many = "a\n".repeat(MAX_LINES + 10);
        let t = run(&many);
        assert!(t.truncated);
        assert_eq!(t.lines.len(), MAX_LINES);
        let t = run("é.rs:99999999999999999999999:1\nmain.rs:0:0\n\u{0}\r\n");
        assert_eq!(t.lines[0].link.as_ref().unwrap().line, 0);
        assert_eq!(t.lines[1].link.as_ref().unwrap().line, 0);
        assert_eq!(run("").lines.len(), 0);
        assert_eq!(analyze("src/main.rs:3", None, &|_| false).lines[0].link.as_ref().unwrap().path, PathBuf::from("src/main.rs"));
    }
}
