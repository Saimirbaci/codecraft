//! Cancellable payload build. Call on a worker. Output is streamed in bounded records.
use crate::{Result, invalid};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::Duration;
#[derive(Clone, Debug)]
pub struct Request {
    pub source: PathBuf,
    pub base: PathBuf,
    pub cargo: PathBuf,
    pub profile: String,
    pub commit: Option<String>,
}
pub fn run(request: &Request, cancel: Arc<AtomicBool>, mut log: impl FnMut(&str)) -> Result<PathBuf> {
    if request.profile.is_empty() || !request.profile.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(invalid("invalid build profile"));
    }
    std::fs::create_dir_all(&request.base)?;
    let _lock = crate::Lock::acquire(&request.base.join("build.lock"))?;
    // A unique directory prevents accidentally staging a stale successful payload.
    let payload =
        request.base.join(format!("payload-{}-{}", std::process::id(), crate::SERIAL.fetch_add(1, Ordering::Relaxed)));
    // Reserve atomically: a reused PID/counter must never accept a stale payload.
    std::fs::create_dir(&payload)?;
    let mut cmd = crate::source::scoped_command(&request.cargo, &request.source);
    cmd.current_dir(&request.source)
        .args(["run", "--locked", "-p", "xtask", "--", "bundle", "--profile", &request.profile, "--version-payload"])
        .arg(&payload)
        .env("CARGO_TARGET_DIR", request.base.join("build-target"))
        .env("CARGO_INCREMENTAL", "1")
        .env("RUSTUP_AUTO_INSTALL", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(parent) = request.cargo.parent().filter(|p| p.is_absolute()) {
        let paths = std::iter::once(parent.to_path_buf())
            .chain(std::env::var_os("PATH").map(|s| std::env::split_paths(&s).collect::<Vec<_>>()).unwrap_or_default());
        if let Ok(path) = std::env::join_paths(paths) {
            cmd.env("PATH", path);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if let Some(commit) = &request.commit {
        if commit.len() < 7 || commit.len() > 40 || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(invalid("invalid build commit"));
        }
        cmd.env("CODECRAFT_BUILD_COMMIT", commit);
    }
    let mut child = cmd.spawn()?;
    let (tx, rx) = mpsc::sync_channel::<String>(128);
    for pipe in [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        if let Err(e) = std::thread::Builder::new().name("selfupdate-log".into()).spawn(move || {
            let mut pipe = pipe;
            let mut buf = [0; 4096];
            let mut pending = Vec::new();
            loop {
                match pipe.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        if !pending.is_empty() {
                            let _ = tx.send(String::from_utf8_lossy(&pending).into());
                        }
                        break;
                    }
                    Ok(n) => {
                        for byte in buf.get(..n).unwrap_or_default() {
                            pending.push(*byte);
                            if *byte == b'\n' || pending.len() >= 65536 {
                                if tx.send(String::from_utf8_lossy(&pending).into()).is_err() {
                                    return;
                                }
                                pending.clear();
                            }
                        }
                    }
                }
            }
        }) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    drop(tx);
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            #[cfg(unix)]
            {
                let group = format!("-{}", child.id());
                let _ = Command::new("/bin/kill").args(["-TERM", "--", &group]).status();
                let _ = Command::new("/bin/kill").args(["-KILL", "--", &group]).status();
            }
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill").args(["/PID", &child.id().to_string(), "/T", "/F"]).status();
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(invalid("build cancelled; running version unchanged"));
        }
        for chunk in rx.try_iter().take(256) {
            log(&chunk);
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    // Bounded final drain: inherited pipes in detached grandchildren cannot hang completion.
    for _ in 0..128 {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(chunk) => log(&chunk),
            Err(_) => break,
        }
    }
    if !status.success() {
        return Err(invalid(format!("payload build failed ({status}); see build log")));
    }
    let executable = crate::Store::new(PathBuf::new(), payload.clone()).executable(crate::FACTORY)?;
    if !executable.is_file() {
        return Err(invalid("build succeeded without a payload executable"));
    }
    Ok(payload)
}
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn fake_success_failure_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let cargo = dir.path().join("fake cargo");
        let request = Request {
            source: dir.path().into(),
            base: dir.path().join("base"),
            cargo: cargo.clone(),
            profile: "selfupdate".into(),
            commit: None,
        };
        let script = |body: &str| {
            std::fs::write(&cargo, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        script(
            "for arg do payload=\"$arg\"; done\nmkdir -p \"$payload\"\nprintf 'fake payload' > \"$payload/codecraft\"\nprintf hello",
        );
        let mut log = String::new();
        assert!(run(&request, Arc::new(AtomicBool::new(false)), |s| log.push_str(s)).unwrap().is_dir());
        assert!(log.contains("hello"));
        script("exit 0");
        assert!(
            run(&request, Arc::new(AtomicBool::new(false)), |_| {})
                .unwrap_err()
                .to_string()
                .contains("without a payload executable")
        );
        script("echo compiler-error >&2; exit 1");
        assert!(run(&request, Arc::new(AtomicBool::new(false)), |_| {}).is_err());
        script("sleep 30");
        let flag = Arc::new(AtomicBool::new(false));
        let copy = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            copy.store(true, Ordering::Relaxed);
        });
        let start = std::time::Instant::now();
        assert!(run(&request, flag, |_| {}).unwrap_err().to_string().contains("cancelled"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
