//! Managed source only. Developer checkouts must never be passed to mutation methods.
use crate::{Lock, Result, invalid};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_REPO_URL: &str = "https://github.com/Saimirbaci/codecraft.git";
pub const USER_BRANCH: &str = "codecraft-user";

/// Accept explicit transports, never git's external-helper or option syntax.
pub fn validate_url(url: &str) -> Result<()> {
    if url.len() > 4096 || url.chars().any(|c| c.is_control()) {
        return Err(invalid("invalid repository URL"));
    }
    let valid = ["https://", "ssh://", "file://"].iter().any(|s| url.starts_with(s) && url.len() > s.len());
    if !valid || url.contains("--upload-pack") || url.contains('\\') {
        return Err(invalid("repository URL must use https://, ssh:// or file://"));
    }
    let (scheme, rest) = url.split_once("://").ok_or_else(|| invalid("invalid repository URL"))?;
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit('@').next().unwrap_or_default();
    if scheme != "file"
        && (host.starts_with('-')
            || host.is_empty()
            || !host.chars().all(|c| c.is_ascii_alphanumeric() || ".-_:[]".contains(c)))
    {
        return Err(invalid("invalid repository host"));
    }
    if (scheme != "file"
        && (authority.is_empty() || authority.starts_with('-') || authority.chars().any(char::is_whitespace)))
        || (scheme == "https" && authority.contains('@'))
        || (scheme == "ssh" && authority.split_once('@').is_some_and(|(user, _)| user.contains(':')))
        || (scheme == "file" && !matches!(authority, "" | "localhost"))
    {
        return Err(invalid(
            "repository URLs cannot contain credentials or option-like hosts; use git's credential configuration",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ManagedSource {
    pub base: PathBuf,
    pub url: String,
    pub git: PathBuf,
}
impl ManagedSource {
    pub fn new(base: PathBuf, url: String) -> Result<Self> {
        validate_url(&url)?;
        Ok(Self { base, url, git: "git".into() })
    }
    pub fn path(&self) -> PathBuf {
        self.base.join("source")
    }
    fn lock(&self) -> Result<Lock> {
        Lock::acquire(&self.base.join("source.lock"))
    }
    pub fn read(&self, args: &[&str]) -> Result<String> {
        git(&self.git, &self.path(), args)
    }
    fn owned(&self) -> Result<()> {
        for path in [self.path(), self.path().join(".git")] {
            if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
                return Err(invalid("managed source cannot be a symbolic link"));
            }
        }
        if bounded_read(&self.path().join(".git/codecraft-managed"), 64)? != b"codecraft managed source v1\n" {
            return Err(invalid("source is not a CodeCraft managed clone"));
        }
        verify_checkout(&self.git, &self.path())
    }
    fn clean(&self) -> Result<()> {
        self.owned()?;
        if !self.read(&["status", "--porcelain", "--untracked-files=all", "--ignored=matching"])?.is_empty() {
            return Err(invalid(
                "managed source has uncommitted or ignored files; commit or move them before updating",
            ));
        }
        if self.read(&["branch", "--show-current"])? != USER_BRANCH {
            return Err(invalid("managed source is not on codecraft-user; refusing to change branches"));
        }
        Ok(())
    }
    pub fn prepare(&self, commit: Option<&str>, mut progress: impl FnMut(&str)) -> Result<PathBuf> {
        validate_url(&self.url)?;
        let _lock = self.lock()?;
        if self.path().exists() {
            self.clean()?;
            return Ok(self.path());
        }
        if commit.is_some_and(|c| c.len() < 7 || c.len() > 40 || !c.bytes().all(|b| b.is_ascii_hexdigit())) {
            return Err(invalid("invalid build commit"));
        }
        progress("Setting up CodeCraft's source: cloning repository…");
        let temporary = self.base.join("source-preparing");
        if temporary.exists() {
            return Err(invalid("source-preparing already exists; inspect it before retrying"));
        }
        let result = (|| {
            let destination = temporary.to_str().ok_or_else(|| invalid("source path is not UTF-8"))?;
            git(
                &self.git,
                &self.base,
                &["clone", "--filter=blob:none", "--no-checkout", "--", &self.url, destination],
            )?;
            verify_checkout(&self.git, &temporary)?;
            progress("Setting up CodeCraft's source: checking out user branch…");
            git(&self.git, &temporary, &["checkout", "-b", USER_BRANCH, commit.unwrap_or("HEAD"), "--"])?;
            std::fs::write(temporary.join(".git/codecraft-managed"), b"codecraft managed source v1\n")?;
            std::fs::rename(&temporary, self.path())?;
            Ok(self.path())
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&temporary);
        }
        result
    }
    /// Fetch does not move remote tracking branches. Failed rebases restore the original user branch.
    pub fn update(&self) -> Result<String> {
        let _lock = self.lock()?;
        self.clean()?;
        let gitdir = self.path().join(".git");
        let snapshots: Vec<_> = ["FETCH_HEAD", "ORIG_HEAD"]
            .into_iter()
            .map(|name| {
                let path = gitdir.join(name);
                let bytes = match bounded_read(&path, 65536) {
                    Ok(bytes) => Some(bytes),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                    Err(e) => return Err(e),
                };
                Ok((path, bytes))
            })
            .collect::<Result<_>>()?;
        let result = (|| {
            self.read(&["fetch", "--no-tags", "--", &self.url])?;
            match self.read(&[
                "-c",
                "user.name=CodeCraft",
                "-c",
                "user.email=noreply@codecraft.invalid",
                "rebase",
                "FETCH_HEAD",
            ]) {
                Ok(out) => Ok(out),
                Err(error) => {
                    let files = self.read(&["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
                    self.read(&["rebase", "--abort"])?;
                    Err(invalid(format!("upstream update aborted; conflicting files: {files}\n{error}")))
                }
            }
        })();
        if result.is_err() {
            for (path, bytes) in snapshots {
                match bytes {
                    Some(bytes) => crate::atomic_write(&path, &bytes)?,
                    None => {
                        if path.exists() {
                            std::fs::remove_file(path)?;
                        }
                    }
                }
            }
        }
        result
    }

    /// Commit exactly the accepted paths. Refuse unrelated staged changes.
    pub fn commit(&self, paths: &[PathBuf], message: &str) -> Result<String> {
        let _lock = self.lock()?;
        self.owned()?;
        if self.read(&["branch", "--show-current"])? != USER_BRANCH
            || !self.read(&["diff", "--cached", "--name-only"])?.is_empty()
        {
            return Err(invalid("commit requires codecraft-user with an empty index"));
        }
        if paths.is_empty() {
            return Err(invalid("no accepted files"));
        }
        for path in paths {
            if path.as_os_str().is_empty() || path.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                return Err(invalid("accepted path must be relative without parent components"));
            }
        }
        let mut args = vec!["add", "--"];
        let names: Vec<_> = paths
            .iter()
            .map(|p| p.to_str().ok_or_else(|| invalid("non UTF-8 accepted path")))
            .collect::<Result<_>>()?;
        args.extend(names);
        self.read(&args)?;
        let name = self.read(&["config", "user.name"]).unwrap_or_else(|_| "CodeCraft".into());
        let email = self.read(&["config", "user.email"]).unwrap_or_else(|_| "noreply@codecraft.invalid".into());
        let message: String = message.chars().take(4096).collect();
        self.read(&[
            "-c",
            &format!("user.name={name}"),
            "-c",
            &format!("user.email={email}"),
            "commit",
            "-m",
            if message.trim().is_empty() { "CodeCraft feedback" } else { &message },
        ])?;
        self.read(&["rev-parse", "HEAD"])
    }
    pub fn revert(&self, commit: &str) -> Result<String> {
        let _lock = self.lock()?;
        self.clean()?;
        if commit.len() != 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid commit"));
        }
        match self.read(&[
            "-c",
            "user.name=CodeCraft",
            "-c",
            "user.email=noreply@codecraft.invalid",
            "revert",
            "--no-edit",
            commit,
        ]) {
            Ok(out) => Ok(out),
            Err(e) => {
                self.read(&["revert", "--abort"])?;
                Err(e)
            }
        }
    }
}
fn bounded_read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    use std::io::Read;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(invalid("managed git metadata is not a bounded regular file"));
    }
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid("managed git metadata exceeds limit"));
    }
    Ok(bytes)
}
fn verify_checkout(program: &Path, path: &Path) -> Result<()> {
    let reported = git(program, path, &["rev-parse", "--show-toplevel"])?;
    if Path::new(&reported).canonicalize()? != path.canonicalize()? {
        return Err(invalid("git configuration points outside the managed checkout; refusing mutation"));
    }
    Ok(())
}
/// Git repository context must come from cwd, never an inherited hook/difftool environment.
pub(crate) fn scoped_command(program: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(program);
    command.current_dir(cwd).env("GIT_TERMINAL_PROMPT", "0").env("RUSTUP_AUTO_INSTALL", "0");
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_PREFIX",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_SHALLOW_FILE",
        "GIT_GRAFT_FILE",
        "GIT_IMPLICIT_WORK_TREE",
    ] {
        command.env_remove(name);
    }
    command
}
/// Output is drained in bounded chunks, avoiding allocation proportional to hostile output.
pub fn git(program: &Path, cwd: &Path, args: &[&str]) -> Result<String> {
    use std::io::Read;
    let mut child = scoped_command(program, cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut readers = Vec::new();
    for pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Err(e) = std::thread::Builder::new().name("source-output".into()).spawn(move || {
            let mut pipe = pipe;
            let mut bytes = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let keep = n.min(65536_usize.saturating_sub(bytes.len()));
                        bytes.extend_from_slice(chunk.get(..keep).unwrap_or_default());
                    }
                }
            }
            let _ = tx.send(bytes);
        }) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        readers.push(rx);
    }
    let started = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => (),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if started.elapsed() > std::time::Duration::from_secs(120) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("git operation timed out; check network and credentials"));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let output: Vec<_> = readers
        .into_iter()
        .map(|rx| rx.recv_timeout(std::time::Duration::from_millis(100)).unwrap_or_default())
        .collect();
    let stdout = String::from_utf8_lossy(output.first().map(Vec::as_slice).unwrap_or_default()).trim().to_string();
    if !status.success() {
        return Err(invalid(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&"operation"),
            String::from_utf8_lossy(output.get(1).map(Vec::as_slice).unwrap_or_default())
        )));
    }
    Ok(stdout)
}

/// Stream the full tracked diff and untracked file contents into a provenance fingerprint.
/// This never writes the developer index or follows untracked symlinks.
pub fn dirty_fingerprint(program: &Path, source: &Path) -> Result<Option<String>> {
    use std::hash::Hasher;
    use std::io::Read;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut child = scoped_command(program, source)
        .args(["diff", "--no-ext-diff", "--no-textconv", "--binary", "HEAD", "--"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = (|| {
        let mut total = 0_u64;
        if let Some(mut pipe) = child.stdout.take() {
            let mut buf = [0; 8192];
            loop {
                let n = pipe.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                total = total.saturating_add(n as u64);
                if total > 1024 * 1024 * 1024 {
                    return Err(invalid("source diff exceeds provenance limit"));
                }
                hash.write(buf.get(..n).unwrap_or_default());
            }
        }
        Ok(total)
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let total = result?;
    if !status.success() {
        return Err(invalid("could not fingerprint source diff"));
    }
    let names = git(program, source, &["ls-files", "--others", "--exclude-standard", "-z", "--"])?;
    if names.len() >= 65536 || (!names.is_empty() && !names.ends_with('\0')) {
        return Err(invalid("untracked path list exceeds provenance limit"));
    }
    for name in names.split('\0').filter(|s| !s.is_empty()) {
        let relative = Path::new(name);
        if relative.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err(invalid("invalid untracked path"));
        }
        hash.write(name.as_bytes());
        hash.write(&[0]);
        let path = source.join(relative);
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            hash.write(std::fs::read_link(path)?.as_os_str().as_encoded_bytes());
        } else if meta.is_file() {
            if meta.len() > 1024 * 1024 * 1024 {
                return Err(invalid("untracked source exceeds provenance limit"));
            }
            let mut file = std::fs::File::open(path)?;
            let mut buf = [0; 8192];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.write(buf.get(..n).unwrap_or_default());
            }
        } else {
            return Err(invalid("untracked source is not a regular file"));
        }
    }
    Ok((total > 0 || !names.is_empty()).then(|| format!("{:016x}", hash.finish())))
}

/// The workspace manifest's simple package string fields (version and rust-version).
/// CodeCraft's own manifest uses this table; callers retain build metadata as a fallback.
pub fn workspace_package_value(manifest: &str, key: &str) -> Option<String> {
    let mut package = false;
    for line in manifest.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            package = line == "[workspace.package]";
            continue;
        }
        if package
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            let value = value.trim().strip_prefix('"')?;
            return value.split_once('"').map(|(v, _)| v.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hostile_urls() {
        for url in [
            "--upload-pack=evil",
            "ext::sh -c evil",
            "git@host:repo",
            "https://host/\nrepo",
            "file://--upload-pack=evil",
            "https://user:token@github.com/private",
            "ssh://user:secret@host/repo",
            "ssh://-oProxyCommand=evil/repo",
            "ssh://git@-evil/repo",
            "ssh://git@%2devil/repo",
        ] {
            assert!(validate_url(url).is_err());
        }
        assert!(validate_url("file:///tmp/repo with spaces;$()'\"").is_ok());
    }
    #[test]
    fn local_prepare_commit_update_conflict_and_revert() {
        if !Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success()) {
            eprintln!("skipping: git missing");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let upstream = d.path().join("upstream with spaces");
        std::fs::create_dir(&upstream).unwrap();
        let run = |args: &[&str]| git(Path::new("git"), &upstream, args).unwrap();
        run(&["init", "-b", "main"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "user.email", "test@example.invalid"]);
        std::fs::write(upstream.join("file"), "original\n").unwrap();
        run(&["add", "--", "file"]);
        run(&["commit", "-m", "initial"]);
        let bare = d.path().join("remote ;$(touch SHOULD_NOT_EXIST).git");
        git(Path::new("git"), d.path(), &["clone", "--bare", "--", upstream.to_str().unwrap(), bare.to_str().unwrap()])
            .unwrap();
        let source = ManagedSource::new(d.path().join("managed"), format!("file://{}", bare.display())).unwrap();
        source.prepare(None, |_| {}).unwrap();
        assert!(dirty_fingerprint(Path::new("git"), &source.path()).unwrap().is_none());
        std::fs::write(source.path().join(".git/info/exclude"), "ignored-work\n").unwrap();
        std::fs::write(source.path().join("ignored-work"), "preserve ignored user work").unwrap();
        assert!(source.update().is_err());
        assert_eq!(std::fs::read_to_string(source.path().join("ignored-work")).unwrap(), "preserve ignored user work");
        std::fs::remove_file(source.path().join("ignored-work")).unwrap();
        assert_eq!(source.read(&["branch", "--show-current"]).unwrap(), USER_BRANCH);
        std::fs::write(source.path().join("file"), "user\n").unwrap();
        assert!(source.update().is_err());
        let commit = source.commit(&["file".into()], "feedback").unwrap();
        source.update().unwrap();
        source.revert(&commit).unwrap();
        assert_eq!(std::fs::read_to_string(source.path().join("file")).unwrap(), "original\n");
        std::fs::write(upstream.join("upstream-only"), "upstream addition\n").unwrap();
        run(&["add", "--", "upstream-only"]);
        run(&["commit", "-m", "nonconflicting upstream"]);
        run(&["push", "--", bare.to_str().unwrap(), "main"]);
        source.update().unwrap();
        assert!(source.path().join("upstream-only").is_file());
        assert!(!d.path().join("SHOULD_NOT_EXIST").exists());
        std::fs::write(source.path().join("file"), "user conflict\n").unwrap();
        source.commit(&["file".into()], "conflict").unwrap();
        let before = source.read(&["rev-parse", "HEAD"]).unwrap();
        std::fs::write(upstream.join("file"), "upstream\n").unwrap();
        run(&["commit", "-am", "upstream"]);
        run(&["push", "--", bare.to_str().unwrap(), "main"]);
        let error = source.update().unwrap_err().to_string();
        assert!(error.contains("file"), "{error}");
        assert_eq!(source.read(&["rev-parse", "HEAD"]).unwrap(), before);
        assert!(source.read(&["status", "--porcelain"]).unwrap().is_empty());
        std::fs::write(source.path().join(".git/codecraft-managed"), vec![b'x'; 1024]).unwrap();
        assert!(source.update().unwrap_err().to_string().contains("bounded"));
    }
    #[test]
    fn manifest_fields_and_complete_dirty_hash() {
        assert_eq!(workspace_package_value("[workspace.package]\nversion=\"0.4.0\" # next\nrust-version = \"1.99\"\n[dependencies]\nversion=\"bad\"","version").as_deref(),Some("0.4.0"));
        if !Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success()) {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        git(Path::new("git"), root, &["init", "-b", "main"]).unwrap();
        std::fs::write(root.join("file"), "original").unwrap();
        git(Path::new("git"), root, &["add", "--", "file"]).unwrap();
        git(
            Path::new("git"),
            root,
            &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-m", "initial"],
        )
        .unwrap();
        std::fs::write(root.join("file"), format!("{}first", "a".repeat(100000))).unwrap();
        let first = dirty_fingerprint(Path::new("git"), root).unwrap();
        std::fs::write(root.join("file"), format!("{}second", "a".repeat(100000))).unwrap();
        assert_ne!(first, dirty_fingerprint(Path::new("git"), root).unwrap());
        std::fs::write(root.join("new file"), "new").unwrap();
        let untracked = dirty_fingerprint(Path::new("git"), root).unwrap();
        std::fs::write(root.join("new file"), "changed").unwrap();
        assert_ne!(untracked, dirty_fingerprint(Path::new("git"), root).unwrap());
    }
    #[test]
    fn git_context_cannot_escape_the_requested_checkout() {
        if let Some(path) = std::env::var_os("CODECRAFT_TEST_GIT_CONTEXT") {
            let root = PathBuf::from(path);
            let actual = git(Path::new("git"), &root, &["rev-parse", "--show-toplevel"]).unwrap();
            assert_eq!(Path::new(&actual).canonicalize().unwrap(), root.canonicalize().unwrap());
            let other = PathBuf::from(std::env::var_os("GIT_WORK_TREE").unwrap());
            let hash = git(Path::new("git"), &root, &["hash-object", "-w", "--stdin"]).unwrap();
            assert!(
                git(Path::new("git"), &other, &["cat-file", "-e", &hash]).is_err(),
                "objects must not be written to the inherited repository"
            );
            return;
        }
        if !Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success()) {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let source = d.path().join("source");
        let other = d.path().join("other checkout");
        for root in [&source, &other] {
            std::fs::create_dir(root).unwrap();
            git(Path::new("git"), root, &["init", "-b", "main"]).unwrap();
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "source::tests::git_context_cannot_escape_the_requested_checkout"])
            .env("CODECRAFT_TEST_GIT_CONTEXT", &source)
            .env("GIT_DIR", other.join(".git"))
            .env("GIT_WORK_TREE", &other)
            .env("GIT_INDEX_FILE", other.join(".git/index"))
            .env("GIT_COMMON_DIR", other.join(".git"))
            .env("GIT_OBJECT_DIRECTORY", other.join(".git/objects"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
    }
}
