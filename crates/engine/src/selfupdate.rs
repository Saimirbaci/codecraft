//! Source and build orchestration. Every expensive operation runs on a worker.
use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, spec};
use crate::{Effect, Session};
use codecraft_selfupdate::{
    Store,
    preflight::{self, Report, Tools},
    source::{DEFAULT_REPO_URL, ManagedSource},
};
use codecraft_workspace::{ConsoleKind, NotificationLevel, RunStatus, RunTabId, ToolWindowId};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub repo_url: String,
    pub source_location: Option<PathBuf>,
    pub keep_versions: usize,
    pub disk_bytes: u64,
    pub warm_disk_bytes: u64,
    pub build_profile: String,
    pub ask_before_restart: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            repo_url: DEFAULT_REPO_URL.into(),
            source_location: None,
            keep_versions: 5,
            disk_bytes: preflight::FIRST_BUILD_BYTES,
            warm_disk_bytes: preflight::WARM_BUILD_BYTES,
            build_profile: "selfupdate".into(),
            ask_before_restart: true,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct Confirmation {
    pub job: u64,
    pub source: PathBuf,
    pub files: Vec<PathBuf>,
    pub summary: String,
    pub session: Option<String>,
}
#[derive(Default)]
pub struct State {
    pub settings: Settings,
    pub loaded: bool,
    pub isolated: bool,
    pub busy: bool,
    pub phase: String,
    pub report: Option<Report>,
    pub ready: Option<String>,
    pub confirm: Option<Confirmation>,
    pub restart_confirm: Option<String>,
    pub log: String,
    pub error: Option<String>,
    pub tools: Tools,
    pub launcher: Option<PathBuf>,
    pub tab: Option<RunTabId>,
    pub source: Option<PathBuf>,
    pub rollback_notice: Option<String>,
    pub updated_notice: Option<String>,
    pub updated_seen: bool,
    pub developer_payload: Option<String>,
    pub revert_confirm: Option<String>,
    diagnostic_pending: String,
    pub pending_submit: Option<Value>,
    cancel: Arc<AtomicBool>,
    rx: Option<Receiver<Event>>,
    restart: bool,
}
impl Drop for State {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
enum Event {
    Log(String),
    Report(Report),
    Source(PathBuf),
    Saved(codecraft_workspace::DocId, u64),
    Done(Result<Option<String>, String>),
}
fn failure(e: impl ToString) -> CommandError {
    CommandError::Failed(e.to_string())
}
fn base(s: &Session) -> Result<PathBuf, CommandError> {
    s.versions.base.clone().map(Ok).unwrap_or_else(codecraft_selfupdate::default_base).map_err(failure)
}
fn url(settings: &Settings) -> String {
    std::env::var("CODECRAFT_REPO_URL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| settings.repo_url.clone())
}
pub(crate) fn load(s: &mut Session) -> Result<(), CommandError> {
    if !s.selfupdate.loaded {
        let path = base(s)?.join("selfupdate.json");
        use std::io::Read;
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() < 65536)
            && let Ok(file) = std::fs::File::open(&path)
        {
            let mut bytes = Vec::new();
            if file.take(65536).read_to_end(&mut bytes).is_ok()
                && let Ok(settings) = serde_json::from_slice(&bytes)
            {
                s.selfupdate.settings = settings;
            }
        }
        s.feedback.managed_source_dir = Some(base(s)?.join("source"));
        s.selfupdate.settings.ask_before_restart = true;
        if s.feedback.source_dir.is_none() {
            s.feedback.source_dir = s.selfupdate.settings.source_location.clone();
        }
        s.selfupdate.loaded = true;
    }
    Ok(())
}
pub fn is_codecraft(s: &Session, job: u64) -> bool {
    let Some(job) = s.agents.job(crate::agents::JobId(job)) else { return false };
    let feedback_target = s.feedback.history.iter().any(|entry| {
        entry.target == crate::feedback::Target::CodeCraft
            && (entry.session.as_ref().is_some_and(|key| job.session_key.as_ref() == Some(key))
                || (entry.session.is_none()
                    && entry.job == Some(job.id.0)
                    && s.feedback.last.as_ref().is_some_and(|last| last.id == entry.id && last.job == Some(job.id.0))))
    });
    feedback_target
        || s.feedback.codecraft_source().is_ok_and(|p| {
            p.canonicalize().unwrap_or_else(|_| crate::normalize_path(&p))
                == job.project.canonicalize().unwrap_or_else(|_| crate::normalize_path(&job.project))
        })
}
pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "selfupdate.openRustup",
            "Open rustup.rs",
            None,
            "{} — open installation instructions in your browser",
            Context::Global,
            always,
            |s, _| {
                s.effects.push(Effect::OpenUrl("https://rustup.rs".into()));
                Ok(Value::Null)
            },
        ),
        spec(
            "selfupdate.openPayload",
            "Open Built CodeCraft",
            None,
            "{id:string} — explicitly open a built payload from a developer launch",
            Context::Global,
            always,
            |s, p| {
                let id = p
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| codecraft_selfupdate::valid_id(id))
                    .ok_or_else(|| failure("valid id required"))?;
                let store = Store::new(base(s)?, PathBuf::new());
                let exe = store.executable(id).map_err(failure)?;
                let mut cmd = if cfg!(target_os = "macos") {
                    let mut c = std::process::Command::new("open");
                    c.arg(store.version_dir(id).map_err(failure)?.join("payload/CodeCraft.app"));
                    c
                } else {
                    std::process::Command::new(exe)
                };
                for key in
                    ["CODECRAFT_VERSION_ID", "CODECRAFT_HEALTH_FILE", "CODECRAFT_LAUNCHER", "CODECRAFT_LAUNCHER_PATH"]
                {
                    cmd.env_remove(key);
                }
                cmd.spawn().map_err(failure)?;
                s.selfupdate.developer_payload = None;
                Ok(Value::Null)
            },
        ),
        spec(
            "selfupdate.status",
            "Self-update Status",
            None,
            "{} → settings, phase, report, ready, error",
            Context::Global,
            always,
            |s, _| {
                load(s)?;
                Ok(
                    json!({"settings":s.selfupdate.settings,"phase":s.selfupdate.phase,"report":s.selfupdate.report,"ready":s.selfupdate.ready,"error":s.selfupdate.error}),
                )
            },
        ),
        spec(
            "selfupdate.settings",
            "Configure Self-update",
            None,
            "{repo_url?,source_location?,keep_versions?,disk_bytes?,warm_disk_bytes?,build_profile?}; ask_before_restart is always true",
            Context::Global,
            always,
            |s, p| {
                load(s)?;
                let mut value = serde_json::to_value(&s.selfupdate.settings).map_err(failure)?;
                if let (Some(dst), Some(src)) = (value.as_object_mut(), p.as_object()) {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                }
                let mut settings: Settings = serde_json::from_value(value).map_err(failure)?;
                codecraft_selfupdate::source::validate_url(&settings.repo_url).map_err(failure)?;
                if settings.build_profile.is_empty()
                    || !settings.build_profile.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err(failure("invalid build profile"));
                }
                settings.keep_versions = settings.keep_versions.clamp(2, 100);
                settings.ask_before_restart = true;
                codecraft_selfupdate::atomic_write(
                    &base(s)?.join("selfupdate.json"),
                    &serde_json::to_vec(&settings).map_err(failure)?,
                )
                .map_err(failure)?;
                s.feedback.source_dir = settings.source_location.clone();
                s.selfupdate.settings = settings;
                Ok(Value::Null)
            },
        ),
        spec(
            "selfupdate.preflight",
            "Check Build Requirements",
            None,
            "{source?:path,network?:bool} — checks tools, disk and optional upstream reachability on a worker",
            Context::Global,
            always,
            |s, p| start(s, p, "preflight", None),
        ),
        spec(
            "selfupdate.prepareSource",
            "Set Up CodeCraft Source",
            None,
            "{} — asynchronously clone at the running build commit on codecraft-user",
            Context::Global,
            always,
            |s, p| start(s, p, "prepare", None),
        ),
        spec(
            "selfupdate.updateSource",
            "Update CodeCraft from Upstream",
            None,
            "{} — clean managed source only; abort conflicts",
            Context::Global,
            always,
            |s, p| start(s, p, "update", None),
        ),
        spec(
            "selfupdate.build",
            "Build CodeCraft",
            None,
            "{source:path,rev?:hex} — build current revision (rev must match HEAD); stage and promote; never auto-restart",
            Context::Global,
            always,
            |s, p| start(s, p, "build", None),
        ),
        spec(
            "selfupdate.cancel",
            "Cancel CodeCraft Build",
            None,
            "{} — cancel worker build; preserve running version",
            Context::Global,
            always,
            |s, _| {
                s.selfupdate.cancel.store(true, Ordering::Relaxed);
                s.selfupdate.pending_submit = None;
                Ok(Value::Null)
            },
        ),
        spec(
            "selfupdate.restartInto",
            "Restart into Version",
            None,
            "{id:string,confirm?:bool} — always confirm before handoff; developer launches show payload path",
            Context::Global,
            always,
            restart,
        ),
        spec(
            "feedback.acceptAndBuild",
            "Accept, Build & Restart",
            None,
            "{job:int,confirm?:true,mode?:restart|build|cancel} — confirmation required even for Auto",
            Context::Global,
            always,
            accept,
        ),
        spec(
            "selfupdate.sendFailure",
            "Send This Failure to the Agent",
            None,
            "{job:int,confirm:true} — follow-up in the same saved agent session",
            Context::Global,
            always,
            |s, p| {
                if p.get("confirm").and_then(Value::as_bool) != Some(true) {
                    return Err(failure("confirm sending failure to the agent"));
                }
                let text = failure_followup(s.selfupdate.rollback_notice.as_deref().unwrap_or(""), &s.selfupdate.log);
                s.feedback.refresh_history();
                let session = p.get("session").and_then(Value::as_str);
                let job = p.get("job").and_then(Value::as_u64);
                let record = s
                    .feedback
                    .history
                    .iter()
                    .find(|entry| {
                        session.is_some_and(|key| entry.session.as_deref() == Some(key))
                            || (session.is_none() && job.is_some() && entry.job == job)
                    })
                    .map(|entry| entry.id.clone());
                if let Some(record) = record {
                    s.execute("feedback.continue", &json!({"id":record}))?;
                    s.execute("feedback.submit", &json!({"confirm":true,"text":text}))?;
                } else if let Some(session) = session {
                    s.execute("agent.sessions.resume", &json!({"session":session,"prompt":text}))?;
                } else {
                    let job = p.get("job").and_then(Value::as_u64).ok_or_else(|| failure("job or session required"))?;
                    crate::agents::follow_up(s, crate::agents::JobId(job), &text)?;
                }
                Ok(Value::Null)
            },
        ),
        spec(
            "selfupdate.revertChange",
            "Revert My Change",
            None,
            "{commit:hex,confirm:true} — git revert in managed source only",
            Context::Global,
            always,
            |s, p| {
                let commit = p
                    .get("commit")
                    .and_then(Value::as_str)
                    .filter(|c| c.len() == 40 && c.bytes().all(|b| b.is_ascii_hexdigit()))
                    .ok_or_else(|| failure("valid accepted commit required"))?;
                if p.get("confirm").and_then(Value::as_bool) != Some(true) {
                    s.selfupdate.revert_confirm = Some(commit.into());
                    return Ok(json!({"confirmationRequired":true}));
                }
                s.selfupdate.revert_confirm = None;
                start(s, p, "revert", None)
            },
        ),
    ]
}
fn accept(s: &mut Session, p: &Value) -> CmdResult {
    if p.get("mode").and_then(Value::as_str) == Some("cancel") {
        s.selfupdate.confirm = None;
        return Ok(Value::Null);
    }
    let job = p.get("job").and_then(Value::as_u64).ok_or_else(|| failure("job required"))?;
    if !is_codecraft(s, job) {
        return Err(failure("this job does not edit CodeCraft's source"));
    }
    if s.selfupdate.busy {
        return Err(failure("self-update is busy"));
    }
    let j = s.agents.job(crate::agents::JobId(job)).ok_or_else(|| failure("job missing"))?;
    let confirmation = Confirmation {
        job,
        source: j.project.clone(),
        files: j
            .review
            .iter()
            .filter(|f| f.decision != crate::agents::ReviewDecision::Rejected)
            .map(|f| PathBuf::from(&f.rel))
            .collect(),
        summary: s
            .feedback
            .history
            .iter()
            .find(|entry| entry.job == Some(job))
            .map(|entry| entry.text.as_str())
            .unwrap_or(&j.prompt)
            .chars()
            .take(4096)
            .collect(),
        session: j.session_key.clone(),
    };
    if p.get("confirm").and_then(Value::as_bool) != Some(true) {
        s.selfupdate.confirm = Some(confirmation);
        return Ok(json!({"confirmationRequired":true}));
    }
    // Only a confirmation tied to the current review can advance this chain.
    if s.selfupdate.confirm.as_ref().is_none_or(|c| c.job != job || c.files != confirmation.files) {
        return Err(failure("open and review the build confirmation first"));
    }
    s.execute("agent.review.acceptAll", &json!({"job":job}))?;
    s.selfupdate.confirm = None;
    start(
        s,
        &json!({"source":confirmation.source,"restart":p.get("mode").and_then(Value::as_str)!=Some("build")}),
        "build",
        Some(confirmation),
    )
}
fn start(s: &mut Session, p: &Value, op: &'static str, accepted: Option<Confirmation>) -> CmdResult {
    load(s)?;
    if s.selfupdate.busy {
        return Err(failure("self-update is busy"));
    }
    let base = base(s)?;
    let mut settings = s.selfupdate.settings.clone();
    if !s.selfupdate.isolated {
        settings.repo_url = url(&settings);
    }
    let mut tools = s.selfupdate.tools.clone();
    if !s.selfupdate.isolated {
        for (path, name) in [(&mut tools.git, "git"), (&mut tools.cargo, "cargo"), (&mut tools.rustc, "rustc")] {
            if path.as_path() == std::path::Path::new(name)
                && let Some(found) = codecraft_lsp::which(name)
            {
                *path = found;
            }
        }
    }
    let factory = s.versions.factory.clone().unwrap_or_default();
    let source = p
        .get("source")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| s.feedback.codecraft_source().ok())
        .unwrap_or_else(|| base.join("source"));
    let rev = p.get("rev").and_then(Value::as_str).map(str::to_string);
    if rev.as_ref().is_some_and(|r| r.len() < 7 || r.len() > 40 || !r.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(failure("invalid revision"));
    }
    let commit = p.get("commit").and_then(Value::as_str).unwrap_or("").to_string();
    let network = p.get("network").and_then(Value::as_bool).unwrap_or(false);
    let blocked = crate::hot_exit::blocked(&s.hot_exit);
    let mut saves = Vec::new();
    if let Some(c) = &accepted {
        for doc in s.ws.docs() {
            if doc.path.as_ref().is_some_and(|path| c.files.iter().any(|rel| c.source.join(rel) == *path))
                && doc.doc.is_modified()
            {
                if blocked.contains(&doc.id) {
                    return Err(failure("accepted file has an unresolved hot-exit conflict"));
                }
                if let Some(path) = &doc.path {
                    saves.push((
                        path.clone(),
                        crate::misc::encoding::disk_bytes(doc).map_err(failure)?,
                        doc.id,
                        doc.doc.revision(),
                    ));
                }
            }
        }
    }
    let (tx, rx) = mpsc::sync_channel(128);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancelled = cancel.clone();
    let restart = p.get("restart").and_then(Value::as_bool).unwrap_or(false) && accepted.is_some();
    std::thread::Builder::new()
        .name("selfupdate".into())
        .spawn(move || {
            let result = {
                let io = (|| -> std::io::Result<Option<String>> {
                    let mut managed = ManagedSource::new(base.clone(), settings.repo_url.clone())?;
                    managed.git = tools.git.clone();
                    if op == "prepare" {
                        let path = managed.prepare(crate::about::BuildInfo::current().commit.as_deref(), |s| {
                            let _ = tx.send(Event::Log(s.into()));
                        })?;
                        let _ = tx.send(Event::Source(path));
                        return Ok(None);
                    }
                    if op == "update" {
                        managed.update()?;
                        return Ok(None);
                    }
                    if op == "revert" {
                        managed.revert(&commit)?;
                        return Ok(None);
                    }
                    for (path, bytes, id, revision) in saves {
                        codecraft_fs::write_file_atomic(&path, &bytes).map_err(std::io::Error::other)?;
                        let _ = tx.send(Event::Saved(id, revision));
                    }
                    let managed_checkout =
                        op == "build" && source.canonicalize()? == managed.path().canonicalize().unwrap_or_default();
                    if managed_checkout && let Some(c) = &accepted {
                        managed.commit(&c.files, &c.summary)?;
                    }
                    use std::io::Read;
                    let mut manifest = String::new();
                    if let Ok(file) = std::fs::File::open(source.join("Cargo.toml")) {
                        let _ = file.take(4 * 1024 * 1024).read_to_string(&mut manifest);
                    }
                    let minimum = codecraft_selfupdate::source::workspace_package_value(&manifest, "rust-version")
                        .unwrap_or_else(|| env!("CARGO_PKG_RUST_VERSION").into());
                    let version = codecraft_selfupdate::source::workspace_package_value(&manifest, "version")
                        .unwrap_or_else(|| crate::about::VERSION.into());
                    let profile_dir = match settings.build_profile.as_str() {
                        "dev" | "test" => "debug",
                        "bench" => "release",
                        other => other,
                    };
                    let threshold = if base.join("build-target").join(profile_dir).is_dir() {
                        settings.warm_disk_bytes
                    } else {
                        settings.disk_bytes
                    };
                    let report = preflight::evaluate(
                        preflight::probe(&base.join("build-target"), &tools, network.then_some(managed.url.as_str())),
                        &minimum,
                        threshold,
                    );
                    let ready = report.ready();
                    let _ = tx.send(Event::Report(report));
                    if !ready {
                        return Err(std::io::Error::other("preflight failed; see checks and fix hints"));
                    }
                    if op == "preflight" {
                        return Ok(None);
                    }
                    let head = codecraft_selfupdate::source::git(&tools.git, &source, &["rev-parse", "HEAD"])?;
                    if rev.as_ref().is_some_and(|r| !head.starts_with(r)) {
                        return Err(std::io::Error::other(
                            "requested revision differs from checkout; no checkout was changed",
                        ));
                    }
                    let branch = codecraft_selfupdate::source::git(&tools.git, &source, &["branch", "--show-current"])?;
                    let dirty_hash = codecraft_selfupdate::source::dirty_fingerprint(&tools.git, &source)?;
                    let mut log = String::new();
                    let build = codecraft_selfupdate::build::run(
                        &codecraft_selfupdate::build::Request {
                            source: source.clone(),
                            base: base.clone(),
                            cargo: tools.cargo,
                            profile: settings.build_profile,
                            commit: Some(head.clone()),
                        },
                        cancelled.clone(),
                        |chunk| {
                            append_log(&mut log, chunk);
                            let _ = tx.send(Event::Log(chunk.into()));
                        },
                    );
                    codecraft_selfupdate::atomic_write(&base.join("selfupdate-build.log"), log.as_bytes())?;
                    let payload = build?;
                    if head != codecraft_selfupdate::source::git(&tools.git, &source, &["rev-parse", "HEAD"])?
                        || dirty_hash != codecraft_selfupdate::source::dirty_fingerprint(&tools.git, &source)?
                    {
                        return Err(std::io::Error::other("source changed during build; rebuild before staging"));
                    }
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(std::io::Error::other("build cancelled before staging"));
                    }
                    let store = Store::new(base.clone(), factory);
                    let id = store.stage(&payload, &version, &head, &branch)?;
                    store.provenance(
                        &id,
                        accepted.as_ref().map(|c| c.summary.as_str()).unwrap_or(""),
                        accepted.as_ref().map(|c| c.job),
                        dirty_hash,
                        accepted.as_ref().filter(|_| managed_checkout).map(|_| head.clone()),
                    )?;
                    codecraft_selfupdate::atomic_write(&store.version_dir(&id)?.join("build.log"), log.as_bytes())?;
                    store.feedback_session(&id, accepted.as_ref().and_then(|c| c.session.as_deref()))?;
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(std::io::Error::other("build cancelled while staging; running version unchanged"));
                    }
                    store.promote_pending(&id)?;
                    let _ = store.gc(settings.keep_versions);
                    let _ = std::fs::remove_dir_all(payload);
                    Ok(Some(id))
                })();
                io.map_err(|e| e.to_string())
            };
            let _ = tx.send(Event::Done(result));
        })
        .map_err(failure)?;
    s.selfupdate.cancel = cancel;
    s.selfupdate.rx = Some(rx);
    s.selfupdate.busy = true;
    s.selfupdate.restart = restart;
    s.selfupdate.phase = if op == "prepare" { "Setting up CodeCraft's source" } else { op }.into();
    s.status = s.selfupdate.phase.clone();
    s.selfupdate.error = None;
    s.selfupdate.source = Some(source_for_state(s, p));
    if op == "build" {
        s.selfupdate.log.clear();
        s.selfupdate.diagnostic_pending.clear();
        s.selfupdate.tab = Some(s.ws.add_run("CodeCraft self-update", "Build version payload", true));
        s.ws.layout.show(ToolWindowId::Build);
    }
    Ok(json!({"queued":true}))
}
fn source_for_state(s: &Session, p: &Value) -> PathBuf {
    p.get("source")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| s.feedback.codecraft_source().ok())
        .unwrap_or_default()
}
/// Crash content is separate from an attached build log, so large build output cannot hide it.
pub fn crash_log(notice: &str) -> &str {
    notice.split_once("\nBuild log:\n").map(|(crash, _)| crash).unwrap_or(notice)
}
fn failure_followup(notice: &str, current_build_log: &str) -> String {
    let (crash, build) = notice.split_once("\nBuild log:\n").unwrap_or((notice, current_build_log));
    format!(
        "CodeCraft self-update failed. Please investigate this follow-up in the same feedback thread.\nReason: {}\nCrash log:\n{}\nBuild log:\n{}",
        crash.lines().next().unwrap_or_default().chars().take(4096).collect::<String>(),
        log_tail(crash, 20000),
        log_tail(build, 40000)
    )
}
/// Bounded Unicode-safe tail for agent follow-ups and presentation.
pub fn log_tail(text: &str, characters: usize) -> &str {
    if characters == 0 {
        return "";
    }
    let start = text.char_indices().rev().nth(characters.saturating_sub(1)).map(|(at, _)| at).unwrap_or(0);
    text.get(start..).unwrap_or_default()
}
fn append_log(log: &mut String, chunk: &str) {
    log.push_str(chunk);
    if log.len() > 1024 * 1024 {
        let mut at = log.len().saturating_sub(1024 * 1024);
        while !log.is_char_boundary(at) {
            at = at.saturating_add(1);
        }
        log.drain(..at);
    }
}
fn restart_allowed(cancelled: bool, live_debug_sessions: usize, unsaved: bool) -> bool {
    !cancelled && live_debug_sessions == 0 && !unsaved
}
fn restart(s: &mut Session, p: &Value) -> CmdResult {
    let id = p
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| codecraft_selfupdate::valid_id(id))
        .ok_or_else(|| failure("valid version id required"))?
        .to_owned();
    if p.get("confirm").and_then(Value::as_bool) != Some(true) {
        s.selfupdate.restart_confirm = Some(id);
        return Ok(json!({"confirmationRequired":true}));
    }
    if s.debug.live_count() > 0 {
        s.selfupdate.ready = Some(id);
        return Err(failure("stop all live debug sessions before restarting"));
    }
    let store = Store::new(base(s)?, s.versions.factory.clone().unwrap_or_default());
    let executable = store.executable(&id).map_err(failure)?;
    if !executable.is_file() {
        return Err(failure("version payload missing"));
    }
    let launcher = s.selfupdate.launcher.clone().or_else(|| {
        if s.selfupdate.isolated || !codecraft_selfupdate::health::is_current_payload() {
            None
        } else {
            std::env::var_os("CODECRAFT_LAUNCHER_PATH").map(PathBuf::from)
        }
    });
    let Some(launcher) = launcher else {
        s.notify(
            NotificationLevel::Info,
            "Build ready",
            format!("Developer launch: built payload is {}. Open this executable to try it.", executable.display()),
        );
        s.selfupdate.developer_payload = Some(id.clone());
        s.selfupdate.ready = Some(id);
        s.selfupdate.restart_confirm = None;
        return Ok(json!({"payload":executable,"developerLaunch":true}));
    };
    if p.get("discard").and_then(Value::as_bool) == Some(true) {
        for doc in s.ws.docs_mut() {
            doc.doc.mark_saved();
        }
    } else {
        s.save_all()?;
    }
    crate::hot_exit::quit_save(s);
    let mut cmd = std::process::Command::new(launcher);
    cmd.args(["--wait-pid", &std::process::id().to_string(), "--version-id", &id, "--wait-lock"])
        .env("CODECRAFT_HOME", &store.base)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().map_err(failure)?;
    s.selfupdate.restart_confirm = None;
    s.effects.push(Effect::Quit);
    Ok(json!({"restarting":id}))
}
pub(crate) fn pump(s: &mut Session) {
    let events: Vec<_> = s.selfupdate.rx.as_ref().map(|rx| rx.try_iter().take(256).collect()).unwrap_or_default();
    if events.is_empty()
        && s.selfupdate.busy
        && s.selfupdate.rx.as_ref().is_some_and(|rx| matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)))
    {
        s.selfupdate.rx = None;
        s.selfupdate.busy = false;
        s.selfupdate.error = Some("self-update worker stopped".into());
    }
    for event in events {
        match event {
            Event::Log(chunk) => {
                append_log(&mut s.selfupdate.log, &chunk);
                s.selfupdate.diagnostic_pending.push_str(&chunk);
                let mut lines = Vec::new();
                while let Some(end) = s.selfupdate.diagnostic_pending.find('\n') {
                    let line: String = s.selfupdate.diagnostic_pending.drain(..=end).collect();
                    lines.push(line);
                }
                if s.selfupdate.diagnostic_pending.len() > 65536 {
                    s.selfupdate.diagnostic_pending.clear();
                }
                for line in &lines {
                    let parsed = codecraft_cargo::parse_message(line);
                    let display = match &parsed {
                        Some(codecraft_cargo::CargoMessage::CompilerMessage { diagnostic, .. }) => {
                            Some(diagnostic.rendered.as_deref().unwrap_or(&diagnostic.message))
                        }
                        Some(_) => None,
                        None => Some(line.as_str()),
                    };
                    if let Some(display) = display
                        && let Some(tab) = s.selfupdate.tab.and_then(|id| s.ws.run_mut(id))
                    {
                        tab.push(ConsoleKind::Stdout, crate::runs::strip_ansi(display));
                        let mut bytes = tab.lines.iter().map(|l| l.text.len()).sum::<usize>();
                        while tab.lines.len() > 1 && (tab.lines.len() > 256 || bytes > 1024 * 1024) {
                            if let Some(line) = tab.lines.first() {
                                bytes = bytes.saturating_sub(line.text.len());
                            }
                            tab.lines.remove(0);
                            tab.dropped_lines = tab.dropped_lines.saturating_add(1);
                        }
                    }
                    if let Some(codecraft_cargo::CargoMessage::CompilerMessage { diagnostic, .. }) = parsed
                        && let Some(problem) = crate::runs::problem_from_diagnostic(
                            s.selfupdate.source.as_deref().unwrap_or(std::path::Path::new("")),
                            &diagnostic,
                        )
                    {
                        s.ws.add_cargo_problem(problem);
                    }
                }
            }
            Event::Saved(id, revision) => {
                if let Some(doc) = s.ws.doc_mut(id)
                    && doc.doc.revision() == revision
                {
                    doc.doc.mark_saved();
                }
            }
            Event::Report(report) => s.selfupdate.report = Some(report),
            Event::Source(path) => {
                s.feedback.managed_source_dir = Some(path);
            }
            Event::Done(result) => {
                s.selfupdate.busy = false;
                s.selfupdate.rx = None;
                let success = result.is_ok();
                if let Some(tab) = s.selfupdate.tab.and_then(|id| s.ws.run_mut(id)) {
                    tab.status = if success { RunStatus::Succeeded } else { RunStatus::Failed(None) };
                }
                match result {
                    Ok(id) => {
                        if id.is_some() {
                            s.selfupdate.ready = id.clone();
                        }
                        s.selfupdate.phase = "Ready".into();
                        s.status = "CodeCraft self-update ready".into();
                        if let Some(id) = id {
                            s.notify(
                                NotificationLevel::Info,
                                "CodeCraft build ready",
                                format!("{id}: Restart now when ready"),
                            );
                            if s.selfupdate.restart
                                && restart_allowed(
                                    s.selfupdate.cancel.load(Ordering::Relaxed),
                                    s.debug.live_count(),
                                    s.ws.docs().any(|d| d.doc.is_modified()),
                                )
                            {
                                let _ = restart(s, &json!({"id":id,"confirm":true}));
                            }
                        } else if let Some(params) = s.selfupdate.pending_submit.take() {
                            let _ = s.execute("feedback.submit", &params);
                        }
                    }
                    Err(e) => {
                        s.selfupdate.pending_submit = None;
                        s.selfupdate.error = Some(e.clone());
                        s.selfupdate.phase = "Failed".into();
                        s.notify(NotificationLevel::Error, "CodeCraft self-update", e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sensitive_policy_and_hostile_params() {
        use crate::agent_access::{AgentAccess, Capability, Level, Verdict};
        assert_eq!(crate::agent_tools::command_capability("selfupdate.build"), Some(Capability::SelfUpdate));
        let mut access = AgentAccess::headless(&Capability::ALL);
        assert!(matches!(
            access.verdict("agent", Some(crate::approvals::AgentMode::Auto), Capability::SelfUpdate),
            Verdict::Deny(_)
        ));
        assert!(access.set_level(Capability::SelfUpdate, Level::Allow).is_err());
        access.allow_for_session("agent", Capability::SelfUpdate);
        assert!(matches!(access.verdict("agent", None, Capability::SelfUpdate), Verdict::Deny(_)));
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.isolate_home(dir.path());
        assert!(s.execute("selfupdate.restartInto", &json!({"id":"../escape","confirm":true})).is_err());
        assert!(s.execute("selfupdate.settings", &json!({"repo_url":"--upload-pack=bad"})).is_err());
        s.execute("selfupdate.settings", &json!({"ask_before_restart":false})).unwrap();
        assert!(s.selfupdate.settings.ask_before_restart);
        let notice =
            format!("Rolled back: test\nimportant crash trace\nBuild log:\n{}", "build output\n".repeat(20000));
        let follow = failure_followup(&notice, "");
        assert!(follow.contains("important crash trace"));
        assert!(follow.contains("build output"));
        assert!(follow.chars().count() < 100000);
        assert!(restart_allowed(false, 0, false));
        assert!(!restart_allowed(false, 2, false), "parked debug sessions also prevent restart");
        assert!(!restart_allowed(false, 0, true));
        assert!(!restart_allowed(true, 0, false));
        s.selfupdate.pending_submit = Some(json!({"confirm":true}));
        s.execute("feedback.cancel", &Value::Null).unwrap();
        assert!(s.selfupdate.pending_submit.is_none(), "cancel revokes deferred agent submission");
        let mut log = String::new();
        append_log(&mut log, &"🚀".repeat(400000));
        assert!(log.len() <= 1024 * 1024);
    }
    #[cfg(unix)]
    #[test]
    fn late_cancel_revokes_restart_and_queued_submission() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.isolate_home(dir.path());
        let payload = dir.path().join("payload");
        std::fs::create_dir(&payload).unwrap();
        std::fs::write(payload.join("codecraft"), "fake payload").unwrap();
        let store = Store::new(dir.path().into(), PathBuf::new());
        let id = store.stage(&payload, "0.3.0", "abcd", "test").unwrap();
        let launcher = dir.path().join("fake launcher");
        std::fs::write(&launcher, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        s.selfupdate.launcher = Some(launcher);
        s.selfupdate.pending_submit = Some(json!({"confirm":true}));
        s.execute("selfupdate.cancel", &Value::Null).unwrap();
        assert!(s.selfupdate.pending_submit.is_none());
        let (tx, rx) = mpsc::sync_channel(1);
        s.selfupdate.rx = Some(rx);
        s.selfupdate.busy = true;
        s.selfupdate.restart = true;
        tx.send(Event::Done(Ok(Some(id.clone())))).unwrap();
        pump(&mut s);
        assert_eq!(s.selfupdate.ready.as_deref(), Some(id.as_str()));
        assert!(
            !s.effects.iter().any(|e| matches!(e, Effect::Quit)),
            "cancel must revoke automatic restart even after staging completes"
        );
        s.selfupdate.launcher = None;
        let result = s.execute("selfupdate.restartInto", &json!({"id":id,"confirm":true})).unwrap();
        assert_eq!(result["developerLaunch"], true);
        assert!(!s.effects.iter().any(|e| matches!(e, Effect::Quit)));
    }
}
