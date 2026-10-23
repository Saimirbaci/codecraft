//! Version management commands. All filesystem/process work runs on a worker.
use crate::commands::{CommandError, CommandSpec, Context, always, spec};
use crate::{Effect, Session};
use codecraft_selfupdate::{Meta, Store};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

#[derive(Default)]
pub struct Versions {
    pub open: bool,
    pub startup_checked: bool,
    pub entries: Vec<Meta>,
    pub status: Value,
    pub busy: bool,
    pub base: Option<PathBuf>,
    pub factory: Option<PathBuf>,
    rx: Option<Receiver<Reply>>,
}
struct Reply {
    result: Result<(Vec<Meta>, Value, Option<String>), String>,
    quit: bool,
}
pub(crate) fn specs() -> Vec<CommandSpec> {
    [
        ("version.list", "Versions & Rollback…", "{} — refresh installed versions and open the dialog"),
        (
            "version.status",
            "Version Status",
            "{} — refresh version status on a worker; result appears in versions.status",
        ),
        ("version.rollback", "Roll Back", "{} — restore backup and select last good version for next launch"),
        ("version.use", "Use This Version", "{id: string} — re-enable and select an installed version for next launch"),
        ("version.markGood", "Mark Version Good", "{id: string} — mark an installed version healthy"),
        ("version.launchFactory", "Launch Factory Version", "{} — launch the stable factory version and quit"),
    ]
    .into_iter()
    .map(|(id, label, params)| {
        let mut s = spec(id, label, None, params, Context::Global, always, dispatch);
        // Distinct function pointers carry the requested operation without hidden UI state.
        s.run = match id {
            "version.list" => |s, p| start(s, p, "list"),
            "version.status" => |s, p| start(s, p, "status"),
            "version.rollback" => |s, p| start(s, p, "rollback"),
            "version.use" => |s, p| start(s, p, "use"),
            "version.markGood" => |s, p| start(s, p, "good"),
            _ => |s, p| start(s, p, "factory"),
        };
        s
    })
    .collect()
}
fn dispatch(_: &mut Session, _: &Value) -> crate::commands::CmdResult {
    Err(CommandError::Failed("missing version operation".into()))
}
fn start(s: &mut Session, p: &Value, op: &'static str) -> crate::commands::CmdResult {
    if s.versions.busy {
        return Err(CommandError::Failed("version operation is busy".into()));
    }
    let id = if matches!(op, "use" | "good") {
        Some(
            p.get("id")
                .and_then(Value::as_str)
                .filter(|id| codecraft_selfupdate::valid_id(id))
                .ok_or_else(|| CommandError::BadParams("id must be a valid version id".into()))?
                .to_owned(),
        )
    } else {
        None
    };
    if op == "list" {
        s.versions.open = true;
    }
    let base = s.versions.base.clone();
    let factory = s.versions.factory.clone();
    let (tx, rx) = mpsc::channel();
    s.versions.rx = Some(rx);
    s.versions.busy = true;
    if let Err(e) = std::thread::Builder::new().name("version-store".into()).spawn(move || {
        let result = (|| -> Result<_, String> {
            let base = base.map(Ok).unwrap_or_else(codecraft_selfupdate::default_base).map_err(|e| e.to_string())?;
            let store = Store::new(
                base,
                factory.or_else(|| std::env::var_os("CODECRAFT_FACTORY_DIR").map(PathBuf::from)).unwrap_or_default(),
            );
            let io = match op {
                "rollback" => store.rollback().map(|_| ()),
                "use" => store.use_version(id.as_deref().unwrap_or("")),
                "good" => store.mark_good(id.as_deref().unwrap_or("")),
                "factory" => {
                    let launcher = std::env::var_os("CODECRAFT_LAUNCHER_PATH")
                        .ok_or("this developer launch has no stable launcher")?;
                    std::process::Command::new(launcher).args(["--factory", "--wait-lock"]).spawn().map(|_| ())
                }
                _ => Ok(()),
            };
            io.map_err(|e| e.to_string())?;
            let status =
                serde_json::from_str(&store.status_json().map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
            let mut notice = if codecraft_selfupdate::health::is_reported() {
                store.take_notice().map_err(|e| e.to_string())?
            } else {
                None
            };
            if let Some(text) = &mut notice {
                use std::io::Read;
                let log_path = text
                    .split_whitespace()
                    .nth(2)
                    .map(|id| id.trim_end_matches(':'))
                    .and_then(|id| store.version_dir(id).ok())
                    .map(|dir| dir.join("build.log"))
                    .filter(|p| p.is_file())
                    .unwrap_or_else(|| store.base.join("selfupdate-build.log"));
                if let Ok(file) = std::fs::File::open(log_path) {
                    let mut log = String::new();
                    let _ = file.take(1024 * 1024).read_to_string(&mut log);
                    text.push_str("\nBuild log:\n");
                    text.push_str(&log);
                }
            }
            Ok((store.list(), status, notice))
        })();
        let _ = tx.send(Reply { result, quit: op == "factory" });
    }) {
        s.versions.rx = None;
        s.versions.busy = false;
        return Err(CommandError::Failed(format!("could not start version worker: {e}")));
    }
    Ok(json!({"queued": true, "status": s.versions.status}))
}
pub(crate) fn pump(s: &mut Session) {
    let Some(rx) = &s.versions.rx else {
        return;
    };
    match rx.try_recv() {
        Ok(reply) => {
            s.versions.rx = None;
            s.versions.busy = false;
            match reply.result {
                Ok((entries, status, notice)) => {
                    if !s.selfupdate.updated_seen
                        && let Ok(id) = std::env::var("CODECRAFT_VERSION_ID")
                        && let Some(meta) = entries.iter().find(|m| m.id == id && !m.feedback_summary.is_empty())
                    {
                        s.selfupdate.updated_seen = true;
                        s.selfupdate.updated_notice =
                            Some(format!("Updated to {} from your feedback: {}", meta.id, meta.feedback_summary));
                        s.notify(
                            crate::workspace::NotificationLevel::Info,
                            "CodeCraft updated",
                            format!(
                                "Updated to {} from your feedback: {}. Roll Back is available in Versions & Rollback.",
                                meta.id, meta.feedback_summary
                            ),
                        );
                    }
                    s.versions.entries = entries;
                    s.versions.status = status;
                    if let Some(notice) = notice {
                        s.selfupdate.rollback_notice = Some(notice.clone());
                        let display = format!(
                            "{}\n{}",
                            notice.lines().next().unwrap_or_default(),
                            crate::selfupdate::log_tail(crate::selfupdate::crash_log(&notice), 8192)
                        );
                        s.notify(crate::workspace::NotificationLevel::Warning, "Version rollback", display);
                    }
                    if reply.quit {
                        s.effects.push(Effect::Quit);
                    }
                }
                Err(e) => s.notify(crate::workspace::NotificationLevel::Error, "Versions", e),
            }
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            s.versions.rx = None;
            s.versions.busy = false;
            s.status = "version worker stopped".into();
        }
        Err(mpsc::TryRecvError::Empty) => (),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_are_isolated_and_validate_ids() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.isolate_home(dir.path());
        assert!(s.execute("version.use", &json!({"id":"../escape"})).is_err());
        s.execute("version.list", &Value::Null).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while s.versions.busy && std::time::Instant::now() < deadline {
            pump(&mut s);
            std::thread::yield_now();
        }
        assert!(!s.versions.busy);
        assert!(s.versions.open);
        assert!(s.versions.entries.is_empty());
        assert_eq!(s.versions.status["choice"]["id"], "factory");
    }
}
