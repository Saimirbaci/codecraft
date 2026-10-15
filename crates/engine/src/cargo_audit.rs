//! `cargo audit` and `cargo deny` in Inspect Code (M12 track RS). When installed, they run over
//! the Cargo workspace; their JSON reports (cargo audit: one object on stdout; cargo deny: one
//! diagnostic per stderr line) become findings placed on the dependency's line in Cargo.toml
//! (else its `[[package]]` entry in Cargo.lock), with the advisory's link and, for
//! vulnerabilities of direct dependencies, a quick-fix raising the requirement to a patched
//! version. After a run, the findings also show in the Cargo.toml editors (Problems) and Alt+Enter
//! there offers "Open advisory".

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use serde_json::{Value, json};

use codecraft_text::Position;
use codecraft_workspace::{Problem, ProblemSource, Severity};

use crate::Session;
use crate::cargo_deps::{manifest, semver};
use crate::inspections::{Applicability, Finding, Fix, JobSpec, ProcOutput, TextChange, Tool, ToolRun};
use crate::popup::ActionItem;

/// Inspection key of the advisory problems shown in Cargo.toml editors.
pub const INSPECTION: &str = "cargo-advisories";

/// What kind of report an advisory is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Vulnerability,
    Unmaintained,
    Unsound,
    Yanked,
    Notice,
    /// Other cargo deny checks (licenses, bans, sources): the diagnostic code.
    Other(String),
}

impl Kind {
    fn parse(code: &str) -> Self {
        match code {
            "vulnerability" => Self::Vulnerability,
            "unmaintained" => Self::Unmaintained,
            "unsound" => Self::Unsound,
            "yanked" => Self::Yanked,
            "notice" | "informational" => Self::Notice,
            other => Self::Other(other.to_string()),
        }
    }

    fn inspection(&self, tool: Tool) -> String {
        match self {
            Self::Vulnerability => "Security vulnerability".into(),
            Self::Unmaintained => "Unmaintained crate".into(),
            Self::Unsound => "Unsound crate".into(),
            Self::Yanked => "Yanked crate version".into(),
            Self::Notice => "Advisory notice".into(),
            Self::Other(code) => format!("{}: {code}", tool.label()),
        }
    }

    fn default_severity(&self) -> Severity {
        match self {
            Self::Vulnerability => Severity::Error,
            Self::Unsound | Self::Unmaintained | Self::Yanked => Severity::Warning,
            Self::Notice | Self::Other(_) => Severity::Info,
        }
    }
}

/// One report of either tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advisory {
    pub tool: Tool,
    pub kind: Kind,
    pub id: Option<String>,
    pub package: String,
    pub version: Option<String>,
    pub title: String,
    pub description: String,
    pub url: Option<String>,
    /// Patched version requirements (`>=0.2.23`).
    pub patched: Vec<String>,
    pub solution: Option<String>,
    pub severity: Severity,
}

fn s(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string).filter(|x| !x.is_empty())
}

fn rustsec_url(id: &str) -> Option<String> {
    id.starts_with("RUSTSEC-").then(|| format!("https://rustsec.org/advisories/{id}.html"))
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn audit_entry(entry: &Value, kind: Kind) -> Option<Advisory> {
    let package = entry.get("package")?;
    let name = s(package, "name")?;
    let advisory = entry.get("advisory").filter(|a| a.is_object());
    let id = advisory.and_then(|a| s(a, "id"));
    let title = advisory.and_then(|a| s(a, "title")).unwrap_or_else(|| match &kind {
        Kind::Yanked => "this version was yanked from crates.io".into(),
        Kind::Unmaintained => "the crate is unmaintained".into(),
        _ => "advisory".into(),
    });
    let url = advisory.and_then(|a| s(a, "url")).or_else(|| id.as_deref().and_then(rustsec_url));
    let patched = string_list(entry.get("versions").and_then(|v| v.get("patched")));
    let severity = kind.default_severity();
    Some(Advisory {
        tool: Tool::CargoAudit,
        kind,
        id,
        version: s(package, "version"),
        package: name,
        title,
        description: advisory.and_then(|a| s(a, "description")).unwrap_or_default(),
        url,
        solution: None,
        patched,
        severity,
    })
}

/// Parse `cargo audit --json` output.
pub fn parse_cargo_audit(stdout: &str) -> Result<Vec<Advisory>, String> {
    let report: Value = serde_json::from_str(stdout.trim())
        .ok()
        .or_else(|| {
            stdout.lines().rev().find(|l| l.trim_start().starts_with('{')).and_then(|l| serde_json::from_str(l).ok())
        })
        .ok_or_else(|| "cargo audit printed no JSON report".to_string())?;
    let mut out = Vec::new();
    if let Some(list) = report.get("vulnerabilities").and_then(|v| v.get("list")).and_then(Value::as_array) {
        out.extend(list.iter().filter_map(|e| audit_entry(e, Kind::Vulnerability)));
    }
    if let Some(warnings) = report.get("warnings").and_then(Value::as_object) {
        for (kind, list) in warnings {
            for e in list.as_array().into_iter().flatten() {
                let k = Kind::parse(&s(e, "kind").unwrap_or_else(|| kind.clone()));
                out.extend(audit_entry(e, k));
            }
        }
    }
    Ok(out)
}

/// Parse `cargo deny --format json check` diagnostics (stderr, one JSON object per line).
pub fn parse_cargo_deny(lines: &[String]) -> Vec<Advisory> {
    let mut out = Vec::new();
    for line in lines {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
        if v.get("type").and_then(Value::as_str) != Some("diagnostic") {
            continue;
        }
        let Some(f) = v.get("fields") else { continue };
        let code = s(f, "code").unwrap_or_else(|| "diagnostic".into());
        let krate = f.get("graphs").and_then(Value::as_array).and_then(|g| g.first()).and_then(|g| g.get("Krate"));
        let advisory = f.get("advisory").filter(|a| a.is_object());
        let package =
            krate.and_then(|k| s(k, "name")).or_else(|| advisory.and_then(|a| s(a, "package"))).or_else(|| {
                f.get("labels")
                    .and_then(Value::as_array)
                    .and_then(|l| l.first())
                    .and_then(|l| s(l, "span"))
                    .and_then(|sp| sp.split_whitespace().next().map(str::to_string))
            });
        let Some(package) = package else { continue };
        let notes = string_list(f.get("notes"));
        let note =
            |prefix: &str| notes.iter().find_map(|n| n.trim().strip_prefix(prefix).map(|x| x.trim().to_string()));
        let id = advisory.and_then(|a| s(a, "id")).or_else(|| note("ID:"));
        let url = advisory
            .and_then(|a| s(a, "url"))
            .or_else(|| note("Advisory:"))
            .or_else(|| id.as_deref().and_then(rustsec_url));
        let severity = match s(f, "severity").as_deref() {
            Some("error") => Severity::Error,
            Some("warning") => Severity::Warning,
            _ => Severity::Info,
        };
        let solution = note("Solution:");
        let patched = solution
            .as_deref()
            .and_then(|sol| sol.strip_prefix("Upgrade to "))
            .map(|v| {
                v.split(" OR ")
                    .filter_map(|x| x.split_whitespace().next().map(str::to_string))
                    .filter(|x| x.chars().any(|c| c.is_ascii_digit()))
                    .collect()
            })
            .unwrap_or_default();
        let message = s(f, "message").unwrap_or_default();
        let description = advisory.and_then(|a| s(a, "description")).unwrap_or_else(|| notes.join("\n"));
        out.push(Advisory {
            tool: Tool::CargoDeny,
            kind: Kind::parse(&code),
            id,
            version: krate.and_then(|k| s(k, "version")),
            package,
            title: advisory.and_then(|a| s(a, "title")).unwrap_or(message),
            description,
            url,
            patched,
            solution,
            severity,
        });
    }
    out
}

fn norm(name: &str) -> String {
    name.to_ascii_lowercase().replace('_', "-")
}

fn position_at(text: &str, offset: usize) -> Position {
    let mut line = 0usize;
    let mut col = 0usize;
    for c in text.chars().take(offset) {
        if c == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    Position::new(line, col)
}

/// The lowest version a patched requirement allows (`>=0.2.23` → 0.2.23, `^1.4` → 1.4.0).
fn patched_version(req: &str) -> Option<semver::Version> {
    let v = req.trim().trim_start_matches(['>', '=', '^', '~', ' ']);
    let mut parts: Vec<&str> = v.split('.').collect();
    while parts.len() < 3 {
        parts.push("0");
    }
    semver::Version::parse(&parts.join("."))
}

/// Turn advisories into findings on the declaring manifests (`manifests`: path and text).
pub fn place(
    advisories: Vec<Advisory>,
    manifests: &[(PathBuf, String)],
    lock: Option<&(PathBuf, String)>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let declared: Vec<(&PathBuf, &String, Vec<manifest::Dependency>)> =
        manifests.iter().map(|(p, t)| (p, t, manifest::dependencies(t))).collect();
    for a in advisories {
        let mut placed = Vec::new();
        for (path, text, deps) in &declared {
            for d in deps.iter().filter(|d| norm(&d.package) == norm(&a.package)) {
                placed.push(((*path).clone(), text.as_str(), d.key_range.clone(), d.version.clone()));
            }
        }
        let finding_at = |path: PathBuf, start: Position, end: Position| {
            let mut f = Finding::new(a.tool, a.kind.inspection(a.tool), a.severity, path, start);
            f.end = end;
            f.code = a.id.clone();
            let version = a.version.as_deref().map(|v| format!(" {v}")).unwrap_or_default();
            let id = a.id.as_deref().map(|i| format!(" ({i})")).unwrap_or_default();
            f.message = format!("{}{version}: {}{id}", a.package, a.title);
            let mut description = a.description.clone();
            if let Some(sol) = &a.solution {
                description.push_str(&format!("\n\nSolution: {sol}"));
            } else if !a.patched.is_empty() {
                description.push_str(&format!("\n\nPatched versions: {}", a.patched.join(", ")));
            }
            if let Some(url) = &a.url {
                description.push_str(&format!("\n\n{url}"));
            }
            f.description = description.chars().take(8 * 1024).collect();
            f.url = a.url.clone();
            f
        };
        if placed.is_empty() {
            // A transitive dependency: its Cargo.lock entry, else the root manifest.
            let lock_pos = lock.and_then(|(p, t)| {
                let needle = format!("name = \"{}\"", a.package);
                let line = t.lines().position(|l| l.trim() == needle)?;
                let len = t.lines().nth(line).map(|l| l.chars().count()).unwrap_or(0);
                Some((p.clone(), Position::new(line, 0), Position::new(line, len)))
            });
            match lock_pos
                .or_else(|| manifests.first().map(|(p, _)| (p.clone(), Position::new(0, 0), Position::new(0, 0))))
            {
                Some((p, st, en)) => out.push(finding_at(p, st, en)),
                None => continue,
            }
            continue;
        }
        for (path, text, key, version) in placed {
            let mut f = finding_at(path.clone(), position_at(text, key.start), position_at(text, key.end));
            if a.kind == Kind::Vulnerability
                && let Some(field) = &version
                && let Some(target) = a.patched.iter().filter_map(|p| patched_version(p)).min()
            {
                let req = semver::updated_requirement(&field.req, &target);
                f.fixes.push(Fix {
                    title: format!("Update '{}' to \"{req}\"", a.package),
                    applicability: Applicability::MaybeIncorrect,
                    changes: vec![TextChange {
                        path: path.clone(),
                        start: position_at(text, field.range.start),
                        end: position_at(text, field.range.end),
                        new_text: req,
                    }],
                });
            }
            out.push(f);
        }
    }
    out
}

/// The installed program of a tool (`cargo-audit` / `cargo-deny`).
pub(crate) fn find(tool: Tool) -> Option<PathBuf> {
    let name = match tool {
        Tool::CargoAudit => "cargo-audit",
        Tool::CargoDeny => "cargo-deny",
        _ => return None,
    };
    codecraft_lsp::which(name).or_else(|| {
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
        let p = home.join("bin").join(name);
        p.is_file().then_some(p)
    })
}

fn read_text(path: &Path) -> Option<String> {
    crate::inspections::read_normalized(path)
}

/// Why cargo audit did not run (QA M14 #21).
pub(crate) const NETWORK_SKIPPED: &str = "advisories off: they fetch the RustSec advisory database over the network; turn on Security advisories in the inspection profile (Settings › Editor › Inspections)";

/// The arguments of cargo audit / cargo deny: advisories only when the profile allows the
/// network fetch; otherwise cargo deny runs `--offline` without the advisories check.
pub(crate) fn deny_args(tool: Tool, advisories: bool, deny_config: bool) -> Vec<String> {
    match tool {
        Tool::CargoAudit => vec!["audit".into(), "--json".into()],
        _ if !advisories => ["--offline", "--format", "json", "--color", "never", "check", "licenses", "bans", "sources"]
            .map(String::from)
            .to_vec(),
        _ => {
            let mut a: Vec<String> = ["--format", "json", "--color", "never", "check"].map(String::from).to_vec();
            if !deny_config {
                a.push("advisories".into());
            }
            a
        }
    }
}

/// Run the advisory tools for an Inspect Code job (on its worker thread).
pub(crate) fn run(
    spec: &JobSpec,
    cancel: &AtomicBool,
    progress: &dyn Fn(&str),
    tools: &mut Vec<ToolRun>,
    findings: &mut Vec<Finding>,
) -> Result<(), String> {
    let Some(root) = &spec.cargo_root else { return Ok(()) };
    let manifests_in_scope = spec
        .scope
        .files
        .iter()
        .any(|f| matches!(f.file_name().and_then(|n| n.to_str()), Some("Cargo.toml" | "Cargo.lock")));
    let project_wide = matches!(spec.scope.scope.kind, crate::inspections::ScopeKind::Project);
    if !project_wide && !manifests_in_scope {
        return Ok(());
    }
    let explicit = spec.tools.is_some();
    let wanted = |t: Tool| spec.tools.as_ref().is_none_or(|l| l.contains(&t));
    let program = |t: Tool| spec.programs.get(&t).cloned().or_else(|| find(t));
    let deny_config = root.join("deny.toml").is_file() || root.join(".deny.toml").is_file();
    // The advisory database comes from the network: only when the profile turns advisories on
    // or the tool is asked for by name (QA M14 #21). Without them, cargo deny checks licenses,
    // bans and sources offline.
    let advisories = spec.profile.advisories || explicit;
    let on = |t: Tool| spec.profile.analyzer_enabled(t);
    let mut chosen: Vec<Tool> = Vec::new();
    if explicit {
        chosen.extend([Tool::CargoAudit, Tool::CargoDeny].into_iter().filter(|t| wanted(*t)));
    } else if !advisories {
        if program(Tool::CargoDeny).is_some() && deny_config && on(Tool::CargoDeny) {
            chosen.push(Tool::CargoDeny);
        } else if let Some(t) = [Tool::CargoAudit, Tool::CargoDeny].into_iter().find(|t| program(*t).is_some() && on(*t)) {
            // Say why the installed tool did not run (not a failure).
            tools.push(ToolRun { tool: t, command_line: String::new(), outcome: NETWORK_SKIPPED.into(), ok: true });
        }
    } else if program(Tool::CargoDeny).is_some() && deny_config && on(Tool::CargoDeny) {
        chosen.push(Tool::CargoDeny);
    } else if program(Tool::CargoAudit).is_some() && on(Tool::CargoAudit) {
        chosen.push(Tool::CargoAudit);
    } else if program(Tool::CargoDeny).is_some() && on(Tool::CargoDeny) {
        chosen.push(Tool::CargoDeny);
    }
    if chosen.is_empty() {
        return Ok(());
    }
    // Manifests that can declare the crates: Cargo.toml files in the scope plus the root one.
    let mut manifest_paths: Vec<PathBuf> =
        spec.scope.files.iter().filter(|f| crate::cargo_deps::is_manifest(f)).take(500).cloned().collect();
    let root_manifest = crate::normalize_path(&root.join("Cargo.toml"));
    if !manifest_paths.contains(&root_manifest) {
        manifest_paths.insert(0, root_manifest);
    }
    let manifests: Vec<(PathBuf, String)> =
        manifest_paths.into_iter().filter_map(|p| read_text(&p).map(|t| (p, t))).collect();
    let lock_path = root.join("Cargo.lock");
    let lock = read_text(&lock_path).map(|t| (crate::normalize_path(&lock_path), t));
    for tool in chosen {
        let Some(prog) = program(tool) else {
            tools.push(ToolRun { tool, command_line: String::new(), outcome: "not installed".into(), ok: false });
            continue;
        };
        let args = deny_args(tool, advisories, deny_config);
        let command_line = format!(
            "{} {}",
            prog.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
            args.iter().map(|a| codecraft_cargo::shell_quote(a)).collect::<Vec<_>>().join(" ")
        );
        progress(&format!("Running {}…", tool.label()));
        let out = match run_capturing(&prog, &args, root, cancel) {
            Ok(o) => o,
            Err(e) if e == crate::inspections::CANCELLED => return Err(e),
            Err(e) => {
                tools.push(ToolRun { tool, command_line, outcome: format!("failed: {e}"), ok: false });
                continue;
            }
        };
        let parsed = match tool {
            Tool::CargoAudit => parse_cargo_audit(&out.stdout.join("\n")),
            _ => {
                let diags = parse_cargo_deny(&out.stderr);
                let reported = out
                    .stderr
                    .iter()
                    .any(|l| l.contains("\"type\":\"summary\"") || l.contains("\"type\": \"summary\""));
                if diags.is_empty() && !out.success && !reported { Err(out.stderr_summary()) } else { Ok(diags) }
            }
        };
        match parsed {
            Ok(advisories) => {
                let mut found = place(advisories, &manifests, lock.as_ref());
                let n = found.len();
                findings.append(&mut found);
                tools.push(ToolRun {
                    tool,
                    command_line,
                    outcome: format!("{n} problem{}", if n == 1 { "" } else { "s" }),
                    ok: true,
                });
            }
            Err(e) => {
                let detail = if out.success { e } else { format!("{e} ({})", out.stderr_summary()) };
                tools.push(ToolRun { tool, command_line, outcome: format!("failed: {detail}"), ok: false });
            }
        }
    }
    Ok(())
}

/// Run a program keeping all of stdout and stderr (cargo deny reports on stderr).
fn run_capturing(program: &Path, args: &[String], cwd: &Path, cancel: &AtomicBool) -> Result<ProcOutput, String> {
    use codecraft_cargo::RunEvent;
    use crossbeam_channel::RecvTimeoutError;
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    const MAX_LINES: usize = 200_000;
    let env = vec![("NO_COLOR".to_string(), "1".to_string()), ("CARGO_TERM_COLOR".to_string(), "never".to_string())];
    let (handle, rx) = codecraft_cargo::spawn_process(program, args, cwd, &env, false)
        .map_err(|e| format!("{}: {e}", program.display()))?;
    let mut out = ProcOutput::default();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = handle.kill();
            return Err(crate::inspections::CANCELLED.into());
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(RunEvent::Stdout(l)) if out.stdout.len() < MAX_LINES => out.stdout.push(l),
            Ok(RunEvent::Stderr(l)) if out.stderr.len() < MAX_LINES => out.stderr.push(crate::runs::strip_ansi(&l)),
            Ok(RunEvent::Error(e)) => out.stderr.push(e),
            Ok(RunEvent::Exited { code, success, .. }) => {
                out.code = code;
                out.success = success;
                return Ok(out);
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(out),
        }
    }
}

/// After Inspect Code: show the advisory findings in the Cargo.toml editors.
pub(crate) fn after_inspection(s: &mut Session) {
    let Some(results) = &s.inspections.results else { return };
    let ran = results.tools.iter().any(|t| matches!(t.tool, Tool::CargoAudit | Tool::CargoDeny));
    if !ran {
        return;
    }
    let mut by_file: std::collections::BTreeMap<PathBuf, Vec<Problem>> = std::collections::BTreeMap::new();
    for f in results.open().filter(|f| matches!(f.tool, Tool::CargoAudit | Tool::CargoDeny)) {
        by_file.entry(f.path.clone()).or_default().push(Problem {
            path: f.path.clone(),
            start: f.start,
            end: f.end,
            severity: f.severity,
            message: f.message.clone(),
            code: f.code.clone(),
            source: ProblemSource::Inspection,
        });
    }
    // Clear files that had advisories before and have none now.
    let previous: Vec<PathBuf> = std::mem::take(&mut s.rust.advisory_files);
    for p in previous {
        if !by_file.contains_key(&p) {
            s.ws.set_inspection_problems(INSPECTION, p, Vec::new());
        }
    }
    for (path, problems) in by_file {
        s.rust.advisory_files.push(path.clone());
        s.ws.set_inspection_problems(INSPECTION, path, problems);
    }
}

/// Alt+Enter on a Cargo.toml line with advisories: open each advisory.
pub(crate) fn context_actions(s: &Session, path: &Path, line: usize) -> Vec<ActionItem> {
    let Some(results) = &s.inspections.results else { return Vec::new() };
    let mut items = Vec::new();
    for f in results.open().filter(|f| matches!(f.tool, Tool::CargoAudit | Tool::CargoDeny)) {
        if f.path != path || f.start.line > line || f.end.line < line {
            continue;
        }
        if let Some(url) = &f.url {
            let title = format!("Open advisory {}", f.code.clone().unwrap_or_else(|| "page".into()));
            items.push(ActionItem {
                title: title.clone(),
                kind: Some("advisory".into()),
                payload: json!({ "title": title, "codecraftCommand": "rust.cargo.openAdvisory", "codecraftParams": { "url": url } }),
            });
        }
        for (i, fix) in f.fixes.iter().enumerate() {
            items.push(ActionItem {
                title: fix.title.clone(),
                kind: Some("quickfix".into()),
                payload: json!({
                    "title": fix.title,
                    "codecraftCommand": "inspections.fix",
                    "codecraftParams": { "id": f.id, "fix": i },
                }),
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUDIT: &str = r#"{"database":{"advisory-count":600},"lockfile":{"dependency-count":3},
"vulnerabilities":{"found":true,"count":1,"list":[{"advisory":{"id":"RUSTSEC-2020-0071","package":"time","title":"Potential segfault in the time crate","description":"Unix-like operating systems may segfault.","date":"2020-11-18","aliases":["CVE-2020-26235"],"url":"https://github.com/time-rs/time/issues/293"},"versions":{"patched":[">=0.2.23"],"unaffected":["=0.2.0"]},"package":{"name":"time","version":"0.1.45","source":"registry+https://github.com/rust-lang/crates.io-index"}}]},
"warnings":{"unmaintained":[{"kind":"unmaintained","package":{"name":"ansi_term","version":"0.12.1"},"advisory":{"id":"RUSTSEC-2021-0139","title":"ansi_term is Unmaintained","description":"no longer maintained"},"versions":null}],"yanked":[{"kind":"yanked","package":{"name":"deep","version":"1.0.0"},"advisory":null}]}}"#;

    #[test]
    fn audit_reports_parse() {
        let a = parse_cargo_audit(AUDIT).unwrap();
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].kind, Kind::Vulnerability);
        assert_eq!(a[0].id.as_deref(), Some("RUSTSEC-2020-0071"));
        assert_eq!(a[0].patched, [">=0.2.23"]);
        assert_eq!(a[0].severity, Severity::Error);
        let un = a.iter().find(|x| x.package == "ansi_term").unwrap();
        assert_eq!(un.kind, Kind::Unmaintained);
        assert_eq!(un.url.as_deref(), Some("https://rustsec.org/advisories/RUSTSEC-2021-0139.html"));
        let y = a.iter().find(|x| x.package == "deep").unwrap();
        assert_eq!((y.kind.clone(), y.id.clone()), (Kind::Yanked, None));
        assert!(parse_cargo_audit("not json").is_err());
        assert!(
            parse_cargo_audit("Fetching advisory database\n{\"vulnerabilities\":{\"list\":[]}}").unwrap().is_empty()
        );
    }

    #[test]
    fn deny_diagnostics_parse() {
        let lines: Vec<String> = [
            r#"{"fields":{"level":"WARN","message":"unable to find a config path"},"type":"log"}"#,
            r#"{"fields":{"code":"vulnerability","graphs":[{"Krate":{"name":"time","version":"0.1.45"},"parents":[]}],"labels":[{"column":1,"line":10,"message":"security vulnerability detected","span":"time 0.1.45"}],"message":"Potential segfault in the time crate","notes":["ID: RUSTSEC-2020-0071","Advisory: https://rustsec.org/advisories/RUSTSEC-2020-0071","Unix-like operating systems may segfault","Solution: Upgrade to >=0.2.23 (try `cargo update -p time`)"],"severity":"error"},"type":"diagnostic"}"#,
            r#"{"fields":{"code":"rejected","graphs":[{"Krate":{"name":"dt","version":"0.1.0"}}],"labels":[],"message":"failed to satisfy license requirements","notes":["MIT - MIT License:"],"severity":"error"},"type":"diagnostic"}"#,
            r#"{"fields":{"bans":{"errors":0}},"type":"summary"}"#,
            "garbage",
        ]
        .map(String::from)
        .to_vec();
        let d = parse_cargo_deny(&lines);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].kind, Kind::Vulnerability);
        assert_eq!(d[0].id.as_deref(), Some("RUSTSEC-2020-0071"));
        assert_eq!(d[0].url.as_deref(), Some("https://rustsec.org/advisories/RUSTSEC-2020-0071"));
        assert_eq!(d[0].patched, [">=0.2.23"]);
        assert_eq!(d[1].kind, Kind::Other("rejected".into()));
        assert_eq!(d[1].severity, Severity::Error);
    }

    #[test]
    fn advisories_land_on_the_dependency_line_with_a_fix() {
        let manifest = "[package]\nname = \"app\"\n\n[dependencies]\ntime = \"0.1\"\n".to_string();
        let lock = "[[package]]\nname = \"ansi_term\"\nversion = \"0.12.1\"\n".to_string();
        let found = place(
            parse_cargo_audit(AUDIT).unwrap(),
            &[(PathBuf::from("/p/Cargo.toml"), manifest)],
            Some(&(PathBuf::from("/p/Cargo.lock"), lock)),
        );
        assert_eq!(found.len(), 3);
        let time = found.iter().find(|f| f.code.as_deref() == Some("RUSTSEC-2020-0071")).unwrap();
        assert_eq!(time.path, PathBuf::from("/p/Cargo.toml"));
        assert_eq!((time.start.line, time.start.column, time.end.column), (4, 0, 4));
        assert!(time.message.contains("time 0.1.45: Potential segfault"));
        assert_eq!(time.fixes.len(), 1);
        assert_eq!(time.fixes[0].changes[0].new_text, "0.2");
        assert_eq!(time.inspection, "Security vulnerability");
        let ansi = found.iter().find(|f| f.code.as_deref() == Some("RUSTSEC-2021-0139")).unwrap();
        assert_eq!((ansi.path.clone(), ansi.start.line), (PathBuf::from("/p/Cargo.lock"), 1));
        let deep = found.iter().find(|f| f.message.starts_with("deep")).unwrap();
        assert_eq!((deep.path.clone(), deep.start.line), (PathBuf::from("/p/Cargo.toml"), 0));
        assert_eq!(patched_version("^1.4").map(|v| v.to_string()).as_deref(), Some("1.4.0"));
    }
}
