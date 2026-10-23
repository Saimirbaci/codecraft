//! Stable, native, background-only boot supervisor. No GUI dependencies.
#![forbid(unsafe_code)]
#![cfg_attr(windows, windows_subsystem = "windows")]
use codecraft_selfupdate::{FACTORY, Store, choose, default_base, now};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Options {
    factory: bool,
    version: Option<String>,
    status: bool,
    rollback: bool,
    wait: bool,
    wait_pid: Option<u32>,
    args: Vec<OsString>,
}
fn parse(args: impl Iterator<Item = OsString>) -> io::Result<Options> {
    let mut o = Options::default();
    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--wait-lock") => o.wait = true,
            Some("--wait-pid") => {
                o.wait_pid = Some(
                    args.next()
                        .and_then(|v| v.to_str().and_then(|s| s.parse::<u32>().ok()))
                        .filter(|p| i32::try_from(*p).is_ok_and(|p| p > 0) && *p != std::process::id())
                        .ok_or_else(|| io::Error::other("--wait-pid requires another positive PID"))?,
                );
            }
            Some("--factory") => o.factory = true,
            Some("--status") => o.status = true,
            Some("--rollback") => o.rollback = true,
            Some("--version-id") => {
                o.version = Some(
                    args.next()
                        .and_then(|v| v.into_string().ok())
                        .filter(|v| codecraft_selfupdate::valid_id(v))
                        .ok_or_else(|| io::Error::other("--version-id requires a valid id"))?,
                );
            }
            _ => o.args.push(arg),
        }
    }
    Ok(o)
}
fn factory_path() -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODECRAFT_FACTORY_DIR") {
        return Ok(path.into());
    }
    let exe = std::env::current_exe()?;
    let parent = exe.parent().ok_or_else(|| io::Error::other("launcher path unavailable"))?;
    if parent.file_name().is_some_and(|n| n == "MacOS") {
        Ok(parent.join("../Resources/factory"))
    } else {
        Ok(parent.join("factory"))
    }
}
fn main() {
    if let Err(e) = run() {
        let _ = writeln!(io::stderr().lock(), "Code Craft launcher: {e}");
        std::process::exit(1);
    }
}
fn run() -> io::Result<()> {
    let opts = parse(std::env::args_os().skip(1))?;
    let store = Store::new(default_base()?, factory_path()?);
    if opts.status {
        // Print through a library serialization helper, keeping the executable std-only.
        writeln!(io::stdout().lock(), "{}", store.status_json()?)?;
        return Ok(());
    }
    if let Some(pid) = opts.wait_pid {
        let deadline = Instant::now() + Duration::from_secs(300);
        while process_alive(pid).inspect_err(|e| {
            let _ = store.log(&format!("handoff wait failed: {e}"));
        })? {
            if Instant::now() >= deadline {
                let _ = store.log("handoff cancelled: previous app has not exited");
                return Err(io::Error::other("previous app has not exited; restart cancelled"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let lock_start = Instant::now();
    let _launcher = loop {
        match store.launcher_lock() {
            Ok(lock) => break lock,
            Err(e)
                if opts.wait
                    && lock_start.elapsed() < Duration::from_secs(30)
                    && e.kind() == io::ErrorKind::WouldBlock =>
            {
                std::thread::sleep(Duration::from_millis(50))
            }
            Err(e) => return Err(e),
        }
    };
    if opts.rollback {
        store.rollback()?;
    }
    let forced = if opts.factory { Some(FACTORY.to_string()) } else { opts.version };
    let mut selected = forced.unwrap_or_else(|| choose(&store.boot_state(), now()).id);
    // A launcher killed before recording failure leaves pending_health behind.
    let state = if opts.factory { codecraft_selfupdate::State::default() } else { store.state() };
    if !opts.factory
        && state.pending_health
        && let Some(b) = &state.boot
    {
        store.failed(&b.id, "previous supervisor interrupted before health")?;
    }
    let timeout = Duration::from_secs(
        std::env::var("CODECRAFT_BOOT_TIMEOUT").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(45).clamp(1, 3600),
    );
    let mut tried = std::collections::BTreeSet::new();
    loop {
        if !tried.insert(selected.clone()) {
            return Err(io::Error::other("no working version; run launcher --factory"));
        }
        let automatic = if opts.factory {
            codecraft_selfupdate::Choice {
                id: FACTORY.into(),
                why: "forced factory (ignoring pointers and metadata)".into(),
            }
        } else {
            choose(&store.boot_state(), now())
        };
        store.log(&format!(
            "selected {selected}: {}",
            if automatic.id == selected { automatic.why.as_str() } else { "explicit selection" }
        ))?;
        // Probe exclusively before taking our shared lease. A payload whose supervisor died
        // may still be alive and holding its own lease; never launch a duplicate over it.
        let probe = store.running_lease(&selected)?;
        drop(probe);
        let _lease = store.app_lease(&selected)?;
        if selected != FACTORY
            && let Err(e) = store.snapshot_state(&selected)
        {
            store.failed(&selected, &format!("configuration snapshot failed: {e}"))?;
            selected = choose(&store.boot_state(), now()).id;
            continue;
        }
        std::fs::create_dir_all(store.base.join("health"))?;
        let health = store.base.join("health").join(format!("{selected}.ok"));
        match std::fs::remove_file(&health) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e),
        }
        if !opts.factory {
            store.begin_boot(&selected, now())?;
        }
        let reason = supervise(&store, &selected, &health, timeout, &opts.args, !opts.factory)?;
        if reason.is_none() {
            if !opts.factory {
                store.clean_exit(&selected)?;
            }
            return Ok(());
        }
        let reason = reason.unwrap_or_default();
        if !opts.factory {
            store.failed(&selected, &reason)?;
        }
        if selected == FACTORY {
            return Err(io::Error::other(format!("factory failed: {reason}; see launcher.log")));
        }
        selected = choose(&store.boot_state(), now()).id;
    }
}
fn supervise(
    store: &Store,
    id: &str,
    health: &std::path::Path,
    timeout: Duration,
    args: &[OsString],
    record_health: bool,
) -> io::Result<Option<String>> {
    let launcher = std::env::current_exe()?;
    let child = match Command::new(store.executable(id)?)
        .args(args)
        .env("CODECRAFT_HOME", &store.base)
        .env("CODECRAFT_VERSION_ID", id)
        .env("CODECRAFT_HEALTH_FILE", health)
        .env("CODECRAFT_LAUNCHER", "1")
        .env("CODECRAFT_LAUNCHER_PATH", launcher)
        .env("CODECRAFT_FACTORY_DIR", &store.factory)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Ok(Some(format!("could not start: {e}"))),
    };
    let mut child = OwnedChild(child);
    // Reader workers drain arbitrary byte streams into bounded rotated log records.
    let mut readers = Vec::new();
    for pipe in [
        child.0.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.0.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let store = store.clone();
        readers.push(std::thread::Builder::new().name("launcher-log".into()).spawn(move || drain(pipe, &store))?);
    }
    let start = Instant::now();
    let mut healthy = false;
    let result = loop {
        if let Some(status) = child.0.try_wait()? {
            break if healthy && status.success() {
                None
            } else {
                Some(format!("child exited {status}{}", if healthy { " after health" } else { " before health" }))
            };
        }
        if !healthy && health_ok(health) {
            if record_health {
                store.mark_good(id)?;
            }
            healthy = true;
        }
        if !healthy && start.elapsed() >= timeout {
            let _ = child.0.kill();
            let _ = child.0.wait();
            break Some("boot health timeout".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Descendants may inherit pipes; dropping handles avoids blocking rollback on their output.
    drop(readers);
    Ok(result)
}
fn drain(mut pipe: Box<dyn Read + Send>, store: &Store) {
    let mut buf = [0u8; 4096];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if let Some(bytes) = buf.get(..n) {
                    let _ = store.log(&String::from_utf8_lossy(bytes));
                }
            }
        }
    }
}

fn health_ok(path: &std::path::Path) -> bool {
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() <= 16) {
        return false;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    file.take(16).read_to_end(&mut bytes).is_ok() && bytes == b"healthy\n"
}

struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn process_alive(pid: u32) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let output = Command::new("/bin/kill").env("LC_ALL", "C").args(["-0", "--", &pid.to_string()]).output()?;
        if output.status.success() {
            return Ok(true);
        }
        let message = String::from_utf8_lossy(&output.stderr);
        if message.contains("No such process") {
            return Ok(false);
        }
        Err(io::Error::other(format!("cannot check previous process: {message}")))
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]).output()?;
        if !output.status.success() {
            return Err(io::Error::other("could not inspect previous app"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
    }
}
