//! Fully local Accept → build → stage/promote → handoff, with fake agent/cargo/launcher.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic)]
use codecraft_engine::agents::{JobId, JobStatus};
use codecraft_engine::{Effect, Session};
use codecraft_selfupdate::{
    Store,
    source::{ManagedSource, git},
};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
fn script(dir: &Path, name: &str, text: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{text}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}
fn wait(s: &mut Session, done: impl Fn(&Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(s) {
        assert!(Instant::now() < deadline, "timeout: {} {:?}", s.status, s.selfupdate.error);
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn accept_build_promote_restart_and_failure_followup() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = dir.path().join("upstream");
    std::fs::create_dir(&upstream).unwrap();
    let run = |args: &[&str]| git(Path::new("git"), &upstream, args).unwrap();
    run(&["init", "-b", "main"]);
    run(&["config", "user.name", "Test"]);
    run(&["config", "user.email", "test@example.invalid"]);
    std::fs::write(upstream.join("file.rs"), "fn before() {}\n").unwrap();
    run(&["add", "--", "file.rs"]);
    run(&["commit", "-m", "initial"]);
    let bare = dir.path().join("remote.git");
    git(Path::new("git"), dir.path(), &["clone", "--bare", "--", upstream.to_str().unwrap(), bare.to_str().unwrap()])
        .unwrap();
    let home = dir.path().join("home");
    let source = ManagedSource::new(home.clone(), format!("file://{}", bare.display())).unwrap();
    source.prepare(None, |_| {}).unwrap();
    let fake = script(
        dir.path(),
        "fake codex",
        r#"echo '{"type":"thread.started","thread_id":"feedback-thread"}'
printf 'fn after() {}\n' > file.rs
echo '{"type":"item.completed","item":{"id":"i","type":"agent_message","text":"Done"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":1}}'"#,
    );
    let cargo = script(
        dir.path(),
        "fake cargo",
        r#"if [ "$1" = --version ]; then echo 'cargo 1.95.0'; exit; fi
for arg do payload="$arg"; done
mkdir -p "$payload"
printf '#!/bin/sh\nexit 0\n' > "$payload/codecraft"
chmod +x "$payload/codecraft"
echo '{"reason":"compiler-artifact","package_id":"p","target":{"name":"hidden artifact"},"fresh":true}'
echo '{"reason":"compiler-message","package_id":"p","message":{"message":"test error","code":null,"level":"error","spans":[{"file_name":"file.rs","line_start":1,"line_end":1,"column_start":1,"column_end":2,"is_primary":true,"label":null,"text":[]}],"children":[],"rendered":"test error"}}'
sleep 0.2
"#,
    );
    let rustc = script(dir.path(), "fake rustc", "echo 'rustc 1.95.0'");
    let launch_log = dir.path().join("launcher args");
    let launcher = script(dir.path(), "fake launcher", &format!("printf '%s\\n' \"$@\" > '{}'", launch_log.display()));
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&home);
    s.feedback.source_dir = Some(source.path());
    s.selfupdate.settings.disk_bytes = 0;
    s.selfupdate.settings.warm_disk_bytes = 0;
    s.selfupdate.loaded = true;
    s.selfupdate.tools.cargo = cargo;
    s.selfupdate.tools.rustc = rustc;
    s.selfupdate.launcher = Some(launcher);
    s.execute("agent.settings", &json!({"binaries":{"codex":fake},"worktreesDir":dir.path().join("worktrees")}))
        .unwrap();
    s.execute("file.open", &json!({"path":source.path().join("file.rs")})).unwrap();
    let job = s
        .execute(
            "agent.start",
            &json!({"backend":"codex","mode":"review","prompt":"Feedback: change before to after","cwd":source.path()}),
        )
        .unwrap()["job"]
        .as_u64()
        .unwrap();
    wait(&mut s, |s| {
        s.agents.job(JobId(job)).is_some_and(|j| j.status == JobStatus::Reviewing && !j.review.is_empty())
    });
    s.execute("feedback.acceptAndBuild", &json!({"job":job})).unwrap();
    assert!(s.selfupdate.confirm.is_some());
    assert!(!s.selfupdate.busy);
    assert_eq!(std::fs::read_to_string(source.path().join("file.rs")).unwrap(), "fn before() {}\n");
    s.execute("feedback.acceptAndBuild", &json!({"job":job,"confirm":true,"mode":"build"})).unwrap();
    wait(&mut s, |s| s.selfupdate.log.contains("compiler-message"));
    let tab = s.ws.run(s.selfupdate.tab.unwrap()).unwrap();
    assert!(tab.lines.iter().any(|l| l.text.contains("test error")));
    assert!(!tab.lines.iter().any(|l| l.text.contains("compiler-artifact")));
    // Closing the tab cannot cancel or lose the job/log.
    if let Some(id) = s.selfupdate.tab {
        s.ws.close_run(id);
    }
    wait(&mut s, |s| !s.selfupdate.busy);
    assert!(s.selfupdate.error.is_none(), "{:?}", s.selfupdate.error);
    let id = s.selfupdate.ready.clone().unwrap();
    let store = Store::new(home.clone(), PathBuf::new());
    assert_eq!(store.state().current.as_deref(), Some(id.as_str()));
    let meta = store.meta(&id).unwrap();
    assert_eq!(meta.feedback_job, Some(job));
    assert!(meta.user_commit.is_some());
    assert_eq!(std::fs::read_to_string(source.path().join("file.rs")).unwrap(), "fn after() {}\n");
    assert!(source.read(&["status", "--porcelain"]).unwrap().is_empty());
    assert!(s.selfupdate.log.contains("compiler-message"));
    assert!(s.ws.problems_for(&source.path().join("file.rs")).iter().any(|p| p.message == "test error"));
    s.execute("selfupdate.restartInto", &json!({"id":id})).unwrap();
    assert!(s.selfupdate.restart_confirm.is_some());
    s.execute("selfupdate.restartInto", &json!({"id":id,"confirm":true})).unwrap();
    assert!(s.effects.iter().any(|e| matches!(e, Effect::Quit)));
    let deadline = Instant::now() + Duration::from_secs(3);
    while !launch_log.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        std::fs::read_to_string(launch_log).unwrap(),
        format!("--wait-pid\n{}\n--version-id\n{id}\n--wait-lock\n", std::process::id())
    );
    store.failed(&id, "deliberate test crash").unwrap();
    s.selfupdate.rollback_notice = store.take_notice().unwrap();
    assert!(s.selfupdate.rollback_notice.as_deref().unwrap().contains("deliberate test crash"));
    s.execute("selfupdate.sendFailure", &json!({"job":job,"confirm":true})).unwrap();
    assert!(s.agents.job(JobId(job)).unwrap().prompt.contains("change before"));
    // The feedback thread continues as a new job that carries the crash and the build log.
    let follow = s.agents.jobs.iter().find(|j| j.id != JobId(job)).map(|j| j.id);
    let follow = follow.expect("the failure follow-up starts a job in the same feedback thread");
    assert!(s.agents.job(follow).unwrap().activity.iter().any(|a| matches!(a,
        codecraft_engine::agents::Activity::Prompt{text} if text.contains("deliberate test crash") && text.contains("compiler-message"))));
    s.shutdown();
}

#[test]
fn failed_and_cancelled_builds_keep_old_version_and_logs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("developer source");
    std::fs::create_dir(&source).unwrap();
    git(Path::new("git"), &source, &["init", "-b", "main"]).unwrap();
    std::fs::write(source.join("file.rs"), "fn before() {}\n").unwrap();
    git(Path::new("git"), &source, &["add", "--", "file.rs"]).unwrap();
    git(
        Path::new("git"),
        &source,
        &["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-m", "initial"],
    )
    .unwrap();
    let mut s = Session::new();
    s.isolate_home(&dir.path().join("home"));
    s.lsp_enabled = false;
    s.selfupdate.loaded = true;
    s.selfupdate.settings.disk_bytes = 0;
    s.selfupdate.settings.warm_disk_bytes = 0;
    let fail = script(
        dir.path(),
        "failing cargo",
        "if [ \"$1\" = --version ]; then echo 'cargo 1.95.0'; exit; fi\necho 'compiler failed' >&2\nexit 3",
    );
    s.selfupdate.tools.cargo = fail;
    s.selfupdate.tools.rustc = script(dir.path(), "rustc", "echo 'rustc 1.95.0'");
    s.execute("selfupdate.build", &json!({"source":source})).unwrap();
    wait(&mut s, |s| !s.selfupdate.busy);
    assert!(s.selfupdate.error.as_deref().unwrap().contains("payload build failed"));
    assert!(s.selfupdate.log.contains("compiler failed"));
    let home = dir.path().join("home");
    let store = Store::new(home.clone(), PathBuf::new());
    assert!(store.state().current.is_none());
    assert!(store.list().is_empty());
    assert!(std::fs::read_to_string(home.join("selfupdate-build.log")).unwrap().contains("compiler failed"));
    s.selfupdate.tools.cargo = script(
        dir.path(),
        "slow cargo",
        "if [ \"$1\" = --version ]; then echo 'cargo 1.95.0'; exit; fi\necho starting\nexec sleep 30",
    );
    s.execute("selfupdate.build", &json!({"source":source})).unwrap();
    wait(&mut s, |s| s.selfupdate.log.contains("starting"));
    s.execute("selfupdate.cancel", &json!(null)).unwrap();
    wait(&mut s, |s| !s.selfupdate.busy);
    assert!(s.selfupdate.error.as_deref().unwrap().contains("cancelled"));
    assert!(store.state().current.is_none());
    assert_eq!(std::fs::read_to_string(source.join("file.rs")).unwrap(), "fn before() {}\n");
    s.shutdown();
}
