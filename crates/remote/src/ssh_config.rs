//! `~/.ssh/config` (read-only): `Host` blocks with patterns (`*`, `?`, `!negation`), `Include`
//! (relative to `~/.ssh`, with wildcards in the file name) and the options CodeCraft shows.
//!
//! As in OpenSSH the first value obtained for an option wins (`IdentityFile` accumulates).
//! `Match` blocks cannot be evaluated without connecting, so their options are skipped; `ssh`
//! itself still applies them because CodeCraft connects through the system `ssh`.

use std::path::{Path, PathBuf};

use crate::{RemoteError, RemoteResult};

/// OpenSSH's include depth limit.
const MAX_DEPTH: usize = 16;
/// Refuse absurdly large config files.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Files matched by one wildcard `Include`.
const MAX_GLOB_FILES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Block {
    /// `Host` patterns; empty for the options before the first `Host` (they apply to all hosts).
    patterns: Vec<String>,
    /// A `Match` block (never applied).
    is_match: bool,
    options: Vec<(String, Vec<String>)>,
}

/// A parsed SSH client configuration.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SshConfig {
    blocks: Vec<Block>,
    /// Problems met while reading (unreadable includes); shown, never fatal.
    pub warnings: Vec<String>,
}

/// The options CodeCraft shows for a host (what `ssh` would use, minus `Match` blocks).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostInfo {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
}

impl SshConfig {
    /// Parse config text. `Include` directives are resolved against `ssh_dir` (normally `~/.ssh`)
    /// with `home` for `~`; pass `None` to ignore includes.
    pub fn parse(text: &str, ssh_dir: Option<&Path>, home: Option<&Path>) -> SshConfig {
        let mut cfg = SshConfig::default();
        cfg.blocks.push(Block { patterns: Vec::new(), is_match: false, options: Vec::new() });
        cfg.read_text(text, ssh_dir, home, 0);
        cfg
    }

    /// Read a config file (missing file = empty config).
    pub fn load(path: &Path, home: Option<&Path>) -> RemoteResult<SshConfig> {
        let text = match read_limited(path) {
            Ok(t) => t,
            Err(e) if !path.exists() => {
                log::debug!("no ssh config at {}: {e}", path.display());
                return Ok(SshConfig::default());
            }
            Err(e) => return Err(RemoteError::Io(e)),
        };
        Ok(Self::parse(&text, path.parent(), home))
    }

    fn read_text(&mut self, text: &str, ssh_dir: Option<&Path>, home: Option<&Path>, depth: usize) {
        for raw in text.lines() {
            let Some((key, args)) = split_line(raw) else { continue };
            match key.as_str() {
                "host" => self.blocks.push(Block { patterns: args, is_match: false, options: Vec::new() }),
                "match" => self.blocks.push(Block { patterns: Vec::new(), is_match: true, options: Vec::new() }),
                "include" => {
                    if depth >= MAX_DEPTH {
                        self.warnings.push("Include nested too deeply".into());
                        continue;
                    }
                    let Some(dir) = ssh_dir else { continue };
                    let current = self.blocks.last().cloned();
                    for pattern in &args {
                        for file in include_files(pattern, dir, home) {
                            match read_limited(&file) {
                                Ok(t) => self.read_text(&t, Some(dir), home, depth + 1),
                                Err(e) => self.warnings.push(format!("{}: {e}", file.display())),
                            }
                        }
                    }
                    // Lines after the Include belong to the block that contained it again.
                    if let Some(b) = current
                        && self.blocks.last().map(|l| (&l.patterns, l.is_match)) != Some((&b.patterns, b.is_match))
                    {
                        self.blocks.push(Block { options: Vec::new(), ..b });
                    }
                }
                _ => {
                    if let Some(b) = self.blocks.last_mut() {
                        b.options.push((key, args));
                    }
                }
            }
        }
    }

    /// Concrete host aliases (no wildcards, not negated), in file order, without duplicates.
    pub fn hosts(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for b in self.blocks.iter().filter(|b| !b.is_match) {
            for p in &b.patterns {
                if p.starts_with('!') || p.contains(['*', '?']) || p.starts_with('-') || p.is_empty() {
                    continue;
                }
                if !out.iter().any(|h| h == p) {
                    out.push(p.clone());
                }
            }
        }
        out
    }

    /// The effective options for `alias`.
    pub fn resolve(&self, alias: &str) -> HostInfo {
        let mut info = HostInfo { alias: alias.to_string(), ..HostInfo::default() };
        let mut hostname: Option<String> = None;
        for b in &self.blocks {
            if b.is_match || !(b.patterns.is_empty() || host_matches(&b.patterns, alias)) {
                continue;
            }
            for (key, args) in &b.options {
                let Some(first) = args.first() else { continue };
                match key.as_str() {
                    "hostname" if hostname.is_none() => hostname = Some(expand_tokens(first, alias)),
                    "user" if info.user.is_none() => info.user = Some(first.clone()),
                    "port" if info.port.is_none() => info.port = first.parse().ok(),
                    "identityfile" => info.identity_files.push(first.clone()),
                    "proxyjump" if info.proxy_jump.is_none() => info.proxy_jump = Some(first.clone()),
                    _ => {}
                }
            }
        }
        info.hostname = hostname.unwrap_or_else(|| alias.to_string());
        info
    }
}

fn read_limited(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_FILE_BYTES {
        return Err("file too large".into());
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `keyword args…` or `keyword=args…`; keyword lowercased. `None` for blank and comment lines.
fn split_line(raw: &str) -> Option<(String, Vec<String>)> {
    let line = raw.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let key = line.get(..end)?.to_ascii_lowercase();
    let mut rest = line.get(end..)?.trim_start();
    if let Some(r) = rest.strip_prefix('=') {
        rest = r.trim_start();
    }
    Some((key, split_args(rest)))
}

/// Whitespace-separated arguments; double quotes group (and are removed).
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            '#' if !quoted && !any => break,
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// Whether `host` matches a `Host` pattern list: some positive pattern matches and no negated one.
pub fn host_matches(patterns: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    let mut matched = false;
    for p in patterns {
        let p = p.to_ascii_lowercase();
        if let Some(neg) = p.strip_prefix('!') {
            if wildcard(neg, &host) {
                return false;
            }
        } else if wildcard(&p, &host) {
            matched = true;
        }
    }
    matched
}

/// `*` (any run) and `?` (one character) matching, iterative (no exponential backtracking).
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ti));
                pi += 1;
            }
            Some(&c) if c == '?' || Some(&c) == t.get(ti) => {
                pi += 1;
                ti += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    pi = sp + 1;
                    ti = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    p.get(pi..).is_some_and(|rest| rest.iter().all(|c| *c == '*'))
}

/// `%h` → the alias, `%%` → `%` (other tokens are left for `ssh`).
fn expand_tokens(value: &str, alias: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(alias),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

/// The files an `Include` argument names: `~` expanded, relative to `ssh_dir`, wildcards in the
/// last component matched against the directory listing (sorted, like `glob(3)`).
fn include_files(pattern: &str, ssh_dir: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let path = match (pattern.strip_prefix("~/"), home) {
        (Some(rest), Some(h)) => h.join(rest),
        (Some(_), None) => return Vec::new(),
        (None, _) if Path::new(pattern).is_absolute() => PathBuf::from(pattern),
        (None, _) => ssh_dir.join(pattern),
    };
    let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else { return Vec::new() };
    if !name.contains(['*', '?']) {
        return if path.is_file() { vec![path] } else { Vec::new() };
    }
    let Some(dir) = path.parent() else { return Vec::new() };
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_str().is_some_and(|n| !n.starts_with('.') && wildcard(&name, n)))
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .take(MAX_GLOB_FILES)
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# global
ServerAliveInterval 30

Host web web-staging
    HostName web.example.com
    User deploy
    Port 2222
    IdentityFile ~/.ssh/web_ed25519

Host *.internal !secret.internal
    User ops
    ProxyJump bastion

Host "quoted host"
    HostName=10.0.0.5

Match host foo
    User matched

Host *
    User fallback
    IdentityFile ~/.ssh/id_ed25519
    Port 22
"#;

    #[test]
    fn hosts_lists_concrete_aliases() {
        let c = SshConfig::parse(SAMPLE, None, None);
        assert_eq!(c.hosts(), vec!["web", "web-staging", "quoted host"]);
    }

    #[test]
    fn resolve_first_value_wins_and_identity_files_accumulate() {
        let c = SshConfig::parse(SAMPLE, None, None);
        let web = c.resolve("web");
        assert_eq!(web.hostname, "web.example.com");
        assert_eq!(web.user.as_deref(), Some("deploy"));
        assert_eq!(web.port, Some(2222));
        assert_eq!(web.identity_files, vec!["~/.ssh/web_ed25519", "~/.ssh/id_ed25519"]);
        let db = c.resolve("db.internal");
        assert_eq!(db.user.as_deref(), Some("ops"));
        assert_eq!(db.proxy_jump.as_deref(), Some("bastion"));
        assert_eq!(db.hostname, "db.internal");
        let secret = c.resolve("secret.internal");
        assert_eq!(secret.user.as_deref(), Some("fallback"));
        assert_eq!(secret.proxy_jump, None);
        assert_eq!(c.resolve("quoted host").hostname, "10.0.0.5");
        // Match blocks are never applied.
        assert_eq!(c.resolve("foo").user.as_deref(), Some("fallback"));
    }

    #[test]
    fn wildcards() {
        assert!(wildcard("*.example.com", "a.example.com"));
        assert!(!wildcard("*.example.com", "example.com"));
        assert!(wildcard("web?", "web1"));
        assert!(!wildcard("web?", "web"));
        assert!(wildcard("*", ""));
        assert!(wildcard("a*b*c", "aXXbYYc"));
        assert!(!wildcard("a*b*c", "aXXbYY"));
        assert!(host_matches(&["WEB".into()], "web"));
        assert!(!host_matches(&["!web".into()], "web"));
        assert!(!host_matches(&["!web".into()], "other"));
        // Pathological patterns stay fast.
        let p = "*a".repeat(50);
        assert!(!wildcard(&p, &"b".repeat(2000)));
    }

    #[test]
    fn hostname_tokens() {
        let c = SshConfig::parse("Host h*\n  HostName %h.example.com\nHost p\n  HostName 100%%\n", None, None);
        assert_eq!(c.resolve("h1").hostname, "h1.example.com");
        assert_eq!(c.resolve("p").hostname, "100%");
    }

    #[test]
    fn includes_are_resolved_relative_to_the_ssh_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::write(ssh.join("conf.d/10-a.conf"), "Host alpha\n  HostName a.example\n").unwrap();
        std::fs::write(ssh.join("conf.d/20-b.conf"), "Host beta\n  User bob\n").unwrap();
        std::fs::write(ssh.join("conf.d/ignored.txt"), "Host nope\n").unwrap();
        std::fs::write(dir.path().join("extra"), "Host gamma\n").unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host top\n  Include conf.d/*.conf ~/extra missing.conf\n  User topuser\nHost last\n",
        )
        .unwrap();
        let c = SshConfig::load(&ssh.join("config"), Some(dir.path())).unwrap();
        assert_eq!(c.hosts(), vec!["top", "alpha", "beta", "gamma", "last"]);
        assert_eq!(c.resolve("alpha").hostname, "a.example");
        assert_eq!(c.resolve("beta").user.as_deref(), Some("bob"));
        // The line after the Include still belongs to `Host top`.
        assert_eq!(c.resolve("top").user.as_deref(), Some("topuser"));
        assert_eq!(c.resolve("alpha").user, None);
    }

    #[test]
    fn recursive_includes_stop() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config"), "Include config\nHost x\n").unwrap();
        let c = SshConfig::load(&dir.path().join("config"), None).unwrap();
        assert!(c.warnings.iter().any(|w| w.contains("deeply")));
        assert_eq!(c.hosts(), vec!["x"]);
    }

    #[test]
    fn missing_file_is_empty_and_garbage_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SshConfig::load(&dir.path().join("none"), None).unwrap().hosts().is_empty());
        let c = SshConfig::parse("Host\n=\n\"\nPort notanumber\nHost -oProxyCommand=x ok\n  Port 99999\n", None, None);
        assert_eq!(c.hosts(), vec!["ok"]);
        assert_eq!(c.resolve("ok").port, None);
    }
}
