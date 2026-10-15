//! Agent sessions, M13 track SR: two windows on one project share live jobs (a lock file with a
//! heartbeat, so a job running elsewhere is never shown as interrupted), a failed `--resume`
//! (the agent's own store cleared) is retried with the transcript, Clean Up removes interrupted
//! and old sessions with the review worktrees CodeCraft created (never others), and History has
//! search, filters and export. Fake agent CLIs only; every file lives under a temp home.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::agent::Backend;
use codecraft_engine::agent_sessions::{self, SavedSession};
use codecraft_engine::agents::{Activity, JobId, JobStatus};
use codecraft_engine::{Effect, Session};

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok_and(|o| o.status.success())
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(ok, "git {args:?}");
}

fn session(home: &Path, project: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s.execute("file.open", &json!({ "path": project })).unwrap();
    s
}

fn wait(s: &mut Session, what: &str, mut cond: impl FnMut(&mut Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !cond(s) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn status(s: &Session, id: u64) -> JobStatus {
    s.agents.job(JobId(id)).map(|j| j.status).unwrap()
}

fn start(s: &mut Session, params: Value) -> (u64, String) {
    let r = s.execute("agent.start", &params).unwrap();
    (r["job"].as_u64().unwrap(), r["session"].as_str().unwrap().to_string())
}

fn listing(s: &mut Session, params: Value) -> Value {
    wait(s, "the session listing", |s| {
        s.pump();
        !s.execute("agent.sessions.list", &Value::Null).unwrap()["loading"].as_bool().unwrap()
    });
    s.execute("agent.sessions.list", &params).unwrap()
}

fn rows(v: &Value) -> Vec<Value> {
    v["sessions"].as_array().cloned().unwrap_or_default()
}

/// A fake `claude`: "@@slow@@" never ends; `--resume` fails like a cleared session store unless
/// the prompt has "@@out@@" (then it prints something first and still fails).
fn fake_claude(dir: &Path, log: &Path) -> PathBuf {
    script(
        dir,
        "claude",
        &format!(
            r#"printf '%s\n' "$@" >> '{log}'
echo ---- >> '{log}'
case "$*" in *--resume*)
  case "$*" in *@@out@@*) echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Partial."}}]}}}}';; esac
  echo 'No conversation found with session ID: sess-A' >&2
  exit 1;;
esac
echo '{{"type":"system","subtype":"init","session_id":"sess-A"}}'
case "$*" in *@@slow@@*) exec sleep 30;; esac
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"All done."}}]}},"session_id":"sess-A"}}'
echo '{{"type":"result","subtype":"success","is_error":false,"duration_ms":7,"total_cost_usd":0.1,"result":"ok","session_id":"sess-A"}}'
"#,
            log = log.display()
        ),
    )
}

#[test]
fn a_job_running_in_another_window_is_listed_as_running_not_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let fake = fake_claude(tmp.path(), &tmp.path().join("args.log"));
    let mut a = session(&home, &proj);
    a.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    let (id, key) = start(&mut a, json!({ "backend": "claude", "mode": "auto", "prompt": "Long task @@slow@@" }));
    let file = a.agents.sessions.project_dir(&proj).unwrap().join(format!("{key}.json"));
    let lock = agent_sessions::lock_file(&file);
    wait(&mut a, "the session and its lock on disk", |a| {
        a.agents.sessions.flush(Duration::from_secs(5));
        file.is_file() && lock.is_file()
    });

    // Window B on the same project (same process, its own session).
    let mut b = session(&home, &proj);
    let list = listing(&mut b, Value::Null);
    let r = rows(&list);
    assert_eq!(r.len(), 1, "{list}");
    assert_eq!(r[0]["status"], "running", "{list}");
    assert_eq!(r[0]["runningElsewhere"], true);
    assert!(agent_sessions::running_elsewhere(&b, &key));
    let err = b.execute("agent.sessions.open", &json!({ "session": key })).unwrap_err().to_string();
    assert!(err.contains("another CodeCraft window"), "{err}");
    assert!(b.execute("agent.sessions.delete", &json!({ "session": key })).is_err());
    assert!(b.execute("agent.sessions.resume", &json!({ "session": key, "prompt": "x" })).is_err());
    let plan = b.execute("agent.sessions.cleanup", &json!({ "days": 0 })).unwrap();
    assert!(plan["sessions"].as_array().unwrap().is_empty(), "never cleaned up while it runs: {plan}");
    assert!(file.is_file());

    // The job ends in window A: B's listing follows (no restart).
    a.execute("agent.stop", &json!({ "job": id })).unwrap();
    wait(&mut a, "the job to stop", |a| status(a, id) == JobStatus::Stopped);
    wait(&mut a, "the lock to go", |a| {
        a.pump();
        a.agents.sessions.flush(Duration::from_secs(5));
        !lock.exists()
    });
    wait(&mut b, "window B to see it stop", |b| {
        b.agents.sessions.saved.iter().any(|x| x.id == key && x.status == JobStatus::Stopped && !x.elsewhere)
    });
    b.execute("agent.sessions.open", &json!({ "session": key })).unwrap();
    a.shutdown();
    b.shutdown();
}

#[test]
fn a_stale_lock_means_interrupted() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let dir = home.join("agent-sessions").join(agent_sessions::project_key(&proj));
    std::fs::create_dir_all(&dir).unwrap();
    let now = agent_sessions::now_ms();
    for (id, beat) in [("crashed-1", now - 120_000), ("future-1", now + 3_600_000), ("garbage-1", 0)] {
        let v = json!({ "id": id, "title": id, "backend": "claude", "mode": "auto", "status": "running",
                        "project": proj, "createdMs": 1, "updatedMs": 1 });
        std::fs::write(dir.join(format!("{id}.json")), v.to_string()).unwrap();
        let lock = if id == "garbage-1" { "{ nope".to_string() } else { json!({ "heartbeatMs": beat }).to_string() };
        let lock_path = agent_sessions::lock_file(&dir.join(format!("{id}.json")));
        std::fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        std::fs::write(lock_path, lock).unwrap();
    }
    let mut s = session(&home, &proj);
    let list = listing(&mut s, Value::Null);
    for r in rows(&list) {
        assert_eq!(r["status"], "interrupted", "{r}");
        assert_eq!(r["runningElsewhere"], false);
    }
    assert_eq!(rows(&list).len(), 3);
    s.shutdown();
}

#[test]
fn a_failed_resume_is_retried_with_the_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let log = tmp.path().join("args.log");
    let fake = fake_claude(tmp.path(), &log);
    let mut s = session(&home, &proj);
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    let (id, _) = start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": "Fix the parser" }));
    wait(&mut s, "the first turn", |s| status(s, id) == JobStatus::Finished);
    std::fs::write(&log, "").unwrap();

    // `claude --resume sess-A` fails (its store was cleared): the turn runs again, seeded.
    s.execute("agent.followUp", &json!({ "job": id, "prompt": "Now the tests" })).unwrap();
    wait(&mut s, "the retried turn", |s| status(s, id) == JobStatus::Finished);
    let args = std::fs::read_to_string(&log).unwrap();
    let runs: Vec<&str> = args.split("----\n").filter(|r| !r.trim().is_empty()).collect();
    assert_eq!(runs.len(), 2, "{args}");
    assert!(runs[0].contains("--resume\nsess-A"), "{args}");
    assert!(!runs[1].contains("--resume"), "{args}");
    assert!(runs[1].contains("<previous-session>") && runs[1].contains("User: Fix the parser"), "{args}");
    assert!(runs[1].contains("Assistant: All done.") && runs[1].contains("Now the tests"), "{args}");
    let job = s.agents.job(JobId(id)).unwrap();
    assert!(
        job.activity
            .iter()
            .any(|a| matches!(a, Activity::Info { text } if text.contains("could not resume its session"))),
        "the feed says so"
    );
    assert_eq!(job.session_id.as_deref(), Some("sess-A"), "the new session's id");
    assert!(job.error.is_none());

    // A resumed turn that failed after producing output is not run twice.
    std::fs::write(&log, "").unwrap();
    s.execute("agent.followUp", &json!({ "job": id, "prompt": "More @@out@@" })).unwrap();
    wait(&mut s, "the failed turn", |s| status(s, id) == JobStatus::Failed);
    let args = std::fs::read_to_string(&log).unwrap();
    assert_eq!(args.matches("----").count(), 1, "{args}");
    s.shutdown();
}

fn saved(id: &str, title: &str, backend: Backend, status: JobStatus, updated_ms: u64, project: &Path) -> SavedSession {
    SavedSession {
        version: agent_sessions::FORMAT_VERSION,
        id: id.into(),
        title: title.into(),
        backend,
        mode: "auto".into(),
        status,
        project: project.into(),
        cwd: None,
        prompt: format!("Please {title}"),
        attachments: Vec::new(),
        created_ms: updated_ms,
        updated_ms,
        cost_usd: Some(0.25),
        duration_ms: Some(1500),
        backend_session_id: None,
        worktree: None,
        worktree_path: None,
        changed_files: Vec::new(),
        review: Vec::new(),
        error: None,
        dropped: 0,
        activity: vec![
            Activity::Prompt { text: format!("Please {title}") },
            Activity::Text { text: "Here is the plan.".into() },
            Activity::Tool {
                id: "t".into(),
                tool: "Bash".into(),
                summary: "cargo test".into(),
                state: codecraft_engine::agents::ToolState::Ok,
                output: "ok ```fenced```".into(),
            },
            Activity::Log { line: "noise".into() },
        ],
    }
}

fn write_saved(dir: &Path, mut x: SavedSession) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(format!("{}.json", x.id)), agent_sessions::encode(&mut x).unwrap()).unwrap();
}

#[test]
fn history_search_filters_and_export() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let dir = home.join("agent-sessions").join(agent_sessions::project_key(&proj));
    let now = agent_sessions::now_ms();
    write_saved(&dir, saved("s-1", "Fix the parser", Backend::Claude, JobStatus::Finished, now - 3000, &proj));
    write_saved(&dir, saved("s-2", "Write docs", Backend::Codex, JobStatus::Failed, now - 2000, &proj));
    write_saved(&dir, saved("s-3", "Parser benchmarks", Backend::Codex, JobStatus::Interrupted, now - 1000, &proj));
    let mut s = session(&home, &proj);
    let ids = |v: &Value| rows(v).iter().map(|r| r["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(ids(&listing(&mut s, Value::Null)), ["s-3", "s-2", "s-1"]);
    assert_eq!(ids(&listing(&mut s, json!({ "query": "PARSER" }))), ["s-3", "s-1"]);
    assert_eq!(ids(&listing(&mut s, json!({ "query": "parser bench" }))), ["s-3"]);
    assert_eq!(ids(&listing(&mut s, json!({ "query": "please docs" }))), ["s-2"], "the prompt is searched too");
    assert_eq!(ids(&listing(&mut s, json!({ "backend": "codex" }))), ["s-3", "s-2"]);
    assert_eq!(ids(&listing(&mut s, json!({ "status": "failed" }))), ["s-2"]);
    assert_eq!(ids(&listing(&mut s, json!({ "status": "all", "backend": "", "query": " " }))).len(), 3);
    assert!(ids(&listing(&mut s, json!({ "status": "nope" }))).is_empty());
    let f = agent_sessions::Filter { query: "parser".into(), backend: Some("claude".into()), status: None };
    assert_eq!(agent_sessions::history_filtered(&s, &f).len(), 1);

    // One session as Markdown (returned).
    let md = s.execute("agent.sessions.export", &json!({ "session": "s-1", "format": "markdown" })).unwrap();
    let text = md["text"].as_str().unwrap();
    assert!(text.starts_with("# Fix the parser\n"), "{text}");
    assert!(text.contains("## User\n\nPlease Fix the parser"), "{text}");
    assert!(text.contains("## Assistant\n\nHere is the plan."), "{text}");
    assert!(text.contains("**Bash** cargo test (done)") && text.contains("````\nok ```fenced```\n````"), "{text}");
    assert!(!text.contains("noise"), "logs are left out");
    assert!(text.contains("**Cost:** $0.25"), "{text}");
    // The filtered History as JSON, to a file.
    let out = tmp.path().join("export/codex.json");
    let r = s.execute("agent.sessions.export", &json!({ "backend": "codex", "format": "json", "path": out })).unwrap();
    assert_eq!(r["sessions"], 2);
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    let exported: Vec<SavedSession> = serde_json::from_value(v["sessions"].clone()).unwrap();
    assert_eq!(exported.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["s-3", "s-2"]);
    assert_eq!(exported[0].activity.len(), 4, "the whole record");
    // Pick: the file dialog first, then its answer completes the export.
    let r =
        s.execute("agent.sessions.export", &json!({ "query": "parser", "format": "markdown", "pick": true })).unwrap();
    assert_eq!(r["picking"], true);
    let effect = s.effects.iter().find_map(|e| match e {
        Effect::PickFile { command, param, save, extensions, .. } => {
            Some((command.clone(), param.clone(), *save, extensions.clone()))
        }
        _ => None,
    });
    assert_eq!(effect, Some(("agent.sessions.export".into(), "path".into(), true, vec!["md".to_string()])));
    let md_out = tmp.path().join("export/parser.md");
    s.execute("agent.sessions.export", &json!({ "path": md_out })).unwrap();
    let text = std::fs::read_to_string(&md_out).unwrap();
    assert!(
        text.contains("# Parser benchmarks") && text.contains("# Fix the parser") && !text.contains("# Write docs")
    );
    // Bad params are errors.
    for bad in [
        json!({ "session": "../x" }),
        json!({ "session": "nope" }),
        json!({ "format": "pdf" }),
        json!({ "sessions": ["../../etc"] }),
        json!({ "query": "zzz-no-match" }),
    ] {
        assert!(s.execute("agent.sessions.export", &bad).is_err(), "{bad}");
    }
    s.shutdown();
}

#[test]
fn clean_up_removes_interrupted_sessions_with_their_own_worktrees_only() {
    if !have("git") {
        eprintln!("skipping: git not found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let fake = fake_claude(tmp.path(), &tmp.path().join("args.log"));
    let worktrees = tmp.path().join("worktrees");
    let settings = json!({ "binaries": { "claude": fake }, "worktreesDir": worktrees });
    let mut s = session(&home, &repo);
    s.execute("agent.settings", &settings).unwrap();
    // Two reviews interrupted by a quit: their worktrees are kept for a resume.
    let mut paths = Vec::new();
    for p in ["Refactor @@slow@@", "Rename @@slow@@"] {
        let (id, _) = start(&mut s, json!({ "backend": "claude", "mode": "review", "prompt": p }));
        wait(&mut s, "the review to run", |s| {
            s.agents.job(JobId(id)).is_some_and(|j| j.status == JobStatus::Running && j.worktree.is_some())
        });
        paths.push(s.agents.job(JobId(id)).unwrap().worktree.as_ref().unwrap().path.clone());
    }
    s.shutdown();
    drop(s);
    assert!(paths.iter().all(|p| p.is_dir()));
    // A session that names a worktree CodeCraft did not create.
    let foreign = tmp.path().join("my-own-checkout");
    std::fs::create_dir_all(&foreign).unwrap();
    let dir = home.join("agent-sessions").join(agent_sessions::project_key(&repo));
    let mut x = saved("foreign-1", "Foreign", Backend::Claude, JobStatus::Interrupted, 1, &repo);
    x.worktree = Some(codecraft_engine::agent::git::Worktree {
        repo: repo.clone(),
        path: foreign.clone(),
        branch: "feature".into(),
        baseline: String::new(),
        prefix: PathBuf::new(),
    });
    write_saved(&dir, x);

    let mut s = session(&home, &repo);
    s.execute("agent.settings", &settings).unwrap();
    // Pruning by count never orphans a worktree: sessions with one stay until Clean Up.
    s.agents.sessions.max_sessions = 0;
    let list = listing(&mut s, Value::Null);
    assert_eq!(rows(&list).len(), 3, "{list}");
    assert!(rows(&list).iter().all(|r| r["status"] == "interrupted"));
    // The plan (nothing removed yet). Fresh interrupted sessions wait a minute; `days: 0` takes all.
    let plan = s.execute("agent.sessions.cleanup", &json!({ "days": 0 })).unwrap();
    assert_eq!(plan["sessions"].as_array().unwrap().len(), 3, "{plan}");
    let mut planned: Vec<PathBuf> =
        plan["worktrees"].as_array().unwrap().iter().map(|p| PathBuf::from(p.as_str().unwrap())).collect();
    planned.sort();
    let mut expected = paths.clone();
    expected.sort();
    assert_eq!(planned, expected);
    assert_eq!(plan["keptWorktrees"], json!([foreign]));
    assert!(paths.iter().all(|p| p.is_dir()), "a plan removes nothing");

    // Only the first one's worktree, keeping the sessions.
    let r = s.execute("agent.sessions.cleanup", &json!({ "days": 0, "confirm": true, "worktreesOnly": true })).unwrap();
    assert_eq!(r["removed"], 0);
    assert_eq!(r["worktreesRemoved"], 2);
    wait(&mut s, "the worktrees to go", |s| !s.agents.any_active() && paths.iter().all(|p| !p.exists()));
    assert!(foreign.is_dir(), "never a worktree it did not create");
    assert_eq!(rows(&listing(&mut s, Value::Null)).len(), 3, "the sessions stay in History");

    // Then the sessions themselves.
    let r = s.execute("agent.sessions.cleanup", &json!({ "days": 0, "confirm": true })).unwrap();
    assert_eq!(r["removed"], 3, "{r}");
    assert!(s.agents.sessions.flush(Duration::from_secs(5)));
    let left = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    assert_eq!(left, 0);
    assert!(foreign.is_dir());
    // Branches of removed worktrees are gone too.
    let branches = Command::new("git").arg("-C").arg(&repo).args(["branch", "--list", "codecraft/*"]).output().unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());
    s.shutdown();
}

#[test]
fn ours_recognises_only_codecraft_worktrees() {
    let base = Path::new("/home/u/.codecraft/worktrees");
    assert!(agent_sessions::ours(&base.join("proj-1/7-1700"), "codecraft/agent-7-1700", base));
    assert!(!agent_sessions::ours(&base.join("proj-1/7-1700"), "main", base));
    assert!(!agent_sessions::ours(&base.join("7-1700"), "codecraft/agent-7-1700", base), "one level too shallow");
    assert!(!agent_sessions::ours(Path::new("/elsewhere/proj-1/7"), "codecraft/agent-7", base));
    assert!(!agent_sessions::ours(&base.join("proj-1/../../x"), "codecraft/agent-x", base));
    assert!(!agent_sessions::ours(base, "codecraft/agent-", base));
}
