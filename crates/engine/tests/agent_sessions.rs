//! Saved agent sessions (track AG): jobs are saved per project while they run and after they
//! end, listed again by a fresh session (running ones as interrupted), reopened with their feed,
//! resumed (`--resume`, ACP `session/load`, transcript fallback), managed and pruned. Fake agent
//! CLIs only; every file lives under a temp `CODECRAFT_HOME` ([`Session::isolate_home`]).
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent_sessions::{self, SavedSession};
use codecraft_engine::agents::{Activity, JobId, JobStatus, ReviewDecision};

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

/// A session with its home under `home`, the project open.
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

fn file_of(s: &Session, project: &Path, key: &str) -> PathBuf {
    s.agents.sessions.project_dir(project).unwrap().join(format!("{key}.json"))
}

fn read(path: &Path) -> Option<SavedSession> {
    agent_sessions::read_session(path).ok()
}

/// The listing once it is read.
fn listing(s: &mut Session) -> Value {
    wait(s, "the session listing", |s| {
        s.pump();
        !s.execute("agent.sessions.list", &Value::Null).unwrap()["loading"].as_bool().unwrap()
    });
    s.execute("agent.sessions.list", &Value::Null).unwrap()
}

/// A fake `claude` that records its arguments; "@@wait@@" waits for `go`, "@@slow@@" never ends.
fn fake_claude(dir: &Path, log: &Path) -> PathBuf {
    let go = dir.join("go");
    script(
        dir,
        "claude",
        &format!(
            r#"printf '%s\n' "$@" >> '{log}'
echo ---- >> '{log}'
echo '{{"type":"system","subtype":"init","session_id":"sess-A"}}'
case "$*" in *@@slow@@*) exec sleep 30;; esac
case "$*" in *@@wait@@*) while [ ! -f '{go}' ]; do sleep 0.05; done;; esac
echo '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"All done."}}]}},"session_id":"sess-A"}}'
echo '{{"type":"result","subtype":"success","is_error":false,"duration_ms":7,"total_cost_usd":0.1,"result":"ok","session_id":"sess-A"}}'
"#,
            log = log.display(),
            go = go.display()
        ),
    )
}

#[test]
fn sessions_are_saved_listed_after_a_restart_reopened_and_resumed() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let log = tmp.path().join("args.log");
    let fake = fake_claude(tmp.path(), &log);

    let mut s = session(&home, &proj);
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    let (id1, key1) = start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": "First task @@wait@@" }));
    let file1 = file_of(&s, &proj, &key1);
    assert!(file1.starts_with(home.join("agent-sessions")), "{}", file1.display());
    assert!(!file1.starts_with(&proj), "never inside the project");

    // Saved while it runs, with the agent's session id.
    wait(&mut s, "the running session on disk", |_| {
        read(&file1).is_some_and(|x| x.status == JobStatus::Running && x.backend_session_id.as_deref() == Some("sess-A"))
    });
    std::fs::write(tmp.path().join("go"), "").unwrap();
    wait(&mut s, "the first job", |s| status(s, id1) == JobStatus::Finished);
    // ...and after it ends.
    wait(&mut s, "the finished session on disk", |_| {
        read(&file1).is_some_and(|x| {
            x.status == JobStatus::Finished
                && x.cost_usd == Some(0.1)
                && x.activity.iter().any(|a| matches!(a, Activity::Text { text } if text == "All done."))
        })
    });
    let saved = read(&file1).unwrap();
    assert_eq!(saved.title, "First task @@wait@@");
    assert_eq!(saved.mode, "auto");
    assert_eq!(saved.prompt, "First task @@wait@@");
    assert!(saved.created_ms > 0 && saved.updated_ms >= saved.created_ms);

    // A second job is still running when the window closes.
    let (id2, key2) = start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": "Second @@slow@@" }));
    wait(&mut s, "the second job's session id", |s| s.agents.job(JobId(id2)).is_some_and(|j| j.session_id.is_some()));
    s.shutdown();
    let saved2 = read(&file_of(&s, &proj, &key2)).unwrap();
    assert_eq!(saved2.status, JobStatus::Interrupted);
    drop(s);

    // A fresh session (a restart) lists both, newest first.
    let mut s = session(&home, &proj);
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    let list = listing(&mut s);
    let rows = list["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{list}");
    assert_eq!(rows[0]["id"], key2.as_str());
    assert_eq!(rows[0]["status"], "interrupted");
    assert_eq!(rows[1]["id"], key1.as_str());
    assert_eq!(rows[1]["status"], "finished");
    assert_eq!(rows[1]["backend"], "claude");
    assert_eq!(rows[1]["mode"], "auto");
    assert_eq!(rows[1]["title"], "First task @@wait@@");
    assert!(rows[1]["job"].is_null());
    assert!(s.agents.jobs.is_empty(), "nothing is reopened until asked");
    assert_eq!(agent_sessions::history(&s).len(), 2);

    // Reopen the first with its whole feed.
    let job = s.execute("agent.sessions.open", &json!({ "session": key1 })).unwrap()["job"].as_u64().unwrap();
    let detail = s.execute("agent.job", &json!({ "job": job })).unwrap();
    assert_eq!(detail["status"], "finished");
    assert_eq!(detail["sessionId"], "sess-A");
    assert_eq!(detail["session"], key1.as_str());
    let feed = detail["activity"].as_array().unwrap();
    assert_eq!(feed.first().unwrap()["kind"], "prompt");
    assert!(feed.iter().any(|a| a["kind"] == "text" && a["text"] == "All done."));
    // Opening it again gives the same job.
    assert_eq!(s.execute("agent.sessions.open", &json!({ "session": key1 })).unwrap()["job"], job);
    assert_eq!(agent_sessions::history(&s).len(), 1);

    // Resume: `--resume <id>` with the same permission flags, appended to the same session.
    std::fs::write(&log, "").unwrap();
    let r = s.execute("agent.sessions.resume", &json!({ "session": key1, "prompt": "And now the tests" })).unwrap();
    assert_eq!(r["job"], job);
    wait(&mut s, "the resumed turn", |s| status(s, job) == JobStatus::Finished);
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains("--resume\nsess-A"), "{args}");
    assert!(args.contains("--dangerously-skip-permissions"), "{args}");
    assert!(!args.contains("previous-session"), "no transcript when the session can be resumed: {args}");
    wait(&mut s, "the resumed turn on disk", |_| {
        read(&file1).is_some_and(|x| {
            x.status == JobStatus::Finished
                && x.activity.iter().any(|a| matches!(a, Activity::Prompt { text } if text == "And now the tests"))
        })
    });
    let saved = read(&file1).unwrap();
    assert!((saved.cost_usd.unwrap() - 0.2).abs() < 1e-9, "costs add up: {:?}", saved.cost_usd);
    let dir = s.agents.sessions.project_dir(&proj).unwrap();
    let files = std::fs::read_dir(&dir).unwrap().filter_map(Result::ok).count();
    assert_eq!(files, 2, "the resumed turn went into the same file");

    // The interrupted one shows as such and resumes too.
    let job2 = s.execute("agent.sessions.open", &json!({ "session": key2 })).unwrap()["job"].as_u64().unwrap();
    assert_eq!(status(&s, job2), JobStatus::Interrupted);
    let j2 = s.agents.job(JobId(job2)).unwrap();
    assert!(j2.activity.iter().any(|a| matches!(a, Activity::Info { text } if text.starts_with("Interrupted"))));
    assert!(j2.can_follow_up().is_ok());
    s.execute("agent.followUp", &json!({ "job": job2, "prompt": "Carry on" })).unwrap();
    wait(&mut s, "the resumed interrupted job", |s| status(s, job2) == JobStatus::Finished);
    s.shutdown();
}

#[test]
fn a_session_without_an_agent_id_resumes_with_a_transcript() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let log = tmp.path().join("args.log");
    // A Codex that never reports a thread id.
    let fake = script(
        tmp.path(),
        "codex",
        &format!(
            "printf '%s\\n' \"$@\" >> '{log}'\necho ---- >> '{log}'\necho '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"i\",\"type\":\"agent_message\",\"text\":\"Hi there.\"}}}}'\n",
            log = log.display()
        ),
    );
    let mut s = session(&home, &proj);
    s.execute("agent.settings", &json!({ "binaries": { "codex": fake } })).unwrap();
    let (id, key) = start(&mut s, json!({ "backend": "codex", "mode": "auto", "prompt": "Say hello" }));
    wait(&mut s, "the job", |s| status(s, id) == JobStatus::Finished);
    s.shutdown();
    drop(s);

    let mut s = session(&home, &proj);
    s.execute("agent.settings", &json!({ "binaries": { "codex": fake } })).unwrap();
    std::fs::write(&log, "").unwrap();
    let job = s.execute("agent.sessions.resume", &json!({ "session": key, "prompt": "Say goodbye" })).unwrap()["job"]
        .as_u64()
        .unwrap();
    wait(&mut s, "the resumed job", |s| status(s, job) == JobStatus::Finished);
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(!args.lines().any(|l| l == "resume"), "a fresh `codex exec`: {args}");
    assert!(args.contains("<previous-session>"), "{args}");
    assert!(args.contains("User: Say hello"), "{args}");
    assert!(args.contains("Assistant: Hi there."), "{args}");
    assert!(args.contains("Say goodbye"), "{args}");
    s.shutdown();
}

fn fake_acp() -> Option<PathBuf> {
    if !have("python3") {
        eprintln!("skipping: python3 not found");
        return None;
    }
    Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/fake_acp.py"))
}

fn calls(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log).unwrap_or_default().lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[test]
fn ask_sessions_resume_over_acp() {
    let Some(script) = fake_acp() else { return };
    for load in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
        std::fs::create_dir_all(&proj).unwrap();
        let log = tmp.path().join("calls.log");
        let mut cmd = vec![json!("python3"), json!(script), json!("--log"), json!(log)];
        if load {
            cmd.push(json!("--load"));
        }
        let settings = json!({ "acp": { "antigravity": cmd } });

        let mut s = session(&home, &proj);
        s.execute("agent.settings", &settings).unwrap();
        let (id, key) = start(&mut s, json!({ "backend": "antigravity", "mode": "ask", "prompt": "chat hello" }));
        wait(&mut s, "the turn", |s| status(s, id) == JobStatus::Finished);
        s.shutdown();
        drop(s);

        let mut s = session(&home, &proj);
        s.execute("agent.settings", &settings).unwrap();
        let job = s.execute("agent.sessions.resume", &json!({ "session": key, "prompt": "chat more" })).unwrap()
            ["job"]
            .as_u64()
            .unwrap();
        wait(&mut s, "the resumed turn", |s| status(s, job) == JobStatus::Finished);
        let calls = calls(&log);
        let prompts: Vec<&Value> = calls.iter().filter(|c| c["method"] == "session/prompt").collect();
        let last = prompts.last().unwrap();
        let j = s.agents.job(JobId(job)).unwrap();
        if load {
            assert!(
                calls.iter().any(|c| c["method"] == "session/load" && c["params"]["sessionId"] == "fake-session"),
                "{calls:?}"
            );
            assert_eq!(last["text"], "chat more");
            assert!(
                !j.activity.iter().any(|a| matches!(a, Activity::Text { text } if text.contains("Replayed"))),
                "the replayed history is not added twice"
            );
        } else {
            assert!(!calls.iter().any(|c| c["method"] == "session/load"));
            let text = last["text"].as_str().unwrap();
            assert!(text.contains("<previous-session>") && text.contains("User: chat hello"), "{text}");
            assert!(text.ends_with("chat more"), "{text}");
        }
        let texts = j.activity.iter().filter(|a| matches!(a, Activity::Text { text } if text == "Chatting.")).count();
        assert_eq!(texts, 2, "both turns are in the feed");
        s.shutdown();
    }
}

#[test]
fn review_sessions_can_be_reviewed_after_a_restart() {
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
    std::fs::write(repo.join("b.txt"), "b\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let log = tmp.path().join("args.log");
    let fake = script(
        tmp.path(),
        "claude",
        &format!(
            r#"printf '%s\n' "$@" >> '{log}'
pwd -P >> '{log}'
echo ---- >> '{log}'
echo '{{"type":"system","subtype":"init","session_id":"sess-R"}}'
echo 'A' > a.txt
echo 'B' > b.txt
echo '{{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"result":"ok","session_id":"sess-R"}}'
"#,
            log = log.display()
        ),
    );
    let worktrees = tmp.path().join("worktrees");
    let settings = json!({ "binaries": { "claude": fake }, "worktreesDir": worktrees });
    let mut s = session(&home, &repo);
    s.execute("agent.settings", &settings).unwrap();
    let mut keys = Vec::new();
    let mut paths = Vec::new();
    for prompt in ["Shout a and b", "Shout again"] {
        let (id, key) = start(&mut s, json!({ "backend": "claude", "mode": "review", "prompt": prompt }));
        wait(&mut s, "the review", |s| {
            s.agents.job(JobId(id)).is_some_and(|j| j.status == JobStatus::Reviewing && j.review.len() == 2)
        });
        paths.push(s.agents.job(JobId(id)).unwrap().worktree.as_ref().unwrap().path.clone());
        if keys.is_empty() {
            s.execute("agent.review.reject", &json!({ "job": id, "path": "b.txt" })).unwrap();
        }
        keys.push(key);
    }
    s.shutdown();
    drop(s);
    assert!(paths.iter().all(|p| p.is_dir()), "worktrees with unreviewed changes are kept");
    std::fs::remove_dir_all(&paths[1]).unwrap();

    let mut s = session(&home, &repo);
    s.execute("agent.settings", &settings).unwrap();
    let list = listing(&mut s);
    let rows = list["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["worktreeExists"], false);
    assert_eq!(rows[1]["worktreeExists"], true);

    // The first is still reviewable, with its decision.
    let job = s.execute("agent.sessions.open", &json!({ "session": keys[0] })).unwrap()["job"].as_u64().unwrap();
    wait(&mut s, "the review collected again", |s| {
        s.agents.job(JobId(job)).is_some_and(|j| j.review.len() == 2 && !s.agents.any_active())
    });
    let j = s.agents.job(JobId(job)).unwrap();
    assert_eq!(j.status, JobStatus::Reviewing);
    let b = j.review.iter().find(|f| f.rel == "b.txt").unwrap();
    assert_eq!(b.decision, ReviewDecision::Rejected, "the decision survived the restart");
    s.execute("agent.review.acceptAll", &json!({ "job": job })).unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "A\n");
    assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap(), "b\n");
    wait(&mut s, "the review to finish", |s| status(s, job) == JobStatus::Done && !s.agents.any_active());
    assert!(!paths[0].exists(), "the finished review's worktree is removed");

    // Resuming the finished review recreates its worktree at the same place and resumes there.
    std::fs::write(&log, "").unwrap();
    s.execute("agent.sessions.resume", &json!({ "session": keys[0], "prompt": "One more thing" })).unwrap();
    assert_eq!(status(&s, job), JobStatus::Preparing);
    wait(&mut s, "the new round", |s| {
        s.agents.job(JobId(job)).is_some_and(|j| j.status == JobStatus::Reviewing && !j.review.is_empty())
    });
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains("--resume\nsess-R"), "{args}");
    let canonical = paths[0].canonicalize().unwrap();
    assert!(args.contains(&canonical.display().to_string()), "ran in {}: {args}", canonical.display());

    // The second lost its worktree: say so.
    let job2 = s.execute("agent.sessions.open", &json!({ "session": keys[1] })).unwrap()["job"].as_u64().unwrap();
    let j2 = s.agents.job(JobId(job2)).unwrap();
    assert_eq!(j2.status, JobStatus::Done);
    assert!(j2.worktree.is_none());
    assert!(j2.activity.iter().any(|a| matches!(a, Activity::Error { message } if message.contains("no longer exists"))));
    s.shutdown();
}

#[test]
fn managing_sessions_rename_delete_clear_reveal() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let log = tmp.path().join("args.log");
    let fake = fake_claude(tmp.path(), &log);
    let mut s = session(&home, &proj);
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    let mut keys = Vec::new();
    for p in ["one", "two", "three"] {
        let (id, key) = start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": p }));
        wait(&mut s, "the job", |s| status(s, id) == JobStatus::Finished);
        keys.push((id, key));
    }
    let (running, running_key) =
        start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": "four @@slow@@" }));
    wait(&mut s, "all saved", |s| {
        s.pump();
        s.agents.sessions.flush(Duration::from_secs(5));
        keys.iter().all(|(_, k)| file_of(s, &proj, k).is_file()) && file_of(s, &proj, &running_key).is_file()
    });

    // Rename a live job and a saved-only session.
    s.execute("agent.sessions.rename", &json!({ "job": keys[0].0, "title": "Renamed live" })).unwrap();
    s.execute("agent.discard", &json!({ "job": keys[1].0 })).unwrap();
    s.execute("agent.sessions.rename", &json!({ "session": keys[1].1, "title": "Renamed saved" })).unwrap();
    wait(&mut s, "the renames on disk", |s| {
        s.pump();
        s.agents.sessions.flush(Duration::from_secs(5));
        read(&file_of(s, &proj, &keys[0].1)).is_some_and(|x| x.title == "Renamed live")
            && read(&file_of(s, &proj, &keys[1].1)).is_some_and(|x| x.title == "Renamed saved")
    });
    assert!(s.execute("agent.sessions.rename", &json!({ "session": keys[1].1, "title": "  " })).is_err());

    // Reveal: the file manager, or an editor tab.
    let r = s.execute("agent.sessions.reveal", &json!({ "session": keys[1].1 })).unwrap();
    assert_eq!(PathBuf::from(r["file"].as_str().unwrap()), file_of(&s, &proj, &keys[1].1));
    assert!(s.effects.iter().any(|e| matches!(e, codecraft_engine::Effect::OpenUrl(u) if u.starts_with("file://"))));
    s.execute("agent.sessions.reveal", &json!({ "session": keys[1].1, "inEditor": true })).unwrap();
    assert!(s.ws.active().is_some_and(|(_, d)| d.path.as_deref() == Some(file_of(&s, &proj, &keys[1].1).as_path())));

    // Delete one (its job goes too); a running one is refused.
    s.execute("agent.sessions.delete", &json!({ "session": keys[0].1 })).unwrap();
    assert!(s.agents.job(JobId(keys[0].0)).is_none());
    assert!(s.execute("agent.sessions.delete", &json!({ "session": running_key })).is_err());
    assert!(s.agents.sessions.flush(Duration::from_secs(5)));
    assert!(!file_of(&s, &proj, &keys[0].1).exists());

    // Clear keeps the running one.
    let r = s.execute("agent.sessions.clear", &Value::Null).unwrap();
    assert_eq!(r["kept"], 1, "{r}");
    assert_eq!(r["deleted"], 2, "{r}");
    assert!(s.agents.sessions.flush(Duration::from_secs(5)));
    let left: Vec<String> = std::fs::read_dir(s.agents.sessions.project_dir(&proj).unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, [format!("{running_key}.json")]);
    assert_eq!(status(&s, running), JobStatus::Running);

    // Bad params are errors, never panics.
    for bad in [json!({}), json!({ "session": "../x" }), json!({ "session": "nope" }), json!({ "job": 999 })] {
        assert!(s.execute("agent.sessions.open", &bad).is_err(), "{bad}");
        assert!(s.execute("agent.sessions.delete", &bad).is_err() || bad["session"] == "nope", "{bad}");
    }
    assert!(s.execute("agent.sessions.resume", &json!({ "session": running_key, "prompt": "" })).is_err());
    s.shutdown();
}

#[test]
fn pruning_keeps_the_newest_and_never_open_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let fake = fake_claude(tmp.path(), &tmp.path().join("args.log"));
    let mut s = session(&home, &proj);
    s.agents.sessions.max_sessions = 2;
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    let mut keys = Vec::new();
    for p in ["one", "two", "three"] {
        let (id, key) = start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": p }));
        wait(&mut s, "the job", |s| status(s, id) == JobStatus::Finished);
        s.pump();
        // Closed jobs can be pruned; the open one never is.
        s.execute("agent.discard", &json!({ "job": id })).unwrap();
        keys.push(key);
    }
    let (_, last) = start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": "four @@slow@@" }));
    wait(&mut s, "pruning", |s| {
        s.pump();
        s.agents.sessions.flush(Duration::from_secs(5));
        let dir = s.agents.sessions.project_dir(&proj).unwrap();
        std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0) == 2
    });
    assert!(file_of(&s, &proj, &last).is_file());
    assert!(file_of(&s, &proj, &keys[2]).is_file());
    assert!(!file_of(&s, &proj, &keys[0]).exists());
    s.shutdown();
    drop(s);

    // Pruning on load too.
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&home);
    s.agents.sessions.max_sessions = 1;
    s.execute("file.open", &json!({ "path": proj })).unwrap();
    let list = listing(&mut s);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["sessions"][0]["id"], last.as_str(), "the newest is kept");
    s.shutdown();
}

#[test]
fn corrupt_and_oversized_session_files_are_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let dir = home.join("agent-sessions").join(agent_sessions::project_key(&proj));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("bad-1.json"), "{\"id\": \"bad-1\", \"backend\": ").unwrap();
    std::fs::write(dir.join("bad-2.json"), "\u{0}\u{1}garbage").unwrap();
    std::fs::write(dir.join("bad-3.json"), json!({ "backend": "vim", "status": "running" }).to_string()).unwrap();
    let huge = std::fs::File::create(dir.join("huge-1.json")).unwrap();
    huge.set_len(agent_sessions::MAX_READ_BYTES + 1).unwrap();
    let good = json!({
        "id": "good-1", "title": "Good", "backend": "codex", "mode": "review", "status": "preparing",
        "project": proj, "createdMs": 5, "activity": [{ "kind": "prompt", "text": "hi" }, { "kind": "future" }]
    });
    std::fs::write(dir.join("good-1.json"), good.to_string()).unwrap();

    let mut s = session(&home, &proj);
    let list = listing(&mut s);
    let rows = list["sessions"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{list}");
    assert_eq!(rows[0]["status"], "interrupted");
    assert_eq!(list["warnings"].as_array().unwrap().len(), 4, "{list}");
    for bad in ["bad-1", "bad-2", "bad-3", "huge-1", "missing-1"] {
        assert!(s.execute("agent.sessions.open", &json!({ "session": bad })).is_err(), "{bad}");
    }
    let job = s.execute("agent.sessions.open", &json!({ "session": "good-1" })).unwrap()["job"].as_u64().unwrap();
    let j = s.agents.job(JobId(job)).unwrap();
    assert_eq!(j.status, JobStatus::Interrupted);
    assert!(matches!(j.activity.front(), Some(Activity::Prompt { text }) if text == "hi"));
    s.shutdown();
}

#[test]
fn feedback_jobs_are_saved_and_linked_to_their_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let fake = fake_claude(tmp.path(), &tmp.path().join("args.log"));
    let mut s = session(&home, &proj);
    s.feedback.dir = Some(tmp.path().join("records"));
    s.feedback.source_dir = Some(proj.clone());
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    s.execute("feedback.toggle", &Value::Null).unwrap();
    s.feedback.window = [800.0, 600.0];
    let r = s
        .execute(
            "feedback.submit",
            &json!({ "confirm": true, "text": "Make the button blue", "backend": "claude", "mode": "auto" }),
        )
        .unwrap();
    let key = r["session"].as_str().unwrap().to_string();
    let job = r["job"].as_u64().unwrap();
    wait(&mut s, "the feedback job", |s| status(s, job) == JobStatus::Finished);
    let records = s.execute("feedback.history", &json!({ "open": false })).unwrap();
    let text = records.to_string();
    assert!(text.contains(&key), "the record links its session: {text}");
    let rec: Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(r["dir"].as_str().unwrap()).join("feedback.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rec["session"], key.as_str());
    let cwd = PathBuf::from(rec["cwd"].as_str().unwrap());
    wait(&mut s, "the feedback session on disk", |s| {
        s.pump();
        read(&file_of(s, &cwd, &key))
            .is_some_and(|x| x.status == JobStatus::Finished && x.title.starts_with("Feedback: Make the button blue"))
    });
    s.shutdown();
}

#[test]
fn feedback_follow_ups_continue_the_same_agent_session() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, proj) = (tmp.path().join("home"), tmp.path().join("proj"));
    std::fs::create_dir_all(&proj).unwrap();
    let log = tmp.path().join("args.log");
    let fake = fake_claude(tmp.path(), &log);
    let mut s = session(&home, &proj);
    s.feedback.dir = Some(tmp.path().join("records"));
    s.feedback.source_dir = Some(proj.clone());
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();
    s.feedback.window = [800.0, 600.0];
    let first = s
        .execute(
            "feedback.submit",
            &json!({ "confirm": true, "text": "Add a toolbar button @@wait@@", "backend": "claude", "mode": "auto" }),
        )
        .unwrap();
    let key = first["session"].as_str().unwrap().to_string();
    let job = first["job"].as_u64().unwrap();
    let first_id = first["id"].as_str().unwrap().to_string();
    let records = tmp.path().join("records");
    let count = || std::fs::read_dir(&records).unwrap().count();

    // While the agent still works, a follow-up is refused and nothing is saved.
    wait(&mut s, "the feedback job to run", |s| status(s, job) == JobStatus::Running);
    s.execute("feedback.continue", &json!({ "id": first_id })).unwrap();
    assert_eq!(s.feedback.draft.session.as_deref(), Some(key.as_str()));
    let busy = s.execute("feedback.submit", &json!({ "confirm": true, "text": "Also make it red" })).unwrap_err();
    assert!(busy.to_string().contains("still working"), "{busy}");
    assert_eq!(count(), 1, "no record for a refused follow-up");

    std::fs::write(tmp.path().join("go"), "").unwrap();
    wait(&mut s, "the feedback job", |s| status(s, job) == JobStatus::Finished);
    std::fs::write(&log, "").unwrap();
    let r = s.execute("feedback.submit", &json!({ "confirm": true, "text": "Also make it red" })).unwrap();
    assert_eq!(r["resumed"], true, "{r}");
    assert_eq!(r["job"], job, "the same job continues");
    assert_eq!(r["session"], key.as_str());
    wait(&mut s, "the follow-up turn", |s| status(s, job) == JobStatus::Finished);
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains("--resume\nsess-A"), "the agent's own session is resumed: {args}");
    assert!(args.contains("follow-up on the feedback you worked on earlier in this session"), "{args}");
    let rec: Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(r["dir"].as_str().unwrap()).join("feedback.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rec["parent"], first_id.as_str());
    assert_eq!(rec["session"], key.as_str());

    // One thread with both requests, its session the same saved one.
    let h = s.execute("feedback.history", &json!({ "open": false })).unwrap();
    let threads = h["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 1, "{h}");
    assert_eq!(threads[0]["entries"].as_array().unwrap().len(), 2);
    assert_eq!(threads[0]["session"], key.as_str());
    let files = std::fs::read_dir(s.agents.sessions.project_dir(&proj).unwrap()).unwrap().count();
    assert_eq!(files, 1, "the follow-up went into the same saved session");
    s.shutdown();
}
