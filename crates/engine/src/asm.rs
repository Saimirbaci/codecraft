//! Code › Show Assembly: the machine code of the function at the caret. OWNED BY the debugger
//! track.
//!
//! Rust: `cargo asm` when cargo-show-asm is installed, else `cargo rustc -- --emit asm` into a
//! separate target directory and the function cut out of the `.s` file by its (legacy or v0)
//! mangled symbol, which spells every path segment as `<length><name>`. C and C++: `cc -S`
//! (`c++ -S`). The work runs on a worker thread; the result opens as a read-only `.s` editor tab.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_syntax::{Language, SymbolKind};
use codecraft_workspace::NotificationLevel;

use crate::commands::{CmdResult, CommandSpec, Context, always, spec};
use crate::params::{bool_or, opt_path, opt_usize};
use crate::{CommandError, Session};

/// Lines of a whole `.s` file shown when the function isn't found.
const MAX_FILE_LINES: usize = 20_000;
const MAX_FUNCTION_LINES: usize = 50_000;

/// What Show Assembly produced (also the command's result for agents).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsmOutput {
    /// The function asked for (`app::Calc::twice`).
    pub function: String,
    /// The `.s` file shown.
    pub file: PathBuf,
    /// `cargo asm`, `cargo rustc --emit asm` or `cc -S`.
    pub tool: String,
    /// The symbol the function was found under (`None`: not found; the file shows everything).
    pub symbol: Option<String>,
}

/// Show Assembly's state (on [`crate::debug::DebugState::asm`]).
#[derive(Default)]
pub struct AsmState {
    rx: Option<Receiver<Result<AsmOutput, String>>>,
    /// The last result.
    pub last: Option<AsmOutput>,
    pub error: Option<String>,
}

impl AsmState {
    /// A Show Assembly job is running.
    pub fn running(&self) -> bool {
        self.rx.is_some()
    }
}

/// How to produce the assembly (worked out on the UI thread, run on a worker).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Job {
    Rust {
        cargo: PathBuf,
        manifest: PathBuf,
        package: String,
        /// `--lib`, `--bin app`, …
        target: Vec<String>,
        /// The crate's name as rustc spells it (`my_app`).
        crate_name: String,
        target_dir: PathBuf,
        release: bool,
    },
    C {
        compiler: String,
        file: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Request {
    job: Job,
    /// Path segments ending with the function (`["Calc", "twice"]`).
    segments: Vec<String>,
    out_dir: PathBuf,
}

impl Request {
    fn function(&self) -> String {
        match &self.job {
            Job::Rust { crate_name, .. } => std::iter::once(crate_name.as_str())
                .chain(self.segments.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join("::"),
            Job::C { .. } => self.segments.join("::"),
        }
    }
}

// ---------------------------------------------------------------- the function at the caret

/// The path of the function enclosing `line` (outline items: modules, impls and the function).
pub fn function_path(language: Language, text: &str, line: usize) -> Option<Vec<String>> {
    let outline = codecraft_syntax::outline(language, text);
    let index = outline.iter().rposition(|i| i.kind == SymbolKind::Function && i.line <= line)?;
    let function = outline.get(index)?;
    let mut segments = vec![clean_name(&function.name)];
    let mut depth = function.depth;
    for item in outline.get(..index)?.iter().rev() {
        if depth == 0 {
            break;
        }
        if item.depth < depth {
            depth = item.depth;
            match item.kind {
                SymbolKind::Impl | SymbolKind::Struct | SymbolKind::Module | SymbolKind::Trait | SymbolKind::Enum => {
                    let name = impl_type(&item.name);
                    if !name.is_empty() {
                        segments.push(name);
                    }
                }
                _ => {}
            }
        }
    }
    segments.reverse();
    Some(segments)
}

/// `fn foo<T>(…)` names come as `foo` already; strip anything that isn't an identifier.
fn clean_name(name: &str) -> String {
    name.trim().chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect()
}

/// `impl Trait for Type<T>` → `Type`; `impl<T> Type<T>` → `Type`; `Type` → `Type`.
fn impl_type(name: &str) -> String {
    let name = name.trim();
    let name = name.strip_prefix("impl").map_or(name, str::trim_start);
    // Skip `<…>` generics right after `impl`.
    let name = if name.starts_with('<') { skip_generics(name).trim_start() } else { name };
    let ty = name.rsplit_once(" for ").map_or(name, |(_, ty)| ty).trim();
    let ty = ty.trim_start_matches(['&', '*']).trim_start_matches("mut ").trim();
    let last = ty.split('<').next().unwrap_or(ty).rsplit("::").next().unwrap_or(ty);
    clean_name(last)
}

fn skip_generics(text: &str) -> &str {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return text.get(i + 1..).unwrap_or("");
                }
            }
            _ => {}
        }
    }
    ""
}

// ---------------------------------------------------------------- finding the symbol

/// `<length><name>` as mangled symbols spell a path segment.
fn token(segment: &str) -> String {
    format!("{}{segment}", segment.len())
}

/// How well `symbol` matches `segments` (Rust legacy/v0 or Itanium C++ mangling): the number of
/// trailing segments found together, 0 for no match. C symbols match by name (`_f`, `f`).
fn symbol_score(symbol: &str, segments: &[String], c: bool) -> usize {
    let Some(function) = segments.last() else { return 0 };
    if c && (symbol == function || symbol.strip_prefix('_') == Some(function.as_str())) {
        return usize::MAX;
    }
    let needle = token(function);
    let mut best = 0;
    let mut from = 0;
    while let Some(found) = symbol.get(from..).and_then(|rest| rest.find(&needle)) {
        let at = from + found;
        from = at + 1;
        // `15twice_more` must not match `5twice`.
        if symbol.get(..at).and_then(|b| b.chars().last()).is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        // Count the parent segments right before it.
        let mut score = 1;
        let mut end = at;
        for parent in segments.iter().rev().skip(1) {
            let t = token(parent);
            match symbol.get(..end) {
                Some(before) if before.ends_with(&t) => {
                    score += 1;
                    end -= t.len();
                }
                _ => break,
            }
        }
        best = best.max(score);
    }
    best
}

/// A label line (`name:`) that starts a function, not a local label (`.LBB0_1:`, `Ltmp0:`).
fn label(line: &str) -> Option<&str> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    // `_triple:                ; @triple`: the label is the first word.
    let name = line.split_whitespace().next()?.strip_suffix(':')?;
    let local = name.starts_with(".L") || name.starts_with('L') || name.starts_with("ltmp") || name.starts_with('.');
    (!local && !name.is_empty()).then_some(name)
}

fn noise(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with(".cfi_")
        || t.starts_with(".loc")
        || t.starts_with(".file")
        || t.starts_with("Ltmp")
        || t.starts_with(".Ltmp")
}

/// Cut the best-matching function out of assembly text: `(symbol, lines)`.
pub fn extract_function(asm: &str, segments: &[String], c: bool) -> Option<(String, String)> {
    let lines: Vec<&str> = asm.lines().collect();
    let (start, symbol, _) = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| label(l).map(|name| (i, name, symbol_score(name, segments, c))))
        .filter(|(_, _, score)| *score > 0)
        // The best score; the first such symbol on ties.
        .fold(None::<(usize, &str, usize)>, |best, cur| match best {
            Some(b) if b.2 >= cur.2 => Some(b),
            _ => Some(cur),
        })?;
    let mut out = Vec::new();
    for line in lines.iter().skip(start).take(MAX_FUNCTION_LINES) {
        let t = line.trim();
        if out.len() > 1 && (label(line).is_some() || t.starts_with(".Lfunc_end") || t.starts_with("Lfunc_end")) {
            break;
        }
        if t == ".cfi_endproc" {
            break;
        }
        if !noise(line) {
            out.push(*line);
        }
    }
    Some((symbol.to_owned(), out.join("\n")))
}

// ---------------------------------------------------------------- running the tools

fn run(program: &Path, args: &[String], cwd: &Path) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", program.display()))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
        Err(format!("{} failed ({}):\n{}", program.display(), out.status, tail.join("\n")))
    }
}

/// A cargo subcommand's executable (`PATH`, else `$CARGO_HOME/bin` or `~/.cargo/bin`).
fn cargo_subcommand(name: &str) -> Option<PathBuf> {
    codecraft_dap::discover::which(name).or_else(|| {
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
        let path = home.join("bin").join(name);
        path.is_file().then_some(path)
    })
}

/// The newest `.s` file of `crate_name` in `dir`.
fn newest_asm(dir: &Path, crate_name: &str) -> Option<PathBuf> {
    let prefix = format!("{crate_name}-");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "s"))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix)))
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .max_by_key(|(t, _)| *t)
        .map(|(_, p)| p)
}

/// Runs on a worker thread.
fn produce(request: &Request) -> Result<AsmOutput, String> {
    let function = request.function();
    let (tool, text, c) = match &request.job {
        Job::Rust { cargo, manifest, package, target, crate_name, target_dir, release } => {
            let cwd = manifest.parent().map(Path::to_path_buf).unwrap_or_default();
            let profile = if *release { "--release" } else { "--dev" };
            // cargo-show-asm prints just the function.
            let show_asm = cargo_subcommand("cargo-asm").and_then(|_| {
                let mut args: Vec<String> = vec![
                    "asm".into(),
                    "--manifest-path".into(),
                    manifest.display().to_string(),
                    "-p".into(),
                    package.clone(),
                ];
                args.extend(target.iter().cloned());
                args.extend([profile.to_owned(), "--simplify".to_owned(), function.clone()]);
                run(cargo, &args, &cwd).ok().filter(|t| !t.trim().is_empty())
            });
            if let Some(text) = show_asm {
                let file = write_output(&request.out_dir, &function, &format!("// {function} (cargo asm)\n\n{text}"))?;
                return Ok(AsmOutput {
                    function,
                    file,
                    tool: "cargo asm".into(),
                    symbol: Some(request.segments.join("::")),
                });
            }
            let dir = target_dir.join("codecraft-asm");
            let mut args: Vec<String> = vec![
                "rustc".into(),
                "--manifest-path".into(),
                manifest.display().to_string(),
                "-p".into(),
                package.clone(),
                "--target-dir".into(),
                dir.display().to_string(),
            ];
            args.extend(target.iter().cloned());
            if *release {
                args.push("--release".into());
            }
            args.extend(["--".into(), "--emit".into(), "asm".into(), "-C".into(), "codegen-units=1".into()]);
            run(cargo, &args, &cwd)?;
            let deps = dir.join(if *release { "release" } else { "debug" }).join("deps");
            let file = newest_asm(&deps, crate_name)
                .ok_or_else(|| format!("rustc wrote no assembly to {}", deps.display()))?;
            let text = std::fs::read_to_string(&file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
            (format!("cargo rustc --emit asm ({})", if *release { "release" } else { "debug" }), text, false)
        }
        Job::C { compiler, file } => {
            std::fs::create_dir_all(&request.out_dir)
                .map_err(|e| format!("cannot create {}: {e}", request.out_dir.display()))?;
            let out = request.out_dir.join("codecraft-cc.s");
            let args: Vec<String> = vec![
                "-S".into(),
                "-O0".into(),
                "-g0".into(),
                "-fno-asynchronous-unwind-tables".into(),
                "-o".into(),
                out.display().to_string(),
                file.display().to_string(),
            ];
            let cwd = file.parent().map(Path::to_path_buf).unwrap_or_default();
            run(Path::new(compiler), &args, &cwd)?;
            let text = std::fs::read_to_string(&out).map_err(|e| format!("cannot read {}: {e}", out.display()))?;
            (format!("{compiler} -S"), text, true)
        }
    };
    let (symbol, body) = match extract_function(&text, &request.segments, c) {
        Some((symbol, body)) => (Some(symbol), body),
        None => (None, text.lines().take(MAX_FILE_LINES).collect::<Vec<_>>().join("\n")),
    };
    let header = match &symbol {
        Some(sym) => format!("// {function} ({tool})\n// symbol: {sym}\n\n"),
        None => format!(
            "// {function} was not found in the {tool} output (it may have been inlined or not used; try \
             #[inline(never)] or a debug build). The whole output follows.\n\n"
        ),
    };
    let file = write_output(&request.out_dir, &function, &format!("{header}{body}\n"))?;
    Ok(AsmOutput { function, file, tool, symbol })
}

fn write_output(dir: &Path, function: &str, text: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let name: String =
        function.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).take(120).collect();
    let file = dir.join(format!("{name}.s"));
    std::fs::write(&file, text).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(file)
}

// ---------------------------------------------------------------- command

fn output_dir() -> PathBuf {
    std::env::temp_dir().join("codecraft-asm").join(std::process::id().to_string())
}

fn request_for(s: &Session, path: &Path, line: usize, release: bool) -> Result<Request, CommandError> {
    let language = Language::from_path(path);
    let text = match s.ws.doc_by_path(path).and_then(|d| s.ws.doc(d)) {
        Some(d) => d.doc.text(),
        None => {
            codecraft_fs::read_text_file(path).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?
        }
    };
    let segments = function_path(language, &text, line)
        .filter(|segs| segs.last().is_some_and(|f| !f.is_empty()))
        .ok_or_else(|| CommandError::Failed("put the caret in a function".into()))?;
    let job = match language {
        Language::Rust => {
            let meta =
                s.ws.project
                    .as_ref()
                    .and_then(|p| p.cargo.as_ref())
                    .ok_or_else(|| CommandError::Failed("Show Assembly for Rust needs a Cargo project".into()))?;
            let package = meta
                .package_for_file(path)
                .ok_or_else(|| CommandError::Failed(format!("{} is not in a workspace package", path.display())))?;
            let target = package
                .targets
                .iter()
                .filter(|t| t.src_path.parent().is_some_and(|d| path.starts_with(d)))
                .max_by_key(|t| {
                    let exact = t.src_path == path;
                    let depth = t.src_path.parent().map(|d| d.as_os_str().len()).unwrap_or(0);
                    (exact, depth, t.is_lib())
                })
                .ok_or_else(|| CommandError::Failed(format!("no Cargo target contains {}", path.display())))?;
            let selector = if target.is_lib() {
                vec!["--lib".to_owned()]
            } else if target.is_bin() {
                vec!["--bin".to_owned(), target.name.clone()]
            } else if target.is_example() {
                vec!["--example".to_owned(), target.name.clone()]
            } else {
                return Err(CommandError::Failed(format!(
                    "Show Assembly works on lib, bin and example targets, not {}",
                    target.name
                )));
            };
            Job::Rust {
                cargo: codecraft_cargo::find_cargo(),
                manifest: package.manifest_path.clone(),
                package: package.name.clone(),
                target: selector,
                crate_name: target.name.replace('-', "_"),
                target_dir: meta.target_directory.clone(),
                release,
            }
        }
        Language::C | Language::Cpp => {
            let cpp = language == Language::Cpp;
            let candidates: &[&str] = if cpp { &["clang++", "c++", "g++"] } else { &["clang", "cc", "gcc"] };
            let compiler = candidates
                .iter()
                .find(|c| codecraft_dap::discover::which(c).is_some())
                .ok_or_else(|| CommandError::Failed("no C compiler found (install clang or gcc)".into()))?;
            Job::C { compiler: (*compiler).to_owned(), file: path.to_path_buf() }
        }
        other => {
            return Err(CommandError::Failed(format!("Show Assembly works on Rust, C and C++ files, not {other:?}")));
        }
    };
    Ok(Request { job, segments, out_dir: output_dir() })
}

/// Code › Show Assembly for the function at the caret (or `path`/`line`).
fn show_assembly(s: &mut Session, p: &Value) -> CmdResult {
    if s.debug.asm.running() {
        return Err(CommandError::Disabled("Show Assembly is already running".into()));
    }
    let (path, line) = match opt_path(p, "path") {
        Some(path) => (crate::normalize_path(&path), opt_usize(p, "line")?.unwrap_or(0)),
        None => {
            let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
            let path = d.path.clone().ok_or_else(|| CommandError::Failed("save the file first".into()))?;
            let line = opt_usize(p, "line")?.unwrap_or_else(|| d.doc.char_to_line(e.selections.primary().head));
            (path, line)
        }
    };
    let request = request_for(s, &path, line, bool_or(p, "release", false))?;
    let function = request.function();
    let (tx, rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("show-assembly".into())
        .spawn(move || {
            let _ = tx.send(produce(&request));
        })
        .map_err(|e| CommandError::Failed(format!("cannot start a worker thread: {e}")))?;
    s.debug.asm.rx = Some(rx);
    s.debug.asm.error = None;
    s.status = format!("Generating assembly for {function}…");
    Ok(json!({ "function": function }))
}

/// Drain a finished job: open the result. True if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(rx) = &s.debug.asm.rx else { return false };
    let result = match rx.try_recv() {
        Ok(result) => result,
        Err(crossbeam_channel::TryRecvError::Empty) => return false,
        Err(crossbeam_channel::TryRecvError::Disconnected) => Err("Show Assembly stopped unexpectedly".into()),
    };
    s.debug.asm.rx = None;
    match result {
        Ok(out) => {
            open_read_only(s, &out.file);
            s.status = match &out.symbol {
                Some(_) => format!("Assembly of {} ({})", out.function, out.tool),
                None => format!("{} not found in the assembly; showing the whole output", out.function),
            };
            s.debug.asm.last = Some(out);
        }
        Err(e) => {
            s.status = "Show Assembly failed".into();
            s.notify(NotificationLevel::Error, "Show Assembly", e.clone());
            s.debug.asm.error = Some(e);
        }
    }
    true
}

/// Open (or refresh) a generated file as a read-only tab.
fn open_read_only(s: &mut Session, path: &Path) {
    if let Some(id) = s.ws.doc_by_path(path)
        && let Ok(text) = codecraft_fs::read_text_file(path)
    {
        let editor = s.ws.editor_for_doc(id);
        let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
        if let Some(od) = s.ws.doc_mut(id)
            && od.doc.replace_all(&text, &sels).is_ok()
        {
            od.doc.mark_saved();
        }
        if let Some(e) = editor {
            let len = s.ws.doc(id).map(|d| d.doc.len_chars()).unwrap_or(0);
            s.ws.set_selections(e, sels.clamp(len));
        }
    }
    match s.open_file(path) {
        Ok(editor) => {
            if let Some(doc) = s.ws.editor(editor).map(|e| e.doc)
                && let Some(od) = s.ws.doc_mut(doc)
            {
                od.read_only = true;
            }
        }
        Err(e) => s.notify(NotificationLevel::Error, "Show Assembly", e.to_string()),
    }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "code.showAssembly",
        "Show Assembly",
        None,
        "{path?: string, line?: int, release?: bool} — the assembly of the Rust, C or C++ function at the caret \
         (cargo asm when installed, else rustc --emit asm; cc -S), in a read-only tab",
        Context::Global,
        always,
        show_assembly,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn function_paths_follow_impls_and_modules() {
        let text = "mod inner {\n    pub fn deep(x: u8) -> u8 {\n        x\n    }\n}\nstruct Calc;\nimpl<T> Show for Calc<T> {\n    fn show(&self) {\n        let x = 1;\n    }\n}\nfn top() {}\n";
        assert_eq!(function_path(Language::Rust, text, 2), Some(vec!["inner".to_owned(), "deep".to_owned()]));
        assert_eq!(function_path(Language::Rust, text, 8), Some(vec!["Calc".to_owned(), "show".to_owned()]));
        assert_eq!(function_path(Language::Rust, text, 11), Some(vec!["top".to_owned()]));
        assert_eq!(function_path(Language::Rust, text, 0), None);
        assert_eq!(function_path(Language::Rust, "", usize::MAX), None);
        assert_eq!(impl_type("impl<'a, T: Clone> fmt::Display for crate::x::Wrapper<'a, T>"), "Wrapper");
        assert_eq!(impl_type("impl Calc"), "Calc");
        assert_eq!(impl_type("impl<"), "");
    }

    #[test]
    fn symbols_match_legacy_v0_and_c_mangling() {
        let segs = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        let twice = segs(&["Calc", "twice"]);
        assert_eq!(symbol_score("__RNvMCs4IVzqLtx3cz_7asmprojNtB2_4Calc5twice", &twice, false), 2);
        assert_eq!(symbol_score("__ZN7asmproj4Calc5twice17h0123456789abcdefE", &twice, false), 2);
        assert_eq!(symbol_score("__ZN7asmproj5Other5twice17h0123456789abcdefE", &twice, false), 1);
        assert_eq!(symbol_score("__ZN7asmproj15twice_more_xyzw17hE", &twice, false), 0);
        assert_eq!(symbol_score("_add", &segs(&["add"]), true), usize::MAX);
        assert_eq!(symbol_score("__Z3addii", &segs(&["add"]), true), 1);
        assert_eq!(symbol_score("anything", &[], false), 0);
    }

    #[test]
    fn functions_are_cut_out_of_assembly() {
        let asm = "\t.section\t__TEXT,__text\n\t.globl\t_other\n_other:   ; @other\n\t.cfi_startproc\n\tret\n\t.cfi_endproc\n\n\t.globl\t__ZN3app4Calc5twice17hE\n__ZN3app4Calc5twice17hE:\nLfunc_begin1:\n\t.cfi_startproc\n\tlsl\tx0, x0, #1\nLtmp3:\n\tret\nLBB1_2:\n\tnop\n\t.cfi_endproc\nLfunc_end1:\n_next:\n\tret\n";
        let (symbol, body) = extract_function(asm, &["Calc".to_owned(), "twice".to_owned()], false).expect("found");
        assert_eq!(symbol, "__ZN3app4Calc5twice17hE");
        assert!(body.contains("lsl\tx0, x0, #1") && body.contains("LBB1_2:") && body.contains("nop"), "{body}");
        assert!(!body.contains("cfi") && !body.contains("_next") && !body.contains("Ltmp3"), "{body}");
        assert!(extract_function(asm, &["missing".to_owned()], false).is_none());
        let (c_symbol, c_body) = extract_function(asm, &["other".to_owned()], true).expect("C symbol");
        assert_eq!(c_symbol, "_other");
        assert!(c_body.contains("ret"));
    }

    #[test]
    fn show_assembly_needs_a_function() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("notes.txt");
        std::fs::write(&file, "hello").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        assert!(s.execute("code.showAssembly", &json!({ "path": file })).is_err());
        let c = tmp.path().join("a.c");
        std::fs::write(&c, "int x;\n").unwrap();
        let err = s.execute("code.showAssembly", &json!({ "path": c, "line": 0 })).unwrap_err();
        assert!(err.to_string().contains("function"), "{err}");
        let rs = tmp.path().join("lib.rs");
        std::fs::write(&rs, "fn f() {}\n").unwrap();
        let err = s.execute("code.showAssembly", &json!({ "path": rs, "line": 0 })).unwrap_err();
        assert!(err.to_string().contains("Cargo"), "{err}");
    }
}
