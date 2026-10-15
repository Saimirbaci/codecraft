//! View › Tool Windows › TypeScript (M13 track RC): the project's TypeScript errors from the
//! compiler (`tsc --noEmit -p <tsconfig> --pretty false` on a worker thread: the project's
//! `node_modules/.bin/tsc`, else `tsc` from `PATH`), the `tsconfig.json` files with their main
//! options (target, module, strict, include…), and Restart TypeScript Service (the language
//! server, then a new check).

use std::path::{Path, PathBuf};
use std::time::Instant;

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_lsp::ServerKind;
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_project, ok, spec};
use crate::params::{opt_path, req_usize};
use crate::{CommandError, Session};

/// Most errors kept.
const MAX_ERRORS: usize = 5_000;
/// Largest compiler output read.
const MAX_OUTPUT: usize = 16 * 1024 * 1024;

/// One compiler diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsError {
    /// Absolute; `None` for global errors (a bad tsconfig, a missing lib).
    pub path: Option<PathBuf>,
    /// 0-based.
    pub line: usize,
    pub column: usize,
    /// `error`, `warning`, `message`.
    pub category: String,
    /// `TS2322`.
    pub code: String,
    pub message: String,
}

/// A tsconfig and its main options.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsConfigInfo {
    pub path: PathBuf,
    /// `(option, value)` of compilerOptions shown (target, module, strict, …), then `include`,
    /// `exclude`, `files`, `extends`, `references`.
    pub options: Vec<(String, String)>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct TsState {
    pub configs: Vec<TsConfigInfo>,
    /// The tsconfig checked (index into `configs`).
    pub selected: usize,
    pub errors: Vec<TsError>,
    pub running: bool,
    /// The last check: when it ended and how long it took (ms).
    pub last_duration_ms: Option<u64>,
    pub checked: bool,
    pub error: Option<String>,
    pub selected_error: Option<usize>,
    rx: Option<Receiver<Result<Vec<TsError>, String>>>,
    started: Option<Instant>,
    /// The `tsc` to run (tests point it at a fake); `None` = discover.
    pub program: Option<PathBuf>,
    /// `Session::isolate_home`: only `program`, never the user's tsc.
    pub isolated: bool,
}

impl TsState {
    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }
}

// ---------------------------------------------------------------- tsconfig

/// JSON with comments and trailing commas (tsconfig's dialect) as plain JSON.
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                }
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    // Trailing commas: `,` followed by whitespace and `}` or `]` (outside strings).
    let mut cleaned = String::with_capacity(out.len());
    let mut in_string = false;
    let mut escaped = false;
    let bytes: Vec<char> = out.chars().collect();
    for (i, &c) in bytes.iter().enumerate() {
        if in_string {
            cleaned.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        }
        if c == ',' {
            let next = bytes.get(i + 1..).and_then(|rest| rest.iter().find(|n| !n.is_whitespace()));
            if matches!(next, Some('}' | ']')) {
                continue;
            }
        }
        cleaned.push(c);
    }
    cleaned
}

fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(show).collect::<Vec<_>>().join(", "),
        Value::Object(o) => o.get("path").map(show).unwrap_or_else(|| v.to_string()),
        other => other.to_string(),
    }
}

/// The options shown for a tsconfig file's text.
pub fn config_options(text: &str) -> Result<Vec<(String, String)>, String> {
    let v: Value = serde_json::from_str(&strip_jsonc(text)).map_err(|e| format!("not valid JSON: {e}"))?;
    let mut out = Vec::new();
    if let Some(e) = v.get("extends") {
        out.push(("extends".to_string(), show(e)));
    }
    if let Some(opts) = v.get("compilerOptions").and_then(Value::as_object) {
        for key in [
            "target",
            "module",
            "moduleResolution",
            "lib",
            "jsx",
            "strict",
            "noEmit",
            "outDir",
            "rootDir",
            "baseUrl",
            "composite",
            "allowJs",
            "checkJs",
            "esModuleInterop",
            "skipLibCheck",
        ] {
            if let Some(val) = opts.get(key) {
                out.push((key.to_string(), show(val)));
            }
        }
    }
    for key in ["files", "include", "exclude", "references"] {
        if let Some(val) = v.get(key) {
            out.push((key.to_string(), show(val)));
        }
    }
    Ok(out)
}

/// `tsconfig.json`, `tsconfig.*.json` and `jsconfig.json` in the project root and its direct
/// children (`web/tsconfig.json`), root first.
pub fn find_configs(root: &Path) -> Vec<PathBuf> {
    let is_config = |p: &Path| {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        (name == "tsconfig.json"
            || name == "jsconfig.json"
            || (name.starts_with("tsconfig.") && name.ends_with(".json")))
            && p.is_file()
    };
    let list = |dir: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| is_config(p)).take(50).collect())
            .unwrap_or_default();
        v.sort_by_key(|p| (p.file_name().is_none_or(|n| n != "tsconfig.json"), p.clone()));
        v
    };
    let mut out = list(root);
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name().is_some_and(|n| n != "node_modules" && !n.to_string_lossy().starts_with('.'))
                })
                .take(200)
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    for d in dirs {
        out.extend(list(&d));
        if out.len() >= 50 {
            break;
        }
    }
    out
}

/// Whether the TypeScript tool window applies (a tsconfig in the project).
pub fn is_typescript_project(s: &Session) -> bool {
    s.project_root().is_some_and(|r| !find_configs(&r).is_empty())
}

fn load_configs(s: &mut Session) {
    let Some(root) = s.project_root() else { return };
    let previous = s.typescript.configs.get(s.typescript.selected).map(|c| c.path.clone());
    s.typescript.configs = find_configs(&root)
        .into_iter()
        .map(|path| {
            let read = std::fs::metadata(&path)
                .map_err(|e| e.to_string())
                .and_then(|m| if m.len() > 1024 * 1024 { Err("larger than 1 MB".into()) } else { Ok(()) })
                .and_then(|_| std::fs::read_to_string(&path).map_err(|e| e.to_string()))
                .and_then(|t| config_options(&t));
            match read {
                Ok(options) => TsConfigInfo { path, options, error: None },
                Err(e) => TsConfigInfo { path, options: Vec::new(), error: Some(e) },
            }
        })
        .collect();
    s.typescript.selected = previous.and_then(|p| s.typescript.configs.iter().position(|c| c.path == p)).unwrap_or(0);
}

// ---------------------------------------------------------------- tsc

/// Parse `tsc --pretty false` output: `file(line,col): error TS1234: message` plus indented
/// continuation lines, and global `error TS5083: …` lines. Relative paths are from `cwd`.
pub fn parse_tsc(text: &str, cwd: &Path) -> Vec<TsError> {
    let mut out: Vec<TsError> = Vec::new();
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(last) = out.last_mut()
                && !line.trim().is_empty()
                && last.message.len() < 64 * 1024
            {
                last.message.push('\n');
                last.message.push_str(line.trim_end());
            }
            continue;
        }
        let line = line.trim_end();
        let (location, rest) = match line.find("): ") {
            Some(i) if line.get(..i).is_some_and(|h| h.contains('(')) => {
                (line.get(..=i), line.get(i + 3..).unwrap_or(""))
            }
            _ => (None, line),
        };
        let mut words = rest.splitn(3, ' ');
        let category = words.next().unwrap_or("");
        if !matches!(category, "error" | "warning" | "message") {
            continue;
        }
        let code = words.next().unwrap_or("").trim_end_matches(':').to_string();
        if !code.starts_with("TS") {
            continue;
        }
        let message = words.next().unwrap_or("").to_string();
        let (path, pos) = match location {
            Some(loc) => {
                let open = loc.rfind('(').unwrap_or(0);
                let file = loc.get(..open).unwrap_or("");
                let nums = loc.get(open + 1..loc.len().saturating_sub(1)).unwrap_or("");
                let mut it = nums.split(',').map(|n| n.trim().parse::<usize>().unwrap_or(1));
                let (l, c) = (it.next().unwrap_or(1), it.next().unwrap_or(1));
                let p = PathBuf::from(file);
                (Some(crate::normalize_path(&if p.is_absolute() { p } else { cwd.join(p) })), (l, c))
            }
            None => (None, (1, 1)),
        };
        out.push(TsError {
            path,
            line: pos.0.saturating_sub(1),
            column: pos.1.saturating_sub(1),
            category: category.to_string(),
            code,
            message,
        });
        if out.len() >= MAX_ERRORS {
            break;
        }
    }
    out
}

/// The `tsc` to run: the project's, else `PATH`'s.
fn tsc(s: &Session, root: &Path) -> Result<PathBuf, String> {
    if let Some(p) = &s.typescript.program {
        return Ok(p.clone());
    }
    if s.typescript.isolated {
        return Err("tsc is not available in this session".into());
    }
    let local = root.join("node_modules").join(".bin").join(if cfg!(windows) { "tsc.cmd" } else { "tsc" });
    if local.is_file() {
        return Ok(local);
    }
    codecraft_lsp::which("tsc").ok_or_else(|| "TypeScript is not installed (npm install --save-dev typescript)".into())
}

/// Start a check of the selected tsconfig on a worker thread.
pub(crate) fn check(s: &mut Session) -> Result<(), CommandError> {
    let root = s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    load_configs(s);
    let config = s
        .typescript
        .configs
        .get(s.typescript.selected)
        .map(|c| c.path.clone())
        .ok_or_else(|| CommandError::Failed("the project has no tsconfig.json".into()))?;
    let program = tsc(s, &root).map_err(CommandError::Failed)?;
    let cwd = config.parent().map(Path::to_path_buf).unwrap_or(root);
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("codecraft-tsc".into())
        .spawn(move || {
            let out = std::process::Command::new(&program)
                .arg("--noEmit")
                .arg("-p")
                .arg(&config)
                .args(["--pretty", "false"])
                .current_dir(&cwd)
                .stdin(std::process::Stdio::null())
                .output();
            let r = match out {
                Ok(o) => {
                    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                    if text.len() > MAX_OUTPUT {
                        let mut end = MAX_OUTPUT;
                        while end > 0 && !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text.truncate(end);
                    }
                    let errors = parse_tsc(&text, &cwd);
                    if errors.is_empty() && !o.status.success() {
                        let err = String::from_utf8_lossy(&o.stderr);
                        let msg =
                            err.lines().chain(text.lines()).find(|l| !l.trim().is_empty()).unwrap_or("").to_string();
                        Err(if msg.is_empty() { format!("tsc failed ({})", o.status) } else { msg })
                    } else {
                        Ok(errors)
                    }
                }
                Err(e) => Err(format!("cannot run {}: {e}", program.display())),
            };
            let _ = tx.send(r);
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a worker thread: {e}")))?;
    s.typescript.rx = Some(rx);
    s.typescript.running = true;
    s.typescript.error = None;
    s.typescript.started = Some(Instant::now());
    Ok(())
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(rx) = &s.typescript.rx else { return false };
    let r = match rx.try_recv() {
        Ok(r) => r,
        Err(TryRecvError::Empty) => return false,
        Err(TryRecvError::Disconnected) => Err("the TypeScript check stopped unexpectedly".into()),
    };
    s.typescript.rx = None;
    s.typescript.running = false;
    s.typescript.checked = true;
    s.typescript.last_duration_ms =
        s.typescript.started.take().map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX));
    match r {
        Ok(errors) => {
            let n = errors.iter().filter(|e| e.category == "error").count();
            s.status = match n {
                0 => "TypeScript: no errors".into(),
                1 => "TypeScript: 1 error".into(),
                n => format!("TypeScript: {n} errors"),
            };
            s.typescript.errors = errors;
            s.typescript.selected_error = None;
        }
        Err(e) => {
            s.status = format!("TypeScript: {e}");
            s.typescript.error = Some(e);
        }
    }
    true
}

fn state(s: &Session) -> Value {
    json!({
        "configs": s.typescript.configs,
        "selected": s.typescript.selected,
        "errors": s.typescript.errors,
        "running": s.typescript.running,
        "checked": s.typescript.checked,
        "error": s.typescript.error,
        "durationMs": s.typescript.last_duration_ms,
        "server": format!("{:?}", s.server_status(ServerKind::TypeScript)),
    })
}

fn restart_service(s: &mut Session, _: &Value) -> CmdResult {
    let running = s.server_name(ServerKind::TypeScript).is_some();
    if running {
        s.restart_server(Some(ServerKind::TypeScript));
    }
    let checked = check(s).is_ok();
    s.status = match (running, checked) {
        (true, true) => "TypeScript service restarted; checking the project…".into(),
        (true, false) => "TypeScript service restarted".into(),
        (false, true) => "No TypeScript language server is running; checking the project with tsc…".into(),
        (false, false) => "No TypeScript language server is running".into(),
    };
    Ok(json!({ "restarted": running, "checking": checked }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec("view.toolWindows.typeScript", "TypeScript", None, "", Context::Global, always, |s, _| {
            s.ws.layout.toggle(ToolWindowId::TypeScript);
            if s.ws.layout.is_visible(ToolWindowId::TypeScript) {
                load_configs(s);
            }
            ok()
        }),
        spec(
            "typescript.check",
            "Check TypeScript Project",
            None,
            "{config?: path} — `tsc --noEmit -p <tsconfig>` in the background (the project's tsc, else PATH's); \
             poll typescript.state",
            Context::Global,
            has_project,
            |s, p| {
                if let Some(path) = opt_path(p, "config") {
                    let path = crate::cpp::resolve_path(s, &path);
                    load_configs(s);
                    s.typescript.selected =
                        s.typescript.configs.iter().position(|c| c.path == path).ok_or_else(|| {
                            CommandError::BadParams(format!("{} is not a tsconfig of the project", path.display()))
                        })?;
                }
                check(s)?;
                s.ws.layout.show(ToolWindowId::TypeScript);
                ok()
            },
        ),
        spec(
            "typescript.state",
            "TypeScript State",
            None,
            "→ {configs: [{path, options, error}], selected, errors: [{path, line, column, category, code, message}], running, error}",
            Context::Global,
            always,
            |s, _| Ok(state(s)),
        ),
        spec(
            "typescript.selectConfig",
            "Select tsconfig",
            None,
            "{index: int}",
            Context::Global,
            has_project,
            |s, p| {
                let i = req_usize(p, "index")?;
                if i >= s.typescript.configs.len() {
                    return Err(CommandError::BadParams(format!("no tsconfig {i}")));
                }
                s.typescript.selected = i;
                ok()
            },
        ),
        spec(
            "typescript.openError",
            "Go to TypeScript Error",
            None,
            "{index: int}",
            Context::Global,
            has_project,
            |s, p| {
                let i = req_usize(p, "index")?;
                let e = s
                    .typescript
                    .errors
                    .get(i)
                    .cloned()
                    .ok_or_else(|| CommandError::BadParams(format!("no error {i}")))?;
                s.typescript.selected_error = Some(i);
                let path = e.path.ok_or_else(|| CommandError::Failed("this error has no location".into()))?;
                s.execute("navigate.openLocation", &json!({ "path": path, "line": e.line, "column": e.column }))?;
                ok()
            },
        ),
        spec(
            "typescript.restartService",
            "Restart TypeScript Service",
            None,
            "Restart the TypeScript language server and check the project again",
            Context::Global,
            has_project,
            restart_service,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_tsc_output() {
        let text = "src/a.ts(3,7): error TS2322: Type 'string' is not assignable to type 'number'.\n\
                    src/b.ts(10,1): error TS2345: Argument of type 'X' is not assignable.\n  Property 'y' is missing in type 'X'.\n\
                    error TS5083: Cannot read file '/p/tsconfig.base.json'.\n\
                    Found 3 errors in 2 files.\n\
                    random noise\n";
        let errors = parse_tsc(text, Path::new("/p"));
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0].path.as_deref(), Some(Path::new("/p/src/a.ts")));
        assert_eq!((errors[0].line, errors[0].column, errors[0].code.as_str()), (2, 6, "TS2322"));
        assert!(errors[1].message.ends_with("Property 'y' is missing in type 'X'."));
        assert_eq!(errors[2].path, None);
        assert_eq!(errors[2].category, "error");
        assert!(parse_tsc("", Path::new("/")).is_empty());
        // A Windows-style path with parentheses in a directory name.
        let odd = parse_tsc("dir (1)/x.ts(2,3): warning TS6133: 'a' is declared but never used.\n", Path::new("/p"));
        assert_eq!(odd[0].path.as_deref(), Some(Path::new("/p/dir (1)/x.ts")));
        assert_eq!((odd[0].line, odd[0].column), (1, 2));
    }

    #[test]
    fn reads_tsconfig_with_comments_and_trailing_commas() {
        let text = r#"{
          // the base
          "extends": "./tsconfig.base.json",
          "compilerOptions": {
            "target": "ES2022", /* modern */
            "module": "NodeNext",
            "strict": true,
            "lib": ["dom", "es2022",],
            "outDir": "dist//x",
          },
          "include": ["src/**/*", "a,]b"],
        }"#;
        let opts = config_options(text).unwrap();
        let get = |k: &str| opts.iter().find(|(o, _)| o == k).map(|(_, v)| v.clone());
        assert_eq!(get("target").as_deref(), Some("ES2022"));
        assert_eq!(get("strict").as_deref(), Some("true"));
        assert_eq!(get("lib").as_deref(), Some("dom, es2022"));
        assert_eq!(get("outDir").as_deref(), Some("dist//x"));
        assert_eq!(get("include").as_deref(), Some("src/**/*, a,]b"));
        assert_eq!(get("extends").as_deref(), Some("./tsconfig.base.json"));
        assert!(config_options("{ nope").is_err());
    }

    #[test]
    fn finds_configs_root_first() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("tsconfig.build.json"), "{}").unwrap();
        std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("web")).unwrap();
        std::fs::write(root.join("web/tsconfig.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("node_modules/tsconfig.json"), "{}").unwrap();
        let found = find_configs(root);
        assert_eq!(
            found,
            vec![root.join("tsconfig.json"), root.join("tsconfig.build.json"), root.join("web/tsconfig.json")]
        );
    }
}
