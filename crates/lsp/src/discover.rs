//! Finding a rust-analyzer binary that actually works.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use crate::client::Deadline;

/// How long a candidate may take to answer `--version`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const PROBE_POLL: Duration = Duration::from_millis(10);
/// We only read a path from `rustup which`; anything longer is not one.
const MAX_CAPTURED_OUTPUT: u64 = 64 * 1024;

pub(crate) fn find_rust_analyzer() -> Option<PathBuf> {
    let mut tried: Vec<PathBuf> = Vec::new();
    let mut works = |candidate: PathBuf| {
        if tried.contains(&candidate) {
            return None;
        }
        tried.push(candidate.clone());
        let ok = answers_version(&candidate, PROBE_TIMEOUT);
        log::debug!("rust-analyzer candidate {} {}", candidate.display(), if ok { "works" } else { "does not work" });
        ok.then_some(candidate)
    };
    for candidate in candidates(&|name| std::env::var_os(name)) {
        if let Some(found) = works(candidate) {
            return Some(found);
        }
    }
    rustup_which(PROBE_TIMEOUT).and_then(works)
}

/// `$RUST_ANALYZER`, then every `PATH` entry, then `$CARGO_HOME/bin` and `~/.cargo/bin`, in that
/// order. `var` reads the environment (injected so tests need not mutate it).
pub(crate) fn candidates(var: &dyn Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let exe = format!("rust-analyzer{}", std::env::consts::EXE_SUFFIX);
    let mut out = Vec::new();
    if let Some(explicit) = var("RUST_ANALYZER").filter(|value| !value.is_empty()) {
        out.push(PathBuf::from(explicit));
    }
    let mut add_file = |path: PathBuf| {
        if path.is_absolute() && path.is_file() && !out.contains(&path) {
            out.push(path);
        }
    };
    if let Some(path) = var("PATH") {
        for dir in std::env::split_paths(&path) {
            add_file(dir.join(&exe));
        }
    }
    if let Some(cargo_home) = var("CARGO_HOME").filter(|value| !value.is_empty()) {
        add_file(PathBuf::from(cargo_home).join("bin").join(&exe));
    }
    let home = var("HOME").or_else(|| var("USERPROFILE")).filter(|value| !value.is_empty());
    if let Some(home) = home {
        add_file(PathBuf::from(home).join(".cargo").join("bin").join(&exe));
    }
    out
}

/// True when `<binary> --version` exits successfully within `timeout`. The rustup proxy for a
/// component that is not installed prints an error and fails, so it is rejected here.
pub(crate) fn answers_version(binary: &Path, timeout: Duration) -> bool {
    let mut command = Command::new(binary);
    command.arg("--version");
    matches!(run_with_timeout(command, timeout, false), Some((status, _)) if status.success())
}

fn rustup_which(timeout: Duration) -> Option<PathBuf> {
    let mut command = Command::new("rustup");
    command.args(["which", "rust-analyzer"]);
    let (status, output) = run_with_timeout(command, timeout, true)?;
    if !status.success() {
        return None;
    }
    let text = String::from_utf8(output).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty()).then(|| PathBuf::from(line))
}

/// Runs `command` with no stdin, killing it after `timeout`. Returns its status and (when
/// `capture`) the start of its stdout; `None` if it could not start or timed out.
pub(crate) fn run_with_timeout(
    mut command: Command,
    timeout: Duration,
    capture: bool,
) -> Option<(ExitStatus, Vec<u8>)> {
    command.stdin(Stdio::null()).stderr(Stdio::null());
    command.stdout(if capture { Stdio::piped() } else { Stdio::null() });
    hide_console(&mut command);
    let mut child = command.spawn().ok()?;
    // Drain stdout on a thread so a chatty binary cannot stall on a full pipe.
    let output = match child.stdout.take() {
        Some(stdout) => {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let spawned = thread::Builder::new().name("lsp-probe-output".to_owned()).spawn(move || {
                let mut buf = Vec::new();
                let _ = stdout.take(MAX_CAPTURED_OUTPUT).read_to_end(&mut buf);
                let _ = tx.send(buf);
            });
            spawned.ok().map(|_| rx)
        }
        None => None,
    };
    let deadline = Deadline::after(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if !deadline.expired() => thread::sleep(PROBE_POLL),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    }?;
    // A grandchild could keep stdout open; don't wait for it forever.
    let output = output.and_then(|rx| rx.recv_timeout(Duration::from_secs(1)).ok()).unwrap_or_default();
    Some((status, output))
}

/// Don't flash a console window for helper processes on Windows.
pub(crate) fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("codecraft-lsp-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn exe() -> String {
        format!("rust-analyzer{}", std::env::consts::EXE_SUFFIX)
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn candidate_order_env_then_path_then_cargo_home() {
        let tmp = TempDir::new("candidates");
        let path_a = tmp.0.join("a");
        let path_b = tmp.0.join("b");
        let home = tmp.0.join("home");
        touch(&path_b.join(exe()));
        touch(&home.join(".cargo").join("bin").join(exe()));
        std::fs::create_dir_all(&path_a).unwrap();
        let mut env: HashMap<&str, OsString> = HashMap::new();
        env.insert("RUST_ANALYZER", OsString::from("/opt/ra/rust-analyzer"));
        env.insert("PATH", std::env::join_paths([&path_a, &path_b]).unwrap());
        env.insert("HOME", home.clone().into_os_string());
        let found = candidates(&|name| env.get(name).cloned());
        assert_eq!(
            found,
            vec![
                PathBuf::from("/opt/ra/rust-analyzer"),
                path_b.join(exe()),
                home.join(".cargo").join("bin").join(exe()),
            ]
        );
    }

    #[test]
    fn duplicates_and_missing_files_are_skipped() {
        let tmp = TempDir::new("dupes");
        let bin = tmp.0.join(".cargo").join("bin");
        touch(&bin.join(exe()));
        let mut env: HashMap<&str, OsString> = HashMap::new();
        env.insert("PATH", std::env::join_paths([&bin, &tmp.0.join("missing")]).unwrap());
        env.insert("CARGO_HOME", tmp.0.join(".cargo").into_os_string());
        env.insert("HOME", tmp.0.clone().into_os_string());
        env.insert("RUST_ANALYZER", OsString::new());
        assert_eq!(candidates(&|name| env.get(name).cloned()), vec![bin.join(exe())]);
    }

    #[test]
    fn empty_environment_has_no_candidates() {
        assert!(candidates(&|_| None).is_empty());
    }

    #[test]
    fn missing_binary_does_not_answer() {
        assert!(!answers_version(Path::new("/definitely/not/a/rust-analyzer"), Duration::from_secs(1)));
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_requires_success() {
        assert!(answers_version(Path::new("true"), PROBE_TIMEOUT));
        assert!(!answers_version(Path::new("false"), PROBE_TIMEOUT));
    }

    #[cfg(unix)]
    #[test]
    fn hanging_binary_is_killed_at_the_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        assert!(run_with_timeout(command, Duration::from_millis(200), true).is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    fn captures_stdout() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo /some/path/rust-analyzer"]);
        let (status, out) = run_with_timeout(command, PROBE_TIMEOUT, true).unwrap();
        assert!(status.success());
        assert_eq!(String::from_utf8(out).unwrap().trim(), "/some/path/rust-analyzer");
    }

    #[test]
    fn find_returns_only_a_working_binary() {
        // rust-analyzer may or may not be installed; whatever is found must answer --version.
        if let Some(found) = find_rust_analyzer() {
            assert!(answers_version(&found, PROBE_TIMEOUT));
        }
    }
}
