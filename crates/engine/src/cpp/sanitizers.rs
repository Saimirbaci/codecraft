//! Sanitizer reports (track CL): AddressSanitizer, LeakSanitizer, ThreadSanitizer,
//! UndefinedBehaviorSanitizer and MemorySanitizer output in a Run tab becomes structured reports
//! (CLion's Sanitizer tab): the error kind, its message, and stacks (the access, where the memory
//! was allocated or freed, the other thread) whose frames link to source. A C/C++ run
//! configuration can add `-fsanitize=…` to its compile (`cpp.runConfig.setSanitizers`), and the
//! built-in CMake profiles can too (`cmake.setSanitizers`).
//!
//! Parsing is pure and line based ([`parse`]); [`pump`] parses a Run tab's console once its
//! process ends if a sanitizer marker appeared in it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_workspace::{ConsoleKind, RunStatus, RunTabId};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::params::{opt_usize, req_str, req_usize};
use crate::{CommandError, Session};

/// Reports kept per run.
const MAX_REPORTS: usize = 500;
/// Frames kept per stack.
const MAX_FRAMES: usize = 200;
/// Console lines scanned per run.
const MAX_LINES: usize = 200_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sanitizer {
    Address,
    Leak,
    Thread,
    UndefinedBehavior,
    Memory,
    HwAddress,
}

impl Sanitizer {
    pub fn label(self) -> &'static str {
        match self {
            Self::Address => "AddressSanitizer",
            Self::Leak => "LeakSanitizer",
            Self::Thread => "ThreadSanitizer",
            Self::UndefinedBehavior => "UndefinedBehaviorSanitizer",
            Self::Memory => "MemorySanitizer",
            Self::HwAddress => "HWAddressSanitizer",
        }
    }
}

/// A source position (0-based line and column).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLocation {
    /// As printed.
    pub written: String,
    /// Resolved (absolute when a base was known).
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub index: usize,
    pub function: String,
    pub location: Option<SourceLocation>,
    /// `(libc.so.6+0x29d8f)`.
    pub module: Option<String>,
    /// The file exists in the project (others are dimmed).
    pub project: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    /// `WRITE of size 4 at 0x… thread T0`, `freed by thread T0 here`, `Previous write …`.
    pub title: String,
    pub frames: Vec<Frame>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub sanitizer: Sanitizer,
    /// `heap-buffer-overflow`, `heap-use-after-free`, `data race`, `Direct leak`,
    /// `signed-integer-overflow`, …
    pub kind: String,
    /// The report's headline.
    pub message: String,
    pub sections: Vec<Section>,
    /// Other lines (`0x… is located 0 bytes to the right of 4-byte region`).
    pub details: Vec<String>,
    pub summary: Option<String>,
    /// Where the problem most likely is: the first project frame, else the first located one
    /// (UBSan: the location printed before `runtime error:`).
    pub location: Option<SourceLocation>,
    /// The console line the report starts at (absolute index).
    pub line: usize,
}

fn re(cell: &'static OnceLock<Option<Regex>>, pattern: &str) -> Option<&'static Regex> {
    cell.get_or_init(|| Regex::new(pattern).ok()).as_ref()
}

fn frame_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    re(&RE, r"^\s*#(\d+)\s+(?:0x[0-9a-fA-F]+\s*)?(?:in\s+)?(.*)$")
}

fn location_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    re(&RE, r"^(.+?\.[A-Za-z0-9+]+):(\d+)(?::(\d+))?$")
}

/// A `file:line[:col]` token.
fn parse_location(token: &str, base: Option<&Path>) -> Option<SourceLocation> {
    let c = location_re()?.captures(token.trim())?;
    let written = c.get(1)?.as_str();
    let line: usize = c.get(2)?.as_str().parse().ok()?;
    let column: usize = c.get(3).and_then(|m| m.as_str().parse().ok()).unwrap_or(1);
    let p = PathBuf::from(written);
    let path = match base {
        Some(b) if p.is_relative() => crate::normalize_path(&b.join(&p)),
        _ => p,
    };
    Some(SourceLocation {
        written: written.to_string(),
        path,
        line: line.saturating_sub(1),
        column: column.saturating_sub(1),
    })
}

/// `#1 0x55d in main /proj/main.c:4:7`, `#0 increment race.cc:4 (race+0xa1b6c)`,
/// `#2 0x7f in start+0x76e (dyld:x86_64+0x…)`.
fn parse_frame(line: &str, base: Option<&Path>) -> Option<Frame> {
    let c = frame_re()?.captures(line)?;
    let index: usize = c.get(1)?.as_str().parse().ok()?;
    let mut rest = c.get(2).map_or("", |m| m.as_str()).trim().to_string();
    let mut module = None;
    if rest.ends_with(')')
        && let Some(open) = rest.rfind(" (").or_else(|| rest.starts_with('(').then_some(0))
    {
        let inner = rest.get(open..).unwrap_or("").trim().trim_start_matches('(').trim_end_matches(')').to_string();
        if inner.contains("+0x") || inner.contains('/') {
            module = Some(inner);
            rest = rest.get(..open).unwrap_or("").trim().to_string();
        }
    }
    let (function, location) = match rest.rsplit_once(' ') {
        Some((f, last)) => match parse_location(last, base) {
            Some(loc) => (f.trim().to_string(), Some(loc)),
            None => (rest.clone(), None),
        },
        None => match parse_location(&rest, base) {
            Some(loc) => (String::new(), Some(loc)),
            None => (rest.clone(), None),
        },
    };
    Some(Frame { index, function, location, module, project: false })
}

/// The start of a report: (sanitizer, kind, message).
fn report_start(line: &str) -> Option<(Sanitizer, String, String)> {
    let t = line.trim();
    for (marker, san) in [
        ("ERROR: AddressSanitizer:", Sanitizer::Address),
        ("ERROR: HWAddressSanitizer:", Sanitizer::HwAddress),
        ("ERROR: LeakSanitizer:", Sanitizer::Leak),
        ("WARNING: ThreadSanitizer:", Sanitizer::Thread),
        ("ERROR: ThreadSanitizer:", Sanitizer::Thread),
        ("WARNING: MemorySanitizer:", Sanitizer::Memory),
        ("ERROR: MemorySanitizer:", Sanitizer::Memory),
    ] {
        if let Some((_, rest)) = t.split_once(marker) {
            let message = rest.trim().to_string();
            let kind = message
                .split(" on ")
                .next()
                .unwrap_or(&message)
                .split(" (")
                .next()
                .unwrap_or(&message)
                .trim()
                .to_string();
            return Some((san, kind, message));
        }
    }
    if let Some((_, rest)) = t.split_once(": runtime error: ") {
        let message = rest.trim().to_string();
        let kind = ubsan_kind(&message);
        return Some((Sanitizer::UndefinedBehavior, kind, message));
    }
    None
}

/// A UBSan check name from its message.
fn ubsan_kind(message: &str) -> String {
    let m = message.to_ascii_lowercase();
    let kind = if m.contains("signed integer overflow") {
        "signed-integer-overflow"
    } else if m.contains("division by zero") {
        "integer-divide-by-zero"
    } else if m.contains("shift exponent") || m.starts_with("left shift") {
        "shift"
    } else if m.contains("null pointer") {
        "null"
    } else if m.contains("misaligned address") {
        "alignment"
    } else if m.contains("out of bounds") {
        "bounds"
    } else if m.contains("load of value") {
        "bool/enum"
    } else if m.contains("unsigned integer overflow") {
        "unsigned-integer-overflow"
    } else if m.contains("execution reached") {
        "unreachable"
    } else {
        "undefined-behavior"
    };
    kind.to_string()
}

fn finish(report: &mut Report) {
    if report.location.is_none() {
        report.location = report
            .sections
            .iter()
            .flat_map(|s| &s.frames)
            .find(|f| f.project)
            .or_else(|| report.sections.iter().flat_map(|s| &s.frames).find(|f| f.location.is_some()))
            .and_then(|f| f.location.clone());
    }
    report.sections.retain(|s| !s.frames.is_empty() || !s.title.is_empty());
}

/// Parse sanitizer reports out of console lines. `base` resolves relative file names;
/// `in_project` marks frames in the project's own code.
pub fn parse(lines: &[&str], base: Option<&Path>, in_project: &dyn Fn(&Path) -> bool) -> Vec<Report> {
    let mut out: Vec<Report> = Vec::new();
    let mut cur: Option<Report> = None;
    // Inside a LeakSanitizer report: each `Direct leak of …` block is its own report.
    let mut leaks = false;
    let mut after_summary = false;
    let push = |out: &mut Vec<Report>, r: Option<Report>| {
        if let Some(mut r) = r
            && out.len() < MAX_REPORTS
        {
            finish(&mut r);
            out.push(r);
        }
    };
    for (i, raw) in lines.iter().enumerate().take(MAX_LINES) {
        let line = raw.trim_end_matches('\r');
        let t = line.trim();
        if let Some((san, kind, message)) = report_start(line) {
            push(&mut out, cur.take());
            after_summary = false;
            leaks = san == Sanitizer::Leak || message.contains("detected memory leaks");
            if leaks {
                continue;
            }
            let mut r = Report {
                sanitizer: san,
                kind,
                message,
                sections: Vec::new(),
                details: Vec::new(),
                summary: None,
                location: None,
                line: i,
            };
            if san == Sanitizer::UndefinedBehavior
                && let Some((head, _)) = line.split_once(": runtime error: ")
            {
                r.location = parse_location(head.trim(), base);
            }
            cur = Some(r);
            continue;
        }
        if leaks && (t.starts_with("Direct leak of") || t.starts_with("Indirect leak of")) {
            push(&mut out, cur.take());
            let kind = if t.starts_with("Direct") { "Direct leak" } else { "Indirect leak" };
            cur = Some(Report {
                sanitizer: Sanitizer::Leak,
                kind: kind.into(),
                message: t.trim_end_matches(':').to_string(),
                sections: vec![Section { title: "allocated from".into(), frames: Vec::new() }],
                details: Vec::new(),
                summary: None,
                location: None,
                line: i,
            });
            continue;
        }
        if let Some(summary) = t.strip_prefix("SUMMARY: ") {
            if let Some(r) = cur.as_mut() {
                r.summary = Some(summary.to_string());
            } else if let Some(r) = out.last_mut().filter(|r| r.summary.is_none() && r.sanitizer == Sanitizer::Leak) {
                r.summary = Some(summary.to_string());
            }
            push(&mut out, cur.take());
            leaks = false;
            after_summary = true;
            continue;
        }
        let Some(r) = cur.as_mut() else { continue };
        if after_summary {
            continue;
        }
        if let Some(mut f) = parse_frame(line, base) {
            f.project = f.location.as_ref().is_some_and(|l| in_project(&l.path));
            if r.sections.is_empty() {
                r.sections.push(Section { title: "Stack".into(), frames: Vec::new() });
            }
            if let Some(s) = r.sections.last_mut()
                && s.frames.len() < MAX_FRAMES
            {
                s.frames.push(f);
            }
            continue;
        }
        if t.is_empty() || t.starts_with("==") && t.ends_with("==") || t.chars().all(|c| c == '=') {
            continue;
        }
        if t.starts_with("Shadow bytes") || t.starts_with("Shadow byte legend") {
            after_summary = true;
            continue;
        }
        // A heading that a stack follows: `freed by thread T0 here:`, `Previous write … :`,
        // `READ of size 4 at 0x… thread T0` (ASan's access line has no colon).
        let access = ["READ of size", "WRITE of size", "Read of size", "Write of size", "Atomic"]
            .iter()
            .any(|p| t.starts_with(p));
        if (t.ends_with(':') || access) && r.details.len() < 200 {
            r.sections.push(Section { title: t.trim_end_matches(':').to_string(), frames: Vec::new() });
        } else if r.details.len() < 200 {
            r.details.push(t.to_string());
        }
    }
    push(&mut out, cur.take());
    out
}

/// Whether a console line may start a sanitizer report (cheap pre-check).
pub fn has_marker(line: &str) -> bool {
    line.contains("Sanitizer:") || line.contains(": runtime error: ")
}

// ---------------------------------------------------------------- state

/// Reports per Run tab.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TabReports {
    pub reports: Vec<Report>,
    /// A sanitizer marker was seen (parsed when the process ends).
    #[serde(skip)]
    marker: bool,
    #[serde(skip)]
    cursor: usize,
    #[serde(skip)]
    parsed: bool,
}

#[derive(Default)]
pub struct SanitizerState {
    pub tabs: BTreeMap<u64, TabReports>,
    /// Selected (report, section, frame) in the Sanitizer view, by tab.
    pub selected: BTreeMap<u64, (usize, Option<(usize, usize)>)>,
}

impl SanitizerState {
    pub fn reports(&self, tab: RunTabId) -> &[Report] {
        self.tabs.get(&tab.0).map(|t| t.reports.as_slice()).unwrap_or(&[])
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let live: Vec<u64> = s.ws.runs.iter().map(|r| r.id.0).collect();
    s.cpp.sanitizers.tabs.retain(|k, _| live.contains(k));
    s.cpp.sanitizers.selected.retain(|k, _| live.contains(k));
    let root = s.project_root();
    for tab in &s.ws.runs {
        let entry = s.cpp.sanitizers.tabs.entry(tab.id.0).or_default();
        if entry.parsed {
            continue;
        }
        let end = tab.dropped_lines + tab.lines.len();
        if entry.cursor < end {
            let from = entry.cursor.saturating_sub(tab.dropped_lines);
            if tab.lines.get(from..).unwrap_or(&[]).iter().any(|l| l.kind != ConsoleKind::System && has_marker(&l.text)) {
                entry.marker = true;
            }
            entry.cursor = end;
        }
        if tab.status == RunStatus::Running || !entry.marker {
            continue;
        }
        entry.parsed = true;
        let text: Vec<&str> =
            tab.lines.iter().filter(|l| l.kind != ConsoleKind::System).map(|l| l.text.as_str()).collect();
        let cwd = root.clone();
        let project = root.clone();
        let in_project =
            |p: &Path| project.as_ref().is_some_and(|r| p.starts_with(r)) && p.is_file() && !p.components().any(|c| c.as_os_str() == "_deps");
        entry.reports = parse(&text, cwd.as_deref(), &in_project);
        changed = true;
    }
    changed
}

fn tab_param(s: &Session, p: &Value) -> Result<RunTabId, CommandError> {
    if let Some(t) = p.get("tab").and_then(Value::as_u64) {
        return Ok(RunTabId(t));
    }
    s.ws.active_run.ok_or_else(|| CommandError::Disabled("no run".into()))
}

fn set_run_config_sanitizers(s: &mut Session, p: &Value) -> CmdResult {
    let index = req_usize(p, "index")?;
    let value = req_str(p, "sanitizers")?.trim().to_string();
    if !value.split(',').all(|x| matches!(x.trim(), "" | "address" | "undefined" | "thread" | "leak" | "memory")) {
        return Err(CommandError::BadParams(
            "sanitizers: a comma-separated list of address, undefined, thread, leak, memory".into(),
        ));
    }
    let in_dialog = s.run_store.dialog.is_some();
    let list = match &mut s.run_store.dialog {
        Some(d) => &mut d.draft,
        None => &mut s.run_store.user,
    };
    let c = list.get_mut(index).ok_or_else(|| CommandError::BadParams(format!("no run configuration {index}")))?;
    if c.kind != crate::run_config_store::RunKind::Cpp {
        return Err(CommandError::BadParams(format!("`{}` is not a C/C++ configuration", c.name)));
    }
    c.sanitizers = value.clone();
    if !in_dialog {
        s.run_store.save().map_err(CommandError::Failed)?;
        crate::run_config_store::rebuild(s);
    }
    Ok(json!({ "index": index, "sanitizers": value }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "sanitizers.reports",
            "Sanitizer Reports",
            None,
            "{tab?: int} — the AddressSanitizer/LeakSanitizer/ThreadSanitizer/UBSan/MemorySanitizer reports of \
             a Run tab (the active one by default)",
            Context::Global,
            always,
            |s, p| {
                let tab = tab_param(s, p)?;
                Ok(json!({ "tab": tab.0, "reports": s.cpp.sanitizers.reports(tab) }))
            },
        ),
        spec(
            "sanitizers.analyze",
            "Analyze Sanitizer Output",
            None,
            "{text: string} — parse pasted sanitizer output into reports",
            Context::Global,
            always,
            |s, p| {
                let text = req_str(p, "text")?;
                let root = s.project_root();
                let lines: Vec<&str> = text.lines().collect();
                let in_project = |path: &Path| root.as_ref().is_some_and(|r| path.starts_with(r)) && path.is_file();
                Ok(json!(parse(&lines, root.as_deref(), &in_project)))
            },
        ),
        spec(
            "sanitizers.navigate",
            "Go to Sanitizer Frame",
            None,
            "{tab?: int, report: int, section?: int, frame?: int} — open a report's frame (its most likely \
             location without section/frame)",
            Context::Global,
            always,
            |s, p| {
                let tab = tab_param(s, p)?;
                let ri = req_usize(p, "report")?;
                let report = s
                    .cpp
                    .sanitizers
                    .reports(tab)
                    .get(ri)
                    .ok_or_else(|| CommandError::BadParams(format!("no report {ri}")))?;
                let loc = match (opt_usize(p, "section")?, opt_usize(p, "frame")?) {
                    (Some(si), Some(fi)) => {
                        report.sections.get(si).and_then(|x| x.frames.get(fi)).and_then(|f| f.location.clone())
                    }
                    _ => report.location.clone(),
                };
                let loc = loc.ok_or_else(|| CommandError::Failed("this frame has no source location".into()))?;
                if !loc.path.is_file() {
                    return Err(CommandError::Failed(format!("{} is not in the project", loc.written)));
                }
                let sel = match (opt_usize(p, "section")?, opt_usize(p, "frame")?) {
                    (Some(a), Some(b)) => Some((a, b)),
                    _ => None,
                };
                s.cpp.sanitizers.selected.insert(tab.0, (ri, sel));
                s.navigate_to_position(&loc.path, loc.line, loc.column)?;
                ok()
            },
        ),
        spec(
            "cpp.runConfig.setSanitizers",
            "Set Run Configuration Sanitizers",
            None,
            "{index: int, sanitizers: \"address,undefined\"|\"thread\"|\"\"} — compile a C/C++ run configuration \
             with -fsanitize=… (empty: none)",
            Context::Global,
            has_project,
            set_run_config_sanitizers,
        ),
        spec(
            "cmake.setSanitizers",
            "Set CMake Profile Sanitizers",
            None,
            "{sanitizers: \"address,undefined\"|\"\"} — add -fsanitize=… to the Debug and Release CMake profiles \
             (Reload CMake Project to apply)",
            Context::Global,
            has_project,
            |s, p| {
                let root = super::project_root(s)?;
                let value = req_str(p, "sanitizers")?.trim().to_string();
                let mut settings = super::cmake::load_settings(&root);
                settings.sanitizers = (!value.is_empty()).then(|| value.clone());
                super::cmake::save_settings(&root, &settings).map_err(CommandError::Failed)?;
                s.cpp.cmake.root = None; // reload profiles on the next pump
                s.cpp.cmake.needs_reload = true;
                Ok(json!({ "sanitizers": value }))
            },
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASAN: &str = include_str!("fixtures/asan.txt");
    const TSAN: &str = include_str!("fixtures/tsan.txt");
    const UBSAN: &str = include_str!("fixtures/ubsan.txt");
    const LSAN: &str = include_str!("fixtures/lsan.txt");

    fn run(text: &str) -> Vec<Report> {
        let lines: Vec<&str> = text.lines().collect();
        parse(&lines, Some(Path::new("/proj")), &|p| p.starts_with("/proj") && !p.ends_with("dep.c"))
    }

    #[test]
    fn address_sanitizer() {
        let r = run(ASAN);
        assert_eq!(r.len(), 1);
        let a = &r[0];
        assert_eq!((a.sanitizer, a.kind.as_str()), (Sanitizer::Address, "heap-buffer-overflow"));
        assert!(a.message.starts_with("heap-buffer-overflow on address 0x602000000014"));
        let titles: Vec<&str> = a.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, vec!["WRITE of size 4 at 0x602000000014 thread T0", "allocated by thread T0 here"]);
        let f0 = &a.sections[0].frames[0];
        assert_eq!(f0.function, "main");
        let loc = f0.location.as_ref().unwrap();
        assert_eq!((loc.path.to_str(), loc.line, loc.column), (Some("/proj/main.cpp"), 4, 6));
        assert!(f0.project);
        let lib = &a.sections[0].frames[1];
        assert_eq!(lib.function, "start+0x76e");
        assert_eq!(lib.module.as_deref(), Some("dyld:x86_64+0xfffffffffff6e41e"));
        assert!(!lib.project);
        assert_eq!(a.location.as_ref().map(|l| l.line), Some(4));
        assert!(a.details.iter().any(|d| d.contains("is located 0 bytes after 4-byte region")));
        assert_eq!(a.summary.as_deref(), Some("AddressSanitizer: heap-buffer-overflow /proj/main.cpp:5:7 in main"));
        assert_eq!(a.line, 1);
    }

    #[test]
    fn thread_sanitizer() {
        let r = run(TSAN);
        assert_eq!(r.len(), 1);
        let t = &r[0];
        assert_eq!((t.sanitizer, t.kind.as_str()), (Sanitizer::Thread, "data race"));
        let titles: Vec<&str> = t.sections.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Write of size 4 at 0x000102f2c000 by thread T1",
                "Previous write of size 4 at 0x000102f2c000 by main thread",
                "Thread T1 (tid=9339, running) created by main thread at"
            ]
        );
        let f = &t.sections[0].frames[1];
        assert_eq!(f.function, "Thread1(void*)");
        assert_eq!(f.location.as_ref().map(|l| (l.path.clone(), l.line)), Some((PathBuf::from("/proj/race.cc"), 9)));
        assert_eq!(f.module.as_deref(), Some("race+0x100003e5c"));
        assert!(t.details.iter().any(|d| d.starts_with("Location is global 'Global'")));
    }

    #[test]
    fn undefined_behavior_and_leaks() {
        let r = run(UBSAN);
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].sanitizer, r[0].kind.as_str()), (Sanitizer::UndefinedBehavior, "signed-integer-overflow"));
        assert_eq!(r[0].location.as_ref().map(|l| (l.line, l.column)), Some((6, 11)));
        assert_eq!(r[1].kind, "integer-divide-by-zero");
        assert_eq!(r[1].sections[0].frames.len(), 2);

        let l = run(LSAN);
        assert_eq!(l.iter().map(|r| r.kind.as_str()).collect::<Vec<_>>(), vec!["Direct leak", "Indirect leak"]);
        assert!(l[0].message.starts_with("Direct leak of 7 byte(s) in 1 object(s)"));
        assert_eq!(l[0].sections[0].frames.len(), 3);
        // The first project frame (not the allocator, not a dependency) is the location.
        assert_eq!(l[0].location.as_ref().map(|x| x.path.clone()), Some(PathBuf::from("/proj/leak.c")));
        assert!(l[1].summary.as_deref().unwrap().contains("leaked in 2 allocation(s)"));
    }

    #[test]
    fn hostile_input_never_panics() {
        let lines = [
            "==1==ERROR: AddressSanitizer:",
            "    #",
            "    #99999999999999999999999 0x in",
            "    #1 0xzz in f :1:2",
            "    #2 (",
            "    #3 0x1 in f file.c:99999999999999999999",
            "x: runtime error: ",
            "SUMMARY: ",
            "Direct leak of",
            "WARNING: ThreadSanitizer: (",
        ];
        let _ = parse(&lines, None, &|_| true);
        let many: Vec<String> = (0..100_000).map(|i| format!("==1==ERROR: AddressSanitizer: x{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(parse(&refs, None, &|_| false).len(), MAX_REPORTS);
        assert!(has_marker("==1==ERROR: LeakSanitizer: detected memory leaks"));
        assert!(!has_marker("plain"));
    }
}
