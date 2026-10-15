//! Which language server to run for a language, where to find it, and how to install it.

use std::path::{Path, PathBuf};

/// The language servers CodeCraft knows how to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ServerKind {
    RustAnalyzer,
    Clangd,
    /// Python: basedpyright, pyright, pylsp or ty (first found).
    Python,
    Zls,
    /// TypeScript/JavaScript: typescript-language-server or vtsls (deno lsp for Deno projects).
    TypeScript,
}

/// How to start a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Human-readable name ("clangd", "basedpyright", …).
    pub name: String,
}

impl ServerKind {
    pub const ALL: [ServerKind; 5] = [Self::RustAnalyzer, Self::Clangd, Self::Python, Self::Zls, Self::TypeScript];

    pub fn label(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rust-analyzer",
            Self::Clangd => "clangd",
            Self::Python => "Python language server",
            Self::Zls => "zls",
            Self::TypeScript => "TypeScript language server",
        }
    }

    /// How to install a server of this kind.
    pub fn install_hint(self) -> &'static str {
        match self {
            Self::RustAnalyzer => "rustup component add rust-analyzer",
            Self::Clangd => "install Xcode Command Line Tools (xcode-select --install) or `brew install llvm`",
            Self::Python => "pip install basedpyright  (or: npm install -g pyright, pip install python-lsp-server)",
            Self::Zls => "brew install zls  (match your Zig version)",
            Self::TypeScript => "npm install -g typescript-language-server typescript",
        }
    }
}

/// Extra directories GUI apps miss in their minimal `PATH`.
pub fn extra_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for d in [".local/bin", ".cargo/bin", ".bun/bin", ".npm-global/bin", ".volta/bin", ".deno/bin", ".pyenv/shims"]
        {
            dirs.push(home.join(d));
        }
    }
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs
}

/// First executable named `name` on `PATH` or in [`extra_bin_dirs`].
pub fn which(name: &str) -> Option<PathBuf> {
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let path_dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    path_dirs.into_iter().chain(extra_bin_dirs()).map(|d| d.join(&exe)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) if m.is_file() => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                m.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
        _ => false,
    }
}

fn xcrun_find(tool: &str) -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let out =
        std::process::Command::new("xcrun").args(["-f", tool]).stdin(std::process::Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    is_executable(&p).then_some(p)
}

fn cmd(program: PathBuf, args: &[&str], name: &str) -> ServerCommand {
    ServerCommand { program, args: args.iter().map(|a| (*a).to_string()).collect(), name: name.to_string() }
}

/// Find a server for `kind`, looking in the project first (virtualenvs, node_modules).
/// `project_root` may be any directory.
pub fn find_server(kind: ServerKind, project_root: &Path) -> Option<ServerCommand> {
    match kind {
        ServerKind::RustAnalyzer => crate::find_rust_analyzer().map(|p| cmd(p, &[], "rust-analyzer")),
        ServerKind::Clangd => which("clangd")
            .or_else(|| xcrun_find("clangd"))
            .or_else(|| Some(PathBuf::from("/opt/homebrew/opt/llvm/bin/clangd")).filter(|p| is_executable(p)))
            .map(|p| {
                let mut c = cmd(p, &["--background-index", "--header-insertion=never"], "clangd");
                // CLion-style build dirs hold compile_commands.json.
                for dir in ["build", "cmake-build-debug", "cmake-build-release", "out"] {
                    if project_root.join(dir).join("compile_commands.json").is_file() {
                        c.args.push(format!("--compile-commands-dir={}", project_root.join(dir).display()));
                        break;
                    }
                }
                c
            }),
        ServerKind::Python => find_python_server(project_root, None, PYTHON_AUTO),
        ServerKind::Zls => which("zls").map(|p| cmd(p, &[], "zls")),
        ServerKind::TypeScript => {
            let is_deno = ["deno.json", "deno.jsonc"].iter().any(|f| project_root.join(f).is_file());
            if is_deno && let Some(deno) = which("deno") {
                return Some(cmd(deno, &["lsp"], "deno"));
            }
            let local = project_root.join("node_modules/.bin");
            let find = |name: &str| Some(local.join(name)).filter(|p| is_executable(p)).or_else(|| which(name));
            find("typescript-language-server")
                .map(|p| cmd(p, &["--stdio"], "typescript-language-server"))
                .or_else(|| find("vtsls").map(|p| cmd(p, &["--stdio"], "vtsls")))
        }
    }
}

/// A Python server CodeCraft can run: (preference id, executable, arguments, display name).
pub type PythonServer = (&'static str, &'static str, &'static [&'static str], &'static str);

/// The Python servers CodeCraft can run, in the order `auto` tries them.
pub const PYTHON_SERVERS: [PythonServer; 4] = [
    ("basedpyright", "basedpyright-langserver", &["--stdio"], "basedpyright"),
    ("pyright", "pyright-langserver", &["--stdio"], "pyright"),
    ("pylsp", "pylsp", &[], "pylsp"),
    ("ty", "ty", &["server"], "ty"),
];

/// `python_language_server = auto`: the project's virtualenv, then the managed basedpyright,
/// then `PATH`.
pub const PYTHON_AUTO: &str = "auto";

/// Find the Python language server (M15 track EA). `preference` is `auto`, a server id from
/// [`PYTHON_SERVERS`] or `none`; `managed_bin` is CodeCraft's own tool directory
/// (`$CODECRAFT_HOME/tools/bin`, where Install basedpyright puts it).
///
/// `auto` takes a server installed in the project's virtualenv first (a project-level choice),
/// then the managed basedpyright, then the first one on `PATH`. A named server is looked up in
/// the same places; `None` when it is not installed (no silent fallback to another server).
pub fn find_python_server(project_root: &Path, managed_bin: Option<&Path>, preference: &str) -> Option<ServerCommand> {
    let preference = preference.trim().to_ascii_lowercase();
    if preference == "none" || preference == "off" {
        return None;
    }
    let venv_bins: Vec<PathBuf> = [".venv/bin", "venv/bin", ".env/bin"].iter().map(|d| project_root.join(d)).collect();
    let exe = |name: &str| format!("{name}{}", std::env::consts::EXE_SUFFIX);
    let local = |name: &str| venv_bins.iter().map(|d| d.join(exe(name))).find(|p| is_executable(p));
    let managed = |name: &str| managed_bin.map(|d| d.join(exe(name))).filter(|p| is_executable(p));
    let make = |s: &PythonServer, program: PathBuf| cmd(program, s.2, s.3);
    let wanted: Vec<&PythonServer> = if preference.is_empty() || preference == PYTHON_AUTO {
        PYTHON_SERVERS.iter().collect()
    } else {
        PYTHON_SERVERS.iter().filter(|s| s.0 == preference).collect()
    };
    // The project's own server (its virtualenv) is the project-level preference.
    for s in &wanted {
        if let Some(p) = local(s.1) {
            return Some(make(s, p));
        }
    }
    // Then the managed basedpyright (installed only on the user's request).
    for s in wanted.iter().filter(|s| s.0 == "basedpyright") {
        if let Some(p) = managed(s.1) {
            return Some(make(s, p));
        }
    }
    wanted.iter().find_map(|s| which(s.1).map(|p| make(s, p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_and_labels() {
        for k in ServerKind::ALL {
            assert!(!k.label().is_empty());
            assert!(!k.install_hint().is_empty());
        }
    }

    #[test]
    fn local_venv_server_wins() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join(".venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("pylsp");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // A project-local server is preferred unless a better-ranked one is installed globally.
        let found = find_server(ServerKind::Python, dir.path()).unwrap();
        assert!(found.name == "pylsp" || found.name.contains("pyright") || found.name == "ty");
    }

    fn exe(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    #[cfg(unix)]
    fn python_server_preference_venv_then_managed() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        let managed = dir.path().join("tools/bin");
        exe(&managed.join("basedpyright-langserver"));
        // The managed basedpyright comes before anything on PATH.
        let found = find_python_server(&project, Some(&managed), "auto").unwrap();
        assert_eq!(found.name, "basedpyright");
        assert_eq!(found.program, managed.join("basedpyright-langserver"));
        assert_eq!(found.args, ["--stdio"]);
        // A server in the project's virtualenv is the project's choice and wins.
        exe(&project.join(".venv/bin/pylsp"));
        let found = find_python_server(&project, Some(&managed), "auto").unwrap();
        assert_eq!(found.name, "pylsp");
        assert_eq!(found.program, project.join(".venv/bin/pylsp"));
        // A named server: the managed copy serves `basedpyright`.
        let found = find_python_server(&project, Some(&managed), "basedpyright").unwrap();
        assert_eq!(found.program, managed.join("basedpyright-langserver"));
        assert_eq!(find_python_server(&project, Some(&managed), "pylsp").unwrap().name, "pylsp");
        // Turned off, or an unknown id: nothing.
        assert!(find_python_server(&project, Some(&managed), "none").is_none());
        assert!(find_python_server(&project, Some(&managed), "made-up").is_none());
    }

    #[test]
    fn missing_dir_is_fine() {
        let _ = find_server(ServerKind::TypeScript, Path::new("/definitely/not/here"));
        let _ = find_server(ServerKind::Zls, Path::new(""));
    }
}
