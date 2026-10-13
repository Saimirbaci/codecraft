//! Finding debug adapters: lldb-dap, CodeLLDB, debugpy and js-debug.
//!
//! Every function here may run external programs (`xcrun`, `python3`) and is meant for a worker
//! thread, not the UI thread. A missing adapter is an [`Unavailable`] with an install hint.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::{AdapterCommand, AdapterKind, AdapterTransport};

/// Why an adapter can't be used, and how to get it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unavailable {
    pub adapter: &'static str,
    pub hint: String,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} is not available. {}", self.adapter, self.hint)
    }
}

impl std::error::Error for Unavailable {}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `name` resolved through `PATH`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| is_executable(p))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Run a program and return its trimmed stdout when it succeeds.
fn output_of(program: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// lldb-dap: `$CODECRAFT_LLDB_DAP`, `xcrun -f lldb-dap` (macOS), `PATH` (also the older name
/// `lldb-vscode`), then Homebrew's LLVM.
pub fn find_lldb_dap() -> Result<AdapterCommand, Unavailable> {
    let candidates = || -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("CODECRAFT_LLDB_DAP").map(PathBuf::from).filter(|p| is_executable(p)) {
            return Some(p);
        }
        if cfg!(target_os = "macos")
            && let Some(p) = output_of(Path::new("xcrun"), &["-f", "lldb-dap"]).map(PathBuf::from)
            && is_executable(&p)
        {
            return Some(p);
        }
        if let Some(p) = which("lldb-dap").or_else(|| which("lldb-vscode")) {
            return Some(p);
        }
        ["/opt/homebrew/opt/llvm/bin/lldb-dap", "/usr/local/opt/llvm/bin/lldb-dap", "/usr/bin/lldb-dap"]
            .iter()
            .map(PathBuf::from)
            .find(|p| is_executable(p))
    };
    match candidates() {
        Some(program) => Ok(AdapterCommand::stdio("lldb-dap", AdapterKind::LldbDap, program)),
        None => Err(Unavailable {
            adapter: "lldb-dap",
            hint: if cfg!(target_os = "macos") {
                "Install the Xcode command line tools (`xcode-select --install`) or LLVM (`brew install llvm`).".into()
            } else {
                "Install LLDB (for example `apt install lldb`), which ships lldb-dap.".into()
            },
        }),
    }
}

/// CodeLLDB from a VS Code (or VSCodium/Cursor) extension install. It talks DAP over TCP.
pub fn find_codelldb() -> Result<AdapterCommand, Unavailable> {
    let mut roots = Vec::new();
    if let Some(home) = home() {
        for dir in [".vscode/extensions", ".vscode-oss/extensions", ".cursor/extensions", ".windsurf/extensions"] {
            roots.push(home.join(dir));
        }
    }
    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("vadimcn.vscode-lldb") {
                let exe = entry.path().join("adapter").join("codelldb");
                if is_executable(&exe) {
                    found.push(exe);
                }
            }
        }
    }
    // The highest version sorts last (`vadimcn.vscode-lldb-1.12.0`).
    found.sort();
    match found.pop() {
        Some(program) => {
            let mut cmd = AdapterCommand::stdio("CodeLLDB", AdapterKind::CodeLldb, program);
            cmd.transport = AdapterTransport::Tcp { port_arg: vec!["--port".into(), "{port}".into()] };
            Ok(cmd)
        }
        None => Err(Unavailable {
            adapter: "CodeLLDB",
            hint: "Install the CodeLLDB extension (vadimcn.vscode-lldb) in VS Code.".into(),
        }),
    }
}

/// A native debugger: lldb-dap first, then CodeLLDB.
pub fn find_native() -> Result<AdapterCommand, Unavailable> {
    find_lldb_dap().or_else(|lldb| find_codelldb().map_err(|_| lldb))
}

/// The Python interpreter to run debugpy with: `python` if given, else `python3` on `PATH`.
pub fn find_python(python: Option<&Path>) -> Option<PathBuf> {
    match python {
        Some(p) if is_executable(p) => Some(p.to_path_buf()),
        Some(p) if p.components().count() == 1 => which(&p.to_string_lossy()),
        Some(_) => None,
        None => which("python3").or_else(|| which("python")),
    }
}

/// Whether `python` can import debugpy.
pub fn has_debugpy(python: &Path) -> bool {
    output_of(python, &["-c", "import debugpy; print(debugpy.__version__)"]).is_some()
}

/// debugpy's adapter (`python -m debugpy.adapter`). Tries `python` (a project virtualenv, say),
/// then `python3` from `PATH`.
pub fn find_debugpy(python: Option<&Path>) -> Result<AdapterCommand, Unavailable> {
    let mut tried = Vec::new();
    for candidate in [python.and_then(|p| find_python(Some(p))), find_python(None)].into_iter().flatten() {
        if tried.contains(&candidate) {
            continue;
        }
        if has_debugpy(&candidate) {
            let mut cmd = AdapterCommand::stdio("debugpy", AdapterKind::Debugpy, candidate);
            cmd.args = vec!["-m".into(), "debugpy.adapter".into()];
            return Ok(cmd);
        }
        tried.push(candidate);
    }
    let hint = match tried.first() {
        Some(p) => format!("Install it with `{} -m pip install debugpy`.", p.display()),
        None => "Install Python 3, then `python3 -m pip install debugpy`.".into(),
    };
    Err(Unavailable { adapter: "debugpy", hint })
}

/// js-debug's DAP server (`dapDebugServer.js` from the vscode-js-debug release, run with node),
/// or a `js-debug-adapter` launcher on `PATH`. It talks DAP over TCP.
pub fn find_js_debug() -> Result<AdapterCommand, Unavailable> {
    let unavailable = |hint: &str| Unavailable { adapter: "js-debug", hint: hint.to_owned() };
    let mut scripts: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("CODECRAFT_JS_DEBUG").map(PathBuf::from) {
        scripts.push(p);
    }
    if let Some(home) = home() {
        scripts.push(home.join(".local/share/js-debug/src/dapDebugServer.js"));
        scripts.push(home.join(".local/share/js-debug/dapDebugServer.js"));
    }
    let script = scripts.into_iter().find(|p| p.is_file());
    let node = which("node");
    let mut cmd = match (script, node) {
        (Some(script), Some(node)) => {
            let mut cmd = AdapterCommand::stdio("js-debug", AdapterKind::JsDebug, node);
            cmd.args = vec![script.display().to_string()];
            cmd
        }
        (Some(_), None) => return Err(unavailable("Install Node.js; js-debug runs on node.")),
        (None, _) => match which("js-debug-adapter") {
            Some(program) => AdapterCommand::stdio("js-debug", AdapterKind::JsDebug, program),
            None => {
                return Err(unavailable(
                    "Download js-debug-dap from https://github.com/microsoft/vscode-js-debug/releases and unpack it \
                     to ~/.local/share/js-debug (or set CODECRAFT_JS_DEBUG to its dapDebugServer.js).",
                ));
            }
        },
    };
    cmd.transport = AdapterTransport::Tcp { port_arg: vec!["{port}".into(), "127.0.0.1".into()] };
    Ok(cmd)
}

/// `rustc --print sysroot`, for Rust's LLDB pretty printers.
pub fn rust_sysroot(rustc: Option<&Path>) -> Option<PathBuf> {
    let fallback = home().map(|h| h.join(".cargo/bin/rustc")).filter(|p| is_executable(p));
    let rustc = rustc
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("RUSTC").map(PathBuf::from))
        .or_else(|| which("rustc"))
        .or(fallback)?;
    output_of(&rustc, &["--print", "sysroot"]).map(PathBuf::from).filter(|p| p.is_dir())
}

/// The directory with Rust's LLDB formatters (`lldb_lookup.py`, `lldb_commands`): the toolchain's
/// own `lib/rustlib/etc`, else (a toolchain installed without them) the newest sibling rustup
/// toolchain's, which is what `rust-lldb` would need too.
pub fn rust_formatters_dir(sysroot: &Path) -> Option<PathBuf> {
    let etc_of = |root: &Path| root.join("lib").join("rustlib").join("etc");
    let own = etc_of(sysroot);
    if own.join("lldb_lookup.py").is_file() {
        return Some(own);
    }
    let toolchains = sysroot.parent()?;
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(toolchains)
        .ok()?
        .flatten()
        .map(|e| etc_of(&e.path()))
        .filter(|etc| etc.join("lldb_lookup.py").is_file())
        .take(64)
        .collect();
    // Toolchain directories are named by version or channel; the last in order is the newest.
    candidates.sort();
    candidates.pop()
}

/// LLDB commands that load Rust's pretty printers (so `Vec`, `String`, `Option`… display their
/// contents), from the toolchain's `lib/rustlib/etc`.
pub fn rust_lldb_init_commands(sysroot: &Path) -> Vec<String> {
    let Some(etc) = rust_formatters_dir(sysroot) else { return Vec::new() };
    let mut commands = Vec::new();
    let lookup = etc.join("lldb_lookup.py");
    if lookup.is_file() {
        commands.push(format!("command script import \"{}\"", lookup.display()));
    }
    let extra = etc.join("lldb_commands");
    if extra.is_file() {
        commands.push(format!("command source -s 0 \"{}\"", extra.display()));
    }
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_commands_for_a_fake_sysroot() {
        let tmp = tempfile::tempdir().unwrap();
        let etc = tmp.path().join("lib/rustlib/etc");
        std::fs::create_dir_all(&etc).unwrap();
        assert!(rust_lldb_init_commands(tmp.path()).is_empty());
        std::fs::write(etc.join("lldb_lookup.py"), "").unwrap();
        std::fs::write(etc.join("lldb_commands"), "").unwrap();
        let commands = rust_lldb_init_commands(tmp.path());
        assert_eq!(commands.len(), 2);
        assert!(commands[0].starts_with("command script import"));
        assert!(commands[1].contains("lldb_commands"));
    }

    #[test]
    fn formatters_fall_back_to_a_sibling_toolchain() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("1.90-aarch64-apple-darwin");
        std::fs::create_dir_all(bare.join("lib/rustlib/etc")).unwrap();
        assert_eq!(rust_formatters_dir(&bare), None);
        for v in ["1.85-x", "1.86-x"] {
            let etc = tmp.path().join(v).join("lib/rustlib/etc");
            std::fs::create_dir_all(&etc).unwrap();
            std::fs::write(etc.join("lldb_lookup.py"), "").unwrap();
        }
        assert_eq!(rust_formatters_dir(&bare), Some(tmp.path().join("1.86-x/lib/rustlib/etc")));
        let commands = rust_lldb_init_commands(&bare);
        assert_eq!(commands.len(), 1);
        assert!(commands[0].contains("1.86-x"), "{commands:?}");
        // The toolchain's own formatters win.
        let own = bare.join("lib/rustlib/etc");
        std::fs::write(own.join("lldb_lookup.py"), "").unwrap();
        assert_eq!(rust_formatters_dir(&bare), Some(own));
        assert_eq!(rust_formatters_dir(Path::new("/")), None);
    }

    #[test]
    fn missing_python_is_unavailable_with_a_hint() {
        let err = find_debugpy(Some(Path::new("/definitely/not/python"))).err();
        // Either debugpy is installed for python3 on PATH (fine) or the hint says how to get it.
        if let Some(err) = err {
            assert!(err.hint.contains("debugpy"), "{err}");
        }
        assert!(find_python(Some(Path::new("/definitely/not/python"))).is_none());
    }

    #[test]
    fn which_finds_sh() {
        if cfg!(unix) {
            assert!(which("sh").is_some());
            assert!(which("definitely-not-a-program-codecraft").is_none());
        }
    }
}
