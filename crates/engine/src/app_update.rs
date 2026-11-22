//! Updating Code Craft from GitHub releases (`codecraft-update`): `update.check` looks for a newer
//! signed release, `update.install` downloads it, verifies it and stages it in the version store;
//! the existing restart flow (`selfupdate.restartInto`, health check, rollback) takes it from
//! there. All network and disk work runs on a worker.
use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, spec};
use crate::{Session, workspace::NotificationLevel};
use codecraft_selfupdate::Store;
use codecraft_update as upd;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

/// How often a launch checks for an update on its own.
const AUTO_CHECK_SECS: u64 = 24 * 60 * 60;
const LAST_CHECK_FILE: &str = "update-check.json";
/// Versions kept after an install (the store's garbage collection).
const KEEP_VERSIONS: usize = 5;

pub struct State {
    pub busy: bool,
    /// A newer release found by the last check (or install attempt).
    pub available: Option<upd::Manifest>,
    pub startup_checked: bool,
    /// Where to look and whom to trust; tests replace it.
    pub config: upd::Config,
    /// Plain `http://` (a local test server).
    pub allow_http: bool,
    rx: Option<Receiver<Reply>>,
}
impl Default for State {
    fn default() -> Self {
        Self { busy: false, available: None, startup_checked: false, config: upd::Config::release(), allow_http: false, rx: None }
    }
}

enum Reply {
    Checked { manual: bool, result: Result<upd::Check, String> },
    Installed(Result<Option<(String, String)>, String>),
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    [
        ("update.check", "Check for Updates", "{} — look for a newer signed release on GitHub; result in the status and notifications"),
        (
            "update.install",
            "Install Update",
            "{} — download the latest signed release, verify it and stage it; then Restart now (selfupdate.restartInto)",
        ),
    ]
    .into_iter()
    .map(|(id, label, params)| {
        let mut s = spec(id, label, None, params, Context::Global, always, dispatch);
        s.run = if id == "update.check" { |s, _| start_check(s, true) } else { |s, _| start_install(s) };
        s
    })
    .collect()
}
fn dispatch(_: &mut Session, _: &Value) -> CmdResult {
    Err(CommandError::Failed("missing update operation".into()))
}

fn failure(e: impl ToString) -> CommandError {
    CommandError::Failed(e.to_string())
}
fn base(s: &Session) -> Result<PathBuf, CommandError> {
    s.versions.base.clone().map(Ok).unwrap_or_else(codecraft_selfupdate::default_base).map_err(failure)
}
fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
fn http(allow: bool) -> upd::Http {
    if allow { upd::Http::new().allow_http() } else { upd::Http::new() }
}
/// Only a launcher-managed install has a store to stage into and a launcher to restart through.
fn managed(s: &Session) -> bool {
    std::env::var_os("CODECRAFT_LAUNCHER").is_some() || s.selfupdate.launcher.is_some() || s.selfupdate.isolated
}

fn start_check(s: &mut Session, manual: bool) -> CmdResult {
    if s.app_update.busy {
        return Err(failure("an update operation is already running"));
    }
    let (cfg, allow) = (s.app_update.config.clone(), s.app_update.allow_http);
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("update-check".into())
        .spawn(move || {
            let result = upd::check(&cfg, &http(allow), current()).map_err(|e| e.to_string());
            let _ = tx.send(Reply::Checked { manual, result });
        })
        .map_err(failure)?;
    s.app_update.rx = Some(rx);
    s.app_update.busy = true;
    if manual {
        s.status = "Checking for updates…".into();
    }
    Ok(json!({"queued": true, "current": current()}))
}

fn start_install(s: &mut Session) -> CmdResult {
    if s.app_update.busy {
        return Err(failure("an update operation is already running"));
    }
    if !managed(s) {
        return Err(failure(
            "this Code Craft was not started by its launcher, so there is nothing to update into; \
             install a release build (developer builds update from source)",
        ));
    }
    if s.debug.live_count() > 0 {
        return Err(failure("stop all live debug sessions before updating"));
    }
    let base = base(s)?;
    let factory = s.versions.factory.clone().or_else(|| std::env::var_os("CODECRAFT_FACTORY_DIR").map(PathBuf::from));
    let (cfg, allow) = (s.app_update.config.clone(), s.app_update.allow_http);
    let launcher_version = std::env::var("CODECRAFT_LAUNCHER_VERSION").ok();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("update-install".into())
        .spawn(move || {
            let result = install(&cfg, &http(allow), &base, factory.unwrap_or_default(), launcher_version.as_deref());
            let _ = tx.send(Reply::Installed(result));
        })
        .map_err(failure)?;
    s.app_update.rx = Some(rx);
    s.app_update.busy = true;
    s.status = "Downloading the update…".into();
    Ok(json!({"queued": true, "current": current()}))
}

/// Fetch, verify, unpack and stage the latest release. `None`: already up to date.
fn install(
    cfg: &upd::Config,
    fetch: &dyn upd::Fetch,
    base: &std::path::Path,
    factory: PathBuf,
    launcher_version: Option<&str>,
) -> Result<Option<(String, String)>, String> {
    let upd::Check::Available(manifest) = upd::check(cfg, fetch, current()).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    if !manifest.min_launcher.is_empty()
        && let (Some(need), Some(have)) =
            (upd::Version::parse(&manifest.min_launcher), launcher_version.and_then(upd::Version::parse))
        && have < need
    {
        return Err(format!(
            "{} needs launcher {} or newer (this one is {}); install the full release from GitHub",
            manifest.version, manifest.min_launcher, launcher_version.unwrap_or_default()
        ));
    }
    let work = base.join("update-tmp");
    let _ = std::fs::remove_dir_all(&work);
    let staged = (|| -> Result<String, String> {
        let file = upd::download(fetch, &manifest, &work.join("download")).map_err(|e| e.to_string())?;
        let payload = work.join("payload");
        upd::unpack(&file, &payload).map_err(|e| e.to_string())?;
        let store = Store::new(base.to_path_buf(), factory);
        let id = store.stage(&payload, &manifest.version, &manifest.commit, "release").map_err(|e| e.to_string())?;
        store.promote_pending(&id).map_err(|e| e.to_string())?;
        let _ = store.gc(KEEP_VERSIONS);
        Ok(id)
    })();
    let _ = std::fs::remove_dir_all(&work);
    staged.map(|id| Some((id, manifest.version)))
}

/// Called every frame: collect a finished worker, and start the daily check once per launch.
pub(crate) fn pump(s: &mut Session) {
    if let Some(rx) = &s.app_update.rx {
        match rx.try_recv() {
            Ok(reply) => {
                s.app_update.rx = None;
                s.app_update.busy = false;
                finish(s, reply);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                s.app_update.rx = None;
                s.app_update.busy = false;
                s.status = "update worker stopped".into();
            }
            Err(mpsc::TryRecvError::Empty) => (),
        }
    }
    if !s.app_update.startup_checked && !s.app_update.busy {
        s.app_update.startup_checked = true;
        if std::env::var_os("CODECRAFT_LAUNCHER").is_some()
            && std::env::var_os("CODECRAFT_NO_UPDATE_CHECK").is_none()
            && !s.selfupdate.isolated
            && due(s)
        {
            let _ = start_check(s, false);
        }
    }
}

/// True (and the time recorded) when the last automatic check is more than a day old.
fn due(s: &Session) -> bool {
    let Ok(base) = base(s) else { return false };
    let path = base.join(LAST_CHECK_FILE);
    let now = codecraft_selfupdate::now();
    let last = std::fs::read_to_string(&path).ok().and_then(|t| t.trim().parse::<u64>().ok()).unwrap_or(0);
    if now.saturating_sub(last) < AUTO_CHECK_SECS {
        return false;
    }
    let _ = codecraft_selfupdate::atomic_write(&path, now.to_string().as_bytes());
    true
}

fn finish(s: &mut Session, reply: Reply) {
    match reply {
        Reply::Checked { manual, result } => match result {
            Ok(upd::Check::Available(m)) => {
                s.notify(
                    NotificationLevel::Info,
                    "Update available",
                    format!("Code Craft {} is available (this is {}). Run Install Update.", m.version, current()),
                );
                s.app_update.available = Some(m);
            }
            Ok(upd::Check::UpToDate) => {
                s.app_update.available = None;
                if manual {
                    s.notify(NotificationLevel::Info, "Code Craft is up to date", format!("Version {}", current()));
                }
            }
            // A background check that cannot reach GitHub is not worth an alert.
            Err(e) if manual => s.notify(NotificationLevel::Error, "Update check failed", e),
            Err(_) => (),
        },
        Reply::Installed(Ok(Some((id, version)))) => {
            s.app_update.available = None;
            s.selfupdate.ready = Some(id.clone());
            s.notify(
                NotificationLevel::Info,
                "Update ready",
                format!("Code Craft {version} is installed as {id}. Restart now to use it; Roll Back is in Versions & Rollback."),
            );
        }
        Reply::Installed(Ok(None)) => {
            s.app_update.available = None;
            s.notify(NotificationLevel::Info, "Code Craft is up to date", format!("Version {}", current()));
        }
        Reply::Installed(Err(e)) => s.notify(NotificationLevel::Error, "Update failed", e),
    }
}
