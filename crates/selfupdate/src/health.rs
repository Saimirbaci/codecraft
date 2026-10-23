//! Main-window health handshake. Developer runs have no health environment and are no-ops.
use crate::{Result, atomic_write};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
static REPORTED: AtomicBool = AtomicBool::new(false);
static QUEUED: AtomicBool = AtomicBool::new(false);
pub fn is_reported() -> bool {
    REPORTED.load(Ordering::Acquire)
}
use std::time::{Duration, Instant};
static STARTED: OnceLock<Instant> = OnceLock::new();
pub fn report_started() {
    STARTED.get_or_init(Instant::now);
}
pub fn healthy_after() -> Duration {
    Duration::from_secs(
        std::env::var("CODECRAFT_HEALTHY_AFTER").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(20).min(3600),
    )
}
/// Call on UI progress, after the window is visible; a hung event loop cannot report healthy.
pub fn report_healthy() -> Result<bool> {
    if std::env::var_os("CODECRAFT_LAUNCHER").is_none() {
        return Ok(false);
    }
    let Some(start) = STARTED.get() else {
        return Ok(false);
    };
    if start.elapsed() < healthy_after() {
        return Ok(false);
    }
    let Some(file) = std::env::var_os("CODECRAFT_HEALTH_FILE") else {
        return Ok(false);
    };
    atomic_write(&PathBuf::from(file), b"healthy\n")?;
    REPORTED.store(true, Ordering::Release);
    Ok(true)
}

/// Called by painted frames; filesystem writes happen on a worker, once per process.
pub fn window_progress() {
    report_started();
    if STARTED.get().is_some_and(|s| s.elapsed() >= healthy_after()) && !QUEUED.swap(true, Ordering::Relaxed) {
        let spawned = std::thread::Builder::new().name("version-health".into()).spawn(|| {
            if !report_healthy().unwrap_or(false) {
                QUEUED.store(false, Ordering::Relaxed);
            }
        });
        if spawned.is_err() {
            QUEUED.store(false, Ordering::Relaxed);
        }
    }
}
