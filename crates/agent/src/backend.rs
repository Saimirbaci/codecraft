//! The supported agent CLIs and how to find them.
//!
//! GUI apps on macOS start with a minimal `PATH` (`/usr/bin:/bin:…`), so detection and every
//! spawn use [`enriched_path`]: the usual per-user install locations first, then the inherited
//! `PATH`. Detection only stats files; it never runs a CLI.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Claude,
    Codex,
    Grok,
    Gemini,
}

impl Backend {
    pub const ALL: [Backend; 4] = [Backend::Claude, Backend::Codex, Backend::Grok, Backend::Gemini];

    /// Machine id used in commands (`"claude"`, `"codex"`, …).
    pub fn id(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
            Self::Gemini => "gemini",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Grok => "Grok",
            Self::Gemini => "Gemini",
        }
    }

    /// The headless CLI's executable name.
    pub fn binary(self) -> &'static str {
        self.id()
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            Self::Claude => "Install Claude Code: curl -fsSL https://claude.ai/install.sh | bash",
            Self::Codex => "Install Codex: npm install -g @openai/codex",
            Self::Grok => "Install Grok Build: see https://x.ai/cli",
            Self::Gemini => "Install Gemini CLI: npm install -g @google/gemini-cli",
        }
    }

    /// ACP adapter executables for agents without native ACP support (first found wins).
    pub fn acp_adapters(self) -> &'static [&'static str] {
        match self {
            Self::Claude => &["claude-code-acp", "claude-agent-acp"],
            Self::Codex => &["codex-acp"],
            Self::Grok | Self::Gemini => &[],
        }
    }

    /// Arguments that make the CLI itself speak ACP on stdio (verified against `--help`:
    /// `gemini --acp`; `grok agent stdio`, whose initialize answers with ACP protocol version 1).
    pub fn native_acp_args(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Gemini => Some(&["--acp"]),
            Self::Grok => Some(&["agent", "--no-leader", "stdio"]),
            Self::Claude | Self::Codex => None,
        }
    }

    /// What to install for "Ask" mode when no ACP path exists.
    pub fn acp_install_hint(self) -> &'static str {
        match self {
            Self::Claude => {
                "Ask mode needs the ACP adapter: npm install -g @zed-industries/claude-code-acp \
                 (CodeCraft looks for claude-code-acp or claude-agent-acp)"
            }
            Self::Codex => "Ask mode needs the ACP adapter: npm install -g @zed-industries/codex-acp (codex-acp)",
            Self::Grok => "Ask mode needs a Grok Build version with `grok agent stdio`",
            Self::Gemini => "Ask mode needs a Gemini CLI version with `--acp`",
        }
    }
}

/// How to start an agent in ACP mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcpLaunch {
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// One backend as detected on this machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub backend: Backend,
    /// The headless CLI, if installed.
    pub path: Option<PathBuf>,
    /// How to run it over ACP (Ask mode), if possible.
    pub acp: Option<AcpLaunch>,
}

impl BackendInfo {
    pub fn installed(&self) -> bool {
        self.path.is_some()
    }

    /// Why Ask mode is unavailable (`None` when it is available).
    pub fn ask_unavailable(&self) -> Option<String> {
        if self.acp.is_some() {
            return None;
        }
        if self.path.is_none() && self.backend.native_acp_args().is_some() {
            return Some(self.backend.install_hint().to_string());
        }
        Some(self.backend.acp_install_hint().to_string())
    }
}

/// User overrides: explicit CLI paths and ACP commands per backend (also used by tests to point
/// at fake agents).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overrides {
    pub binaries: HashMap<Backend, PathBuf>,
    /// Full ACP command line (program first).
    pub acp: HashMap<Backend, Vec<String>>,
}

/// Detect one backend.
pub fn detect(backend: Backend, overrides: &Overrides) -> BackendInfo {
    let search = enriched_path();
    let path = overrides
        .binaries
        .get(&backend)
        .filter(|p| p.is_file())
        .cloned()
        .or_else(|| find_in_path(backend.binary(), &search));
    let acp = if let Some(cmd) = overrides.acp.get(&backend) {
        cmd.split_first().map(|(program, args)| AcpLaunch { program: PathBuf::from(program), args: args.to_vec() })
    } else if let Some(args) = backend.native_acp_args() {
        path.clone().map(|program| AcpLaunch { program, args: args.iter().map(|a| (*a).to_string()).collect() })
    } else {
        backend
            .acp_adapters()
            .iter()
            .find_map(|name| find_in_path(name, &search))
            .map(|program| AcpLaunch { program, args: Vec::new() })
    };
    BackendInfo { backend, path, acp }
}

pub fn detect_all(overrides: &Overrides) -> Vec<BackendInfo> {
    Backend::ALL.into_iter().map(|b| detect(b, overrides)).collect()
}

/// Directories where agent CLIs are commonly installed, most specific first.
fn extra_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        for d in [".local/bin", ".cargo/bin", ".bun/bin", ".grok/bin", ".volta/bin", ".npm-global/bin"] {
            dirs.push(home.join(d));
        }
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    if let Some(prefix) = npm_prefix(home.as_deref()) {
        dirs.push(prefix.join("bin"));
    }
    if let Some(home) = &home {
        dirs.extend(nvm_bins(home));
    }
    dirs
}

/// The npm global prefix from `$NPM_CONFIG_PREFIX` or `~/.npmrc` (no `npm` process is run).
fn npm_prefix(home: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("NPM_CONFIG_PREFIX").or_else(|| std::env::var_os("npm_config_prefix")) {
        return Some(PathBuf::from(p));
    }
    let home = home?;
    let rc = std::fs::read_to_string(home.join(".npmrc")).ok()?;
    rc.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        if k.trim() != "prefix" {
            return None;
        }
        let v = v.trim();
        Some(match v.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(v),
        })
    })
}

/// `bin` directories of nvm-installed Node versions, newest name last-sorted first.
fn nvm_bins(home: &Path) -> Vec<PathBuf> {
    let root = std::env::var_os("NVM_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".nvm"));
    let mut out = vec![root.join("current/bin")];
    if let Ok(rd) = std::fs::read_dir(root.join("versions/node")) {
        let mut versions: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path().join("bin")).collect();
        versions.sort();
        versions.reverse();
        out.extend(versions);
    }
    out
}

/// `PATH` for detection and child processes: the common install locations, then the inherited
/// `PATH`, then the system defaults.
pub fn enriched_path() -> OsString {
    let mut dirs = extra_dirs();
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    for d in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        dirs.push(PathBuf::from(d));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// First executable file called `name` in a `PATH`-style list.
pub fn find_in_path(name: &str, path: &OsString) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') {
        return None;
    }
    std::env::split_paths(path).map(|d| d.join(name)).find(|p| is_executable(p))
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip() {
        for b in Backend::ALL {
            assert_eq!(Backend::from_id(b.id()), Some(b));
            assert_eq!(serde_json::to_value(b).ok(), Some(serde_json::json!(b.id())));
        }
        assert_eq!(Backend::from_id("vim"), None);
    }

    #[test]
    fn enriched_path_has_user_dirs_and_system() {
        let p = enriched_path();
        let dirs: Vec<PathBuf> = std::env::split_paths(&p).collect();
        assert!(dirs.contains(&PathBuf::from("/usr/bin")));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(dirs.contains(&PathBuf::from(home).join(".local/bin")));
        }
    }

    #[test]
    fn find_and_override() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("claude");
        std::fs::write(&bin, "#!/bin/sh\n").ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).ok();
        }
        let path = std::env::join_paths([tmp.path()]).unwrap_or_default();
        assert_eq!(find_in_path("claude", &path), Some(bin.clone()));
        assert_eq!(find_in_path("../claude", &path), None);
        let mut o = Overrides::default();
        o.binaries.insert(Backend::Claude, bin.clone());
        o.acp.insert(Backend::Claude, vec!["/fake/acp".into(), "--x".into()]);
        let info = detect(Backend::Claude, &o);
        assert_eq!(info.path, Some(bin));
        assert_eq!(info.acp, Some(AcpLaunch { program: "/fake/acp".into(), args: vec!["--x".into()] }));
        assert!(info.ask_unavailable().is_none());
    }

    #[test]
    fn ask_hint_when_no_adapter() {
        let info = BackendInfo { backend: Backend::Codex, path: Some("/x/codex".into()), acp: None };
        assert!(info.ask_unavailable().is_some_and(|h| h.contains("codex-acp")));
        let gem = BackendInfo { backend: Backend::Gemini, path: None, acp: None };
        assert!(gem.ask_unavailable().is_some_and(|h| h.contains("gemini-cli")));
    }
}
