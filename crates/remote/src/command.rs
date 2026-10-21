//! Argument vectors for the system `ssh`, `rsync` and `scp` (the program itself is not included).
//!
//! Every non-interactive invocation uses `BatchMode=yes` so a prompt for a password or a host key
//! fails instead of hanging. Connections share one ControlMaster socket per host when the engine
//! gives a control path: an explicit master (`ssh -M -N`) owns the connection and the other
//! invocations attach with `ControlMaster=no` (falling back to their own connection while the
//! master is not up yet).

use std::path::Path;

use crate::quote;
use crate::server::{ServerSource, SshServer};
use crate::{RemoteError, RemoteResult};

/// How to run `ssh`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshOptions {
    /// The `ssh` program (name on PATH or a path).
    pub program: String,
    /// ControlPath of the host's master socket (`None` = no connection sharing).
    pub control_path: Option<String>,
    /// `ConnectTimeout` in seconds.
    pub connect_timeout: Option<u32>,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self { program: "ssh".into(), control_path: None, connect_timeout: Some(15) }
    }
}

fn opt(v: impl Into<String>) -> [String; 2] {
    ["-o".to_string(), v.into()]
}

impl SshOptions {
    /// Options shared by ssh invocations: BatchMode (unless interactive), the timeout and the
    /// control socket.
    fn base(&self, batch: bool, share: bool) -> Vec<String> {
        let mut out = Vec::new();
        if batch {
            out.extend(opt("BatchMode=yes"));
        }
        if let Some(t) = self.connect_timeout {
            out.extend(opt(format!("ConnectTimeout={t}")));
        }
        match (&self.control_path, share) {
            (Some(p), true) => {
                out.extend(opt("ControlMaster=no"));
                out.extend(opt(format!("ControlPath={p}")));
            }
            _ => out.extend(opt("ControlPath=none")),
        }
        out
    }
}

/// `ssh … -- dest '<remote command>'` (BatchMode). The command is passed as one word; build it
/// with [`quote`] helpers.
pub fn ssh_exec(server: &SshServer, opts: &SshOptions, remote_cmd: &str) -> RemoteResult<Vec<String>> {
    let mut out = opts.base(true, true);
    out.extend(server.connect_options());
    out.push("--".into());
    out.push(server.destination()?);
    out.push(remote_cmd.to_string());
    Ok(out)
}

/// An interactive session (Tools › Start SSH Session…): no BatchMode, so ssh may ask for a
/// passphrase or to confirm a host key in the terminal.
pub fn ssh_interactive(server: &SshServer, opts: &SshOptions) -> RemoteResult<Vec<String>> {
    let mut out = opts.base(false, true);
    out.extend(server.connect_options());
    out.push("--".into());
    out.push(server.destination()?);
    Ok(out)
}

/// The control master for a host (`None` without a control path).
pub fn ssh_master(server: &SshServer, opts: &SshOptions) -> RemoteResult<Option<Vec<String>>> {
    let Some(path) = &opts.control_path else { return Ok(None) };
    let mut out = Vec::new();
    out.extend(opt("BatchMode=yes"));
    if let Some(t) = opts.connect_timeout {
        out.extend(opt(format!("ConnectTimeout={t}")));
    }
    out.extend(opt("ControlMaster=yes"));
    out.extend(opt(format!("ControlPath={path}")));
    out.extend(opt("ControlPersist=60"));
    out.extend(opt("ServerAliveInterval=30"));
    out.push("-N".into());
    out.extend(server.connect_options());
    out.push("--".into());
    out.push(server.destination()?);
    Ok(Some(out))
}

/// Ask a master to exit (`ssh -O exit`).
pub fn ssh_exit_master(server: &SshServer, opts: &SshOptions) -> RemoteResult<Option<Vec<String>>> {
    let Some(path) = &opts.control_path else { return Ok(None) };
    let mut out = Vec::new();
    out.extend(opt("BatchMode=yes"));
    out.extend(opt(format!("ControlPath={path}")));
    out.extend(["-O".to_string(), "exit".into()]);
    out.extend(server.connect_options());
    out.push("--".into());
    out.push(server.destination()?);
    Ok(Some(out))
}

/// How a tunnel authenticates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TunnelAuth {
    /// Keys and the agent only (`BatchMode=yes`: never a prompt).
    #[default]
    Batch,
    /// A password, answered by `SSH_ASKPASS` (no public keys tried).
    Password,
    /// A key whose passphrase `SSH_ASKPASS` answers.
    Passphrase,
}

/// `ssh -N -L 127.0.0.1:<local>:<host>:<port>`: a tunnel of its own (never through the master,
/// so killing the process closes the forward).
pub fn ssh_tunnel(
    server: &SshServer,
    opts: &SshOptions,
    local_port: u16,
    target_host: &str,
    target_port: u16,
) -> RemoteResult<Vec<String>> {
    ssh_tunnel_auth(server, opts, local_port, target_host, target_port, TunnelAuth::Batch)
}

/// [`ssh_tunnel`] with an authentication mode. A host seen for the first time is added to
/// `known_hosts` (`StrictHostKeyChecking=accept-new`, what answering "yes" does); a changed host
/// key is still refused.
pub fn ssh_tunnel_auth(
    server: &SshServer,
    opts: &SshOptions,
    local_port: u16,
    target_host: &str,
    target_port: u16,
    auth: TunnelAuth,
) -> RemoteResult<Vec<String>> {
    let host = target_host.trim();
    if host.is_empty() || host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(RemoteError::Invalid(format!("invalid tunnel target host `{target_host}`")));
    }
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    let mut out = opts.base(auth == TunnelAuth::Batch, false);
    match auth {
        TunnelAuth::Batch => {}
        TunnelAuth::Password => {
            out.extend(opt("PreferredAuthentications=password,keyboard-interactive"));
            out.extend(opt("PubkeyAuthentication=no"));
            out.extend(opt("NumberOfPasswordPrompts=1"));
        }
        TunnelAuth::Passphrase => {
            out.extend(opt("PreferredAuthentications=publickey"));
            out.extend(opt("NumberOfPasswordPrompts=1"));
        }
    }
    out.extend(opt("StrictHostKeyChecking=accept-new"));
    out.extend(opt("ExitOnForwardFailure=yes"));
    out.extend(opt("ServerAliveInterval=30"));
    out.extend(opt("ServerAliveCountMax=3"));
    out.push("-N".into());
    out.push("-L".into());
    out.push(format!("127.0.0.1:{local_port}:{host}:{target_port}"));
    out.extend(server.connect_options());
    out.push("--".into());
    out.push(server.destination()?);
    Ok(out)
}

/// Upload (local → remote) or download.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Direction {
    Upload,
    Download,
}

/// One rsync run.
#[derive(Clone, Debug)]
pub struct RsyncSpec<'a> {
    pub direction: Direction,
    /// Absolute local file or directory.
    pub local: &'a Path,
    /// Remote file or directory (absolute, `~/…` or relative to the home directory).
    pub remote: &'a str,
    /// Directories sync their contents (`dir/` → `dir/`).
    pub dir: bool,
    /// `--exclude` patterns.
    pub excludes: &'a [String],
    pub dry_run: bool,
    /// Delete target items that do not exist in the source.
    pub delete: bool,
    /// Skip files that are newer on the receiver.
    pub update: bool,
    /// Only these paths (relative to the source directory), from a file.
    pub files_from: Option<&'a Path>,
}

/// Remote paths handed to rsync or scp go through the remote shell on older versions, so only
/// characters that mean the same quoted or not are accepted. `~/x` becomes `x` (both start in the
/// home directory).
pub fn transfer_path(remote: &str) -> RemoteResult<String> {
    let p = match remote {
        "~" | "~/" | "" => ".".to_string(),
        r => r.strip_prefix("~/").unwrap_or(r).to_string(),
    };
    let safe = p.chars().all(|c| c.is_ascii_alphanumeric() || "-_./+,=@%".contains(c));
    if !safe || p.starts_with('-') {
        return Err(RemoteError::Invalid(format!(
            "the remote path `{remote}` has characters rsync/scp cannot pass safely (use letters, digits and - _ . / + , = @ %)"
        )));
    }
    Ok(p)
}

/// Quote a word of rsync's `-e` command (rsync splits on spaces and honours quotes, but not
/// backslashes).
fn rsync_word(w: &str) -> RemoteResult<String> {
    if !w.is_empty() && !w.contains([' ', '\'', '"']) {
        return Ok(w.to_string());
    }
    if !w.contains('\'') {
        return Ok(format!("'{w}'"));
    }
    if !w.contains('"') {
        return Ok(format!("\"{w}\""));
    }
    Err(RemoteError::Invalid(format!("cannot pass `{w}` to rsync's -e")))
}

fn local_str(p: &Path) -> RemoteResult<String> {
    if !p.is_absolute() {
        return Err(RemoteError::Invalid(format!("not an absolute local path: {}", p.display())));
    }
    p.to_str().map(str::to_string).ok_or_else(|| RemoteError::Invalid("the local path is not valid UTF-8".into()))
}

/// `rsync` arguments for a spec.
pub fn rsync_args(server: &SshServer, opts: &SshOptions, spec: &RsyncSpec<'_>) -> RemoteResult<Vec<String>> {
    let mut e = vec![rsync_word(&opts.program)?];
    for w in opts.base(true, true).iter().chain(server.connect_options().iter()) {
        e.push(rsync_word(w)?);
    }
    let mut out = vec!["-rlptz".to_string(), "--itemize-changes".into(), "-e".into(), e.join(" ")];
    if spec.dry_run {
        out.push("--dry-run".into());
    }
    if spec.update {
        out.push("--update".into());
    }
    if spec.delete {
        out.push("--delete".into());
    }
    if let Some(f) = spec.files_from {
        out.push(format!("--files-from={}", local_str(f)?));
    }
    for x in spec.excludes {
        if x.contains(['\n', '\r']) {
            return Err(RemoteError::Invalid("an exclude pattern has a line break".into()));
        }
        out.push(format!("--exclude={x}"));
    }
    let mut local = local_str(spec.local)?;
    let mut remote = format!("{}:{}", server.transfer_host()?, transfer_path(spec.remote)?);
    if spec.dir {
        if !local.ends_with('/') {
            local.push('/');
        }
        if !remote.ends_with('/') {
            remote.push('/');
        }
    }
    match spec.direction {
        Direction::Upload => out.extend([local, remote]),
        Direction::Download => out.extend([remote, local]),
    }
    Ok(out)
}

/// Bounded, incremental remote-project snapshot. Symlinks are skipped; the working copy is
/// refreshed separately against a baseline, never by an unconditional rsync overwrite.
/// Per-directory gitignore filters support common ignore patterns (not Git's full semantics).
pub fn project_download_args(
    server: &SshServer,
    opts: &SshOptions,
    local: &Path,
    remote: &str,
) -> RemoteResult<Vec<String>> {
    let excludes: Vec<String> = crate::deploy::DEFAULT_EXCLUDES
        .iter()
        .copied()
        .chain(["*.codecraft-base-*", "*.codecraft-upload-*"])
        .map(str::to_string)
        .collect();
    let mut args = rsync_args(
        server,
        opts,
        &RsyncSpec {
            direction: Direction::Download,
            local,
            remote,
            dir: true,
            excludes: &excludes,
            dry_run: false,
            delete: true,
            update: false,
            files_from: None,
        },
    )?;
    // Insert before positional arguments (works with rsync 2.6.9 shipped on macOS).
    let at = args.len().saturating_sub(2);
    args.splice(
        at..at,
        [
            "--no-links".into(),
            "--checksum".into(),
            format!("--max-size={}", crate::mirror::MAX_FILE_BYTES),
            "--filter=:- .gitignore".into(),
        ],
    );
    Ok(args)
}

/// `scp` arguments (rsync fallback). A directory upload copies its contents into `remote`
/// (`local/.`); a directory download copies `remote` into the parent of `local`, so the two must
/// share a base name.
pub fn scp_args(
    server: &SshServer,
    opts: &SshOptions,
    direction: Direction,
    local: &Path,
    remote: &str,
    dir: bool,
) -> RemoteResult<Vec<String>> {
    let mut out = opts.base(true, true);
    out.push("-p".into());
    if dir {
        out.push("-r".into());
    }
    if server.source == ServerSource::CodeCraft {
        if let Some(p) = server.port {
            out.extend(["-P".to_string(), p.to_string()]);
        }
        if let Some(i) = &server.identity_file {
            out.extend(["-i".to_string(), i.clone()]);
            out.extend(opt("IdentitiesOnly=yes"));
        }
    }
    out.push("--".into());
    let rpath = transfer_path(remote)?;
    let host = server.transfer_host()?;
    match (direction, dir) {
        (Direction::Upload, true) => out.extend([format!("{}/.", local_str(local)?), format!("{host}:{rpath}/")]),
        (Direction::Upload, false) => out.extend([local_str(local)?, format!("{host}:{rpath}")]),
        (Direction::Download, true) => {
            let rname = Path::new(&rpath).file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let lname = local.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if rname.is_empty() || rname != lname {
                return Err(RemoteError::Invalid(
                    "scp can only download a folder into a local folder of the same name; install rsync".into(),
                ));
            }
            let parent = local.parent().ok_or_else(|| RemoteError::Invalid("no parent folder".into()))?;
            out.extend([format!("{host}:{rpath}"), local_str(parent)?]);
        }
        (Direction::Download, false) => out.extend([format!("{host}:{rpath}"), local_str(local)?]),
    }
    Ok(out)
}

/// Remote command: list a directory (`LC_ALL=C ls -lAn`), parsed by [`crate::listing::parse_ls`].
pub fn ls_command(dir: &str) -> String {
    let d = if dir.ends_with('/') { dir.to_string() } else { format!("{dir}/") };
    quote::sh_c(&format!("LC_ALL=C ls -lAn -- {}", quote::quote_path(&d)))
}

/// Remote command: go to a directory, print its absolute path, then list it; parsed by
/// [`crate::listing::parse_pwd_ls`].
pub fn pwd_ls_command(dir: &str) -> String {
    quote::sh_c(&format!("cd -- {} && pwd && LC_ALL=C ls -lAn", quote::quote_path(dir)))
}

/// Remote command: write stdin to `path` atomically (temp file, then rename), creating parents.
pub fn put_command(path: &str) -> String {
    let parent = parent_of(path);
    let tmp = format!("{path}.codecraft-upload");
    quote::sh_c(&format!(
        "mkdir -p -- {} && cat > {} && mv -f -- {} {}",
        quote::quote_path(&parent),
        quote::quote_path(&tmp),
        quote::quote_path(&tmp),
        quote::quote_path(path)
    ))
}

/// Receive an upload into a temporary file, then compare against the accepted baseline before
/// renaming. Checking after the stream prevents edits made during a slow upload being lost.
/// `expected` is a separately uploaded immutable baseline; `None` requires an absent target.
pub fn checked_put_command(path: &str, expected: Option<&str>, token: &str) -> String {
    let parent = parent_of(path);
    let tmp = format!("{path}.codecraft-upload-{token}");
    let p = quote::quote_path(path);
    let check = match expected {
        Some(base) => format!("test -f {p} && test ! -L {p} && cmp -s -- {p} {}", quote::quote_path(base)),
        None => format!("test ! -e {p} && test ! -L {p}"),
    };
    let cleanup = match expected {
        Some(base) => format!("rm -f -- {} {}", quote::quote_path(&tmp), quote::quote_path(base)),
        None => format!("rm -f -- {}", quote::quote_path(&tmp)),
    };
    quote::sh_c(&format!(
        "umask 077; mkdir -p -- {} && trap {} 0 && {{ if test -f {p} && test ! -L {p}; then cp -p -- {p} {}; fi; }} && cat > {} && if {check}; then mv -f -- {} {p}; else echo 'Remote file changed: refresh and compare before uploading' >&2; exit 3; fi",
        quote::quote_path(&parent),
        quote::sh_quote(&cleanup),
        quote::quote_path(&tmp),
        quote::quote_path(&tmp),
        quote::quote_path(&tmp)
    ))
}

/// Remote command: print a file.
pub fn cat_command(path: &str) -> String {
    quote::sh_c(&format!("cat -- {}", quote::quote_path(path)))
}

/// Remote command: `mkdir -p`.
pub fn mkdir_command(path: &str) -> String {
    quote::sh_c(&format!("mkdir -p -- {}", quote::quote_path(path)))
}

/// Remote command: rename (fails if the target exists).
pub fn rename_command(from: &str, to: &str) -> String {
    quote::sh_c(&format!(
        "if [ -e {t} ]; then echo 'target exists' >&2; exit 1; fi; mv -- {f} {t}",
        f = quote::quote_path(from),
        t = quote::quote_path(to)
    ))
}

/// Remote command: delete files or directories (recursive).
pub fn delete_command(paths: &[String]) -> RemoteResult<String> {
    for p in paths {
        let t = p.trim_end_matches('/');
        if t.is_empty() || t == "~" || t == "." || t == ".." || t.ends_with("/..") || t.ends_with("/.") {
            return Err(RemoteError::Invalid(format!("refusing to delete `{p}`")));
        }
    }
    let list: Vec<String> = paths.iter().map(|p| quote::quote_path(p)).collect();
    Ok(quote::sh_c(&format!("rm -rf -- {}", list.join(" "))))
}

/// The parent of a remote path (`/a/b` → `/a`, `b` → `.`, `~/b` → `~`).
pub fn parent_of(path: &str) -> String {
    let t = path.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => t.get(..i).unwrap_or(".").to_string(),
        None => ".".into(),
    }
}

/// Join a remote directory and a relative path with `/`.
pub fn join(dir: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches("./").trim_start_matches('/');
    if rel.is_empty() || rel == "." {
        return dir.to_string();
    }
    if dir.is_empty() || dir == "." {
        return rel.to_string();
    }
    format!("{}/{rel}", dir.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::ServerConfig;

    fn own() -> SshServer {
        ServerConfig {
            name: "box".into(),
            host: "h.example".into(),
            port: Some(2222),
            user: "me".into(),
            identity_file: "/k/id".into(),
        }
        .to_server()
    }

    fn alias() -> SshServer {
        SshServer {
            name: "web".into(),
            host: "w".into(),
            port: None,
            user: None,
            identity_file: None,
            source: ServerSource::SshConfig,
        }
    }

    fn shared() -> SshOptions {
        SshOptions { program: "ssh".into(), control_path: Some("/tmp/cc-s/%C".into()), connect_timeout: Some(10) }
    }

    #[test]
    fn exec_uses_batch_mode_control_path_and_separator() {
        let a = ssh_exec(&own(), &shared(), "sh -c 'ls'").unwrap();
        assert_eq!(
            a,
            vec![
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=/tmp/cc-s/%C",
                "-p",
                "2222",
                "-l",
                "me",
                "-i",
                "/k/id",
                "-o",
                "IdentitiesOnly=yes",
                "--",
                "h.example",
                "sh -c 'ls'"
            ]
        );
        let i = ssh_interactive(&alias(), &SshOptions::default()).unwrap();
        assert!(!i.iter().any(|w| w.contains("BatchMode")));
        assert_eq!(i.last().map(String::as_str), Some("web"));
    }

    #[test]
    fn master_and_exit() {
        let m = ssh_master(&alias(), &shared()).unwrap().unwrap();
        assert!(m.contains(&"ControlMaster=yes".to_string()));
        assert!(m.contains(&"-N".to_string()));
        let x = ssh_exit_master(&alias(), &shared()).unwrap().unwrap();
        assert!(x.windows(2).any(|w| w == ["-O", "exit"]));
        assert!(ssh_master(&alias(), &SshOptions::default()).unwrap().is_none());
    }

    #[test]
    fn tunnel_binds_loopback_and_never_shares() {
        let t = ssh_tunnel(&alias(), &shared(), 40001, "db.internal", 5432).unwrap();
        assert!(t.contains(&"127.0.0.1:40001:db.internal:5432".to_string()));
        assert!(t.contains(&"ControlPath=none".to_string()));
        assert!(t.contains(&"ExitOnForwardFailure=yes".to_string()));
        let v6 = ssh_tunnel(&alias(), &shared(), 1, "::1", 5).unwrap();
        assert!(v6.contains(&"127.0.0.1:1:[::1]:5".to_string()));
        assert!(ssh_tunnel(&alias(), &shared(), 1, "-oProxyCommand=x", 5).is_err());
        assert!(ssh_tunnel(&alias(), &shared(), 1, "a b", 5).is_err());
    }

    #[test]
    fn tunnel_auth_modes() {
        let has = |v: &[String], o: &str| v.windows(2).any(|w| w[0] == "-o" && w[1] == o);
        let batch = ssh_tunnel(&alias(), &shared(), 1, "db", 5).unwrap();
        assert!(has(&batch, "BatchMode=yes") && has(&batch, "StrictHostKeyChecking=accept-new"));
        let pw = ssh_tunnel_auth(&alias(), &shared(), 1, "db", 5, TunnelAuth::Password).unwrap();
        assert!(!has(&pw, "BatchMode=yes"), "SSH_ASKPASS needs prompts: {pw:?}");
        assert!(has(&pw, "PubkeyAuthentication=no") && has(&pw, "NumberOfPasswordPrompts=1"));
        let key = ssh_tunnel_auth(&alias(), &shared(), 1, "db", 5, TunnelAuth::Passphrase).unwrap();
        assert!(!has(&key, "BatchMode=yes") && has(&key, "PreferredAuthentications=publickey"));
    }

    #[test]
    fn rsync_upload_dir_with_excludes_and_dry_run() {
        let ex = vec!["target".to_string(), "/.git".to_string()];
        let spec = RsyncSpec {
            direction: Direction::Upload,
            local: Path::new("/p/proj"),
            remote: "~/proj",
            dir: true,
            excludes: &ex,
            dry_run: true,
            delete: true,
            update: false,
            files_from: None,
        };
        let a = rsync_args(&own(), &shared(), &spec).unwrap();
        assert_eq!(a[0], "-rlptz");
        assert_eq!(a[1], "--itemize-changes");
        assert_eq!(a[2], "-e");
        assert_eq!(
            a[3],
            "ssh -o BatchMode=yes -o ConnectTimeout=10 -o ControlMaster=no -o ControlPath=/tmp/cc-s/%C -p 2222 -l me -i /k/id -o IdentitiesOnly=yes"
        );
        assert!(a.contains(&"--dry-run".to_string()));
        assert!(a.contains(&"--delete".to_string()));
        assert!(a.contains(&"--exclude=/.git".to_string()));
        assert_eq!(&a[a.len() - 2..], ["/p/proj/", "me@h.example:proj/"]);
    }

    #[test]
    fn rsync_download_file_and_bad_paths() {
        let spec = RsyncSpec {
            direction: Direction::Download,
            local: Path::new("/p/a.rs"),
            remote: "/srv/a.rs",
            dir: false,
            excludes: &[],
            dry_run: false,
            delete: false,
            update: true,
            files_from: Some(Path::new("/tmp/list")),
        };
        let a = rsync_args(&alias(), &SshOptions::default(), &spec).unwrap();
        assert!(a.contains(&"--files-from=/tmp/list".to_string()));
        assert!(a.contains(&"--update".to_string()));
        assert_eq!(&a[a.len() - 2..], ["web:/srv/a.rs", "/p/a.rs"]);
        let bad = RsyncSpec { remote: "/srv/my file", ..spec.clone() };
        assert!(rsync_args(&alias(), &SshOptions::default(), &bad).is_err());
        let bad = RsyncSpec { remote: "/srv/$(x)", ..spec.clone() };
        assert!(rsync_args(&alias(), &SshOptions::default(), &bad).is_err());
        let rel = RsyncSpec { local: Path::new("rel"), ..spec };
        assert!(rsync_args(&alias(), &SshOptions::default(), &rel).is_err());
        // A control path with a space is quoted for rsync's -e parser.
        let o = SshOptions { control_path: Some("/tmp/a b/%C".into()), ..SshOptions::default() };
        let spec = RsyncSpec { remote: "x", local: Path::new("/x"), ..bad_spec() };
        let a = rsync_args(&alias(), &o, &spec).unwrap();
        assert!(a[3].contains("'ControlPath=/tmp/a b/%C'"));
    }

    fn bad_spec() -> RsyncSpec<'static> {
        RsyncSpec {
            direction: Direction::Upload,
            local: Path::new("/x"),
            remote: "x",
            dir: false,
            excludes: &[],
            dry_run: false,
            delete: false,
            update: false,
            files_from: None,
        }
    }

    #[test]
    fn project_snapshot_is_incremental_bounded_and_filtered() {
        let args = project_download_args(&alias(), &shared(), Path::new("/cache/snapshot"), "~/app").unwrap();
        for flag in [
            "--delete",
            "--checksum",
            "--no-links",
            "--max-size=52428800",
            "--filter=:- .gitignore",
            "--exclude=target",
            "--exclude=node_modules",
            "--exclude=.git",
        ] {
            assert!(args.iter().any(|a| a == flag), "missing {flag}: {args:?}");
        }
        assert_eq!(args.last().map(String::as_str), Some("/cache/snapshot/"));
        assert!(ssh_master(&alias(), &shared()).unwrap().unwrap().iter().any(|a| a == "ControlPersist=60"));
    }

    #[test]
    fn scp_fallback() {
        let up =
            scp_args(&own(), &SshOptions::default(), Direction::Upload, Path::new("/p/d"), "/srv/d", true).unwrap();
        assert!(up.windows(2).any(|w| w == ["-P", "2222"]));
        assert_eq!(&up[up.len() - 2..], ["/p/d/.", "me@h.example:/srv/d/"]);
        let down =
            scp_args(&alias(), &SshOptions::default(), Direction::Download, Path::new("/p/d"), "/srv/d", true).unwrap();
        assert_eq!(&down[down.len() - 2..], ["web:/srv/d", "/p"]);
        assert!(
            scp_args(&alias(), &SshOptions::default(), Direction::Download, Path::new("/p/e"), "/srv/d", true).is_err()
        );
        let f =
            scp_args(&alias(), &SshOptions::default(), Direction::Download, Path::new("/p/f"), "~/f", false).unwrap();
        assert_eq!(&f[f.len() - 2..], ["web:f", "/p/f"]);
    }

    #[test]
    fn remote_commands_quote_paths() {
        assert_eq!(ls_command("~/my dir"), quote::sh_c("LC_ALL=C ls -lAn -- \"$HOME\"/'my dir/'"));
        assert_eq!(pwd_ls_command("~"), quote::sh_c("cd -- \"$HOME\" && pwd && LC_ALL=C ls -lAn"));
        assert!(put_command("/srv/a b.rs").contains("a b.rs.codecraft-upload"));
        assert_eq!(cat_command("/x;y"), quote::sh_c("cat -- '/x;y'"));
        assert!(delete_command(&["/".into()]).is_err());
        assert!(delete_command(&["~".into()]).is_err());
        assert!(delete_command(&["/a/..".into()]).is_err());
        assert_eq!(delete_command(&["/a b".into()]).unwrap(), quote::sh_c("rm -rf -- '/a b'"));
        assert!(rename_command("/a", "/b").contains("mv -- /a /b"));
    }

    #[cfg(unix)]
    #[test]
    fn checked_upload_compares_after_receiving_and_removes_temporary_files() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a file");
        let base = dir.path().join("expected");
        std::fs::write(&target, "remote edit").unwrap();
        std::fs::write(&base, "accepted").unwrap();
        let script = checked_put_command(target.to_str().unwrap(), Some(base.to_str().unwrap()), "fixture");
        let mut child =
            Command::new("sh").arg("-c").arg(script).stdin(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"local edit").unwrap();
        assert!(!child.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "remote edit");
        assert!(!base.exists());
        assert!(!dir.path().join("a file.codecraft-upload-fixture").exists());
        let script = checked_put_command(target.to_str().unwrap(), None, "fixture");
        assert!(
            !Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "remote edit");
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&base, "remote edit").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script = checked_put_command(target.to_str().unwrap(), Some(base.to_str().unwrap()), "success");
        let mut child = Command::new("sh").arg("-c").arg(script).stdin(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(b"accepted local edit").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "accepted local edit");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o755);
        assert!(!base.exists());
    }

    #[test]
    fn path_helpers() {
        assert_eq!(parent_of("/a/b"), "/a");
        assert_eq!(parent_of("/a"), "/");
        assert_eq!(parent_of("b"), ".");
        assert_eq!(parent_of("~/b/"), "~");
        assert_eq!(join("/srv/", "src/a.rs"), "/srv/src/a.rs");
        assert_eq!(join("/srv", ""), "/srv");
        assert_eq!(join("", "x"), "x");
        assert_eq!(transfer_path("~").unwrap(), ".");
        assert_eq!(transfer_path("~/a").unwrap(), "a");
        assert!(transfer_path("-a").is_err());
    }
}
