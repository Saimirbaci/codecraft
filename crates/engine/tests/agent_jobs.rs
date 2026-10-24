//! Agent jobs end to end with fake agent CLIs (shell scripts that print recorded stream lines and
//! edit files) and a fake ACP agent. The real CLIs are never run.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agents::{Activity, JobId, JobStatus, ReviewDecision};
use codecraft_engine::approvals::ApprovalKind;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn session() -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s
}

fn wait(s: &mut Session, what: &str, mut cond: impl FnMut(&Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while !cond(s) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn status(s: &Session, id: u64) -> JobStatus {
    s.agents.job(JobId(id)).map(|j| j.status).unwrap()
}

fn start(s: &mut Session, params: Value) -> u64 {
    let r = s.execute("agent.start", &params).unwrap();
    r["job"].as_u64().unwrap()
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

const CLAUDE_STREAM_1: &str = r#"{"type":"system","subtype":"init","session_id":"sess-1","cwd":"."}
{"type":"assistant","message":{"content":[{"type":"text","text":"I will **edit** a.rs."},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":"src/a.rs","old_string":"a","new_string":"b"}}]},"session_id":"sess-1"}"#;
const CLAUDE_STREAM_2: &str = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
{"type":"result","subtype":"success","is_error":false,"duration_ms":12,"total_cost_usd":0.25,"result":"Done.","session_id":"sess-1"}"#;

#[test]
fn auto_mode_edits_the_project_and_reloads_open_files() {
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(proj.join("src/a.rs"), "fn a() {}\n").unwrap();
    let args_log = tmp.path().join("args.log");
    let fake = script(
        tmp.path(),
        "claude",
        &format!(
            "printf '%s\\n' \"$@\" >> '{log}'\necho ---- >> '{log}'\ncat <<'EOF'\n{CLAUDE_STREAM_1}\nEOF\nprintf 'fn a() {{ changed() }}\\n' > src/a.rs\ncat <<'EOF'\n{CLAUDE_STREAM_2}\nEOF\n",
            log = args_log.display()
        ),
    );
    let mut s = session();
    s.execute("file.open", &json!({ "path": proj })).unwrap();
    s.execute("file.open", &json!({ "path": proj.join("src/a.rs") })).unwrap();
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake } })).unwrap();

    let id = start(&mut s, json!({ "backend": "claude", "mode": "auto", "prompt": "Make a() call changed()" }));
    wait(&mut s, "the job to finish", |s| status(s, id) == JobStatus::Finished);

    assert_eq!(std::fs::read_to_string(proj.join("src/a.rs")).unwrap(), "fn a() { changed() }\n");
    let doc = s.ws.active().map(|(_, d)| d.doc.text()).unwrap();
    assert_eq!(doc, "fn a() { changed() }\n", "the open editor was reloaded");
    let job = s.agents.job(JobId(id)).unwrap();
    assert_eq!(job.session_id.as_deref(), Some("sess-1"));
    assert_eq!(job.cost_usd, Some(0.25));
    assert_eq!(job.duration_ms, Some(12));
    assert!(job.activity.iter().any(|a| matches!(a, Activity::Text { text } if text.contains("**edit**"))));
    assert!(job.activity.iter().any(|a| matches!(a, Activity::Tool { summary, .. } if summary == "Editing src/a.rs")));
    assert!(job.changed_files.contains(&proj.join("src/a.rs")));
    let args = std::fs::read_to_string(&args_log).unwrap();
    assert!(args.contains("--dangerously-skip-permissions"));
    assert!(args.contains("stream-json"));
    assert!(args.contains("Context from the IDE"), "the prompt carries IDE context");
    assert!(args.contains("Active file: src/a.rs"));

    // A follow-up resumes the session.
    s.execute("agent.followUp", &json!({ "job": id, "prompt": "and now tests" })).unwrap();
    assert_eq!(status(&s, id), JobStatus::Running);
    wait(&mut s, "the follow-up", |s| status(s, id) == JobStatus::Finished);
    let args = std::fs::read_to_string(&args_log).unwrap();
    let second = args.split("----").nth(1).unwrap();
    assert!(second.contains("--resume\nsess-1"), "{second}");
    assert_eq!(s.agents.job(JobId(id)).unwrap().cost_usd, Some(0.5));

    // The listing commands.
    let jobs = s.execute("agent.jobs", &Value::Null).unwrap();
    assert_eq!(jobs[0]["status"], "finished");
    let detail = s.execute("agent.job", &json!({ "job": id })).unwrap();
    assert!(detail["activity"].as_array().is_some_and(|a| a.len() > 4));
    s.execute("agent.discard", &json!({ "job": id })).unwrap();
    assert!(s.agents.jobs.is_empty());
    s.shutdown();
}

#[test]
fn failing_and_stopped_jobs() {
    let tmp = tempfile::tempdir().unwrap();
    let fail = script(tmp.path(), "antigravity", "echo '{\"event\":\"init\",\"conversation_id\":\"g\"}'\necho boom >&2\nexit 2\n");
    let slow = script(tmp.path(), "codex", "echo '{\"type\":\"thread.started\",\"thread_id\":\"t\"}'\nexec sleep 30\n");
    let mut s = session();
    s.execute("agent.settings", &json!({ "binaries": { "antigravity": fail, "codex": slow } })).unwrap();
    let cwd = tmp.path().to_path_buf();

    let id = start(&mut s, json!({ "backend": "antigravity", "mode": "auto", "prompt": "x", "cwd": cwd }));
    wait(&mut s, "failure", |s| status(s, id) == JobStatus::Failed);
    let job = s.agents.job(JobId(id)).unwrap();
    assert!(job.error.as_deref().is_some_and(|e| e.contains("code 2")), "{:?}", job.error);
    assert!(job.activity.iter().any(|a| matches!(a, Activity::Log { line } if line == "boom")));

    let id = start(&mut s, json!({ "backend": "codex", "mode": "auto", "prompt": "x", "cwd": cwd }));
    wait(&mut s, "the session id", |s| s.agents.job(JobId(id)).is_some_and(|j| j.session_id.is_some()));
    let t = Instant::now();
    s.execute("agent.stop", &json!({ "job": id })).unwrap();
    wait(&mut s, "stop", |s| status(s, id) == JobStatus::Stopped);
    assert!(t.elapsed() < Duration::from_secs(10));

    // Bad params never start anything.
    for bad in [
        json!({}),
        json!({ "backend": "vim", "prompt": "x" }),
        json!({ "backend": "codex", "prompt": "" }),
        json!({ "backend": "codex", "mode": "yolo", "prompt": "x" }),
        json!({ "backend": "codex", "prompt": "x", "attachments": "nope" }),
    ] {
        assert!(s.execute("agent.start", &bad).is_err(), "{bad}");
    }
    // Review mode outside a git repository is refused up front.
    let plain = tempfile::tempdir().unwrap();
    let err =
        s.execute("agent.start", &json!({ "backend": "codex", "mode": "review", "prompt": "x", "cwd": plain.path() }));
    assert!(err.is_err());
    s.shutdown();
}

#[test]
fn review_mode_produces_reviewable_changes_from_a_worktree() {
    if !have("git") {
        eprintln!("skipping: git not found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::fs::write(repo.join("keep.txt"), "keep\n").unwrap();
    std::fs::write(repo.join("other.txt"), "other\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    // Uncommitted user work travels into the worktree.
    std::fs::write(repo.join("other.txt"), "other (wip)\n").unwrap();

    let args_log = tmp.path().join("args.log");
    let fake = script(
        tmp.path(),
        "codex",
        &format!(
            r#"printf '%s\n' "$@" > '{log}'
pwd >> '{log}'
cat other.txt >> '{log}'
echo '{{"type":"thread.started","thread_id":"th-9"}}'
echo '{{"type":"item.started","item":{{"id":"i1","type":"file_change","changes":[{{"path":"src/a.rs","kind":"update"}}],"status":"in_progress"}}}}'
printf 'fn a() {{ agent() }}\n' > src/a.rs
printf 'new\n' > src/new.rs
rm keep.txt
echo '{{"type":"item.completed","item":{{"id":"i1","type":"file_change","changes":[{{"path":"src/a.rs","kind":"update"}},{{"path":"src/new.rs","kind":"add"}},{{"path":"keep.txt","kind":"delete"}}],"status":"completed"}}}}'
echo '{{"type":"item.completed","item":{{"id":"i2","type":"agent_message","text":"Done."}}}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":1}}}}'
"#,
            log = args_log.display()
        ),
    );
    let worktrees = tmp.path().join("worktrees");
    let mut s = session();
    s.execute("file.open", &json!({ "path": repo })).unwrap();
    s.execute("file.open", &json!({ "path": repo.join("src/a.rs") })).unwrap();
    s.execute("agent.settings", &json!({ "binaries": { "codex": fake }, "worktreesDir": worktrees })).unwrap();

    let id = start(&mut s, json!({ "backend": "codex", "mode": "review", "prompt": "Change a()" }));
    assert_eq!(status(&s, id), JobStatus::Preparing);
    wait(&mut s, "the review", |s| {
        s.agents.job(JobId(id)).is_some_and(|j| j.status == JobStatus::Reviewing && !j.review.is_empty())
    });
    let log = std::fs::read_to_string(&args_log).unwrap();
    assert!(log.contains("--dangerously-bypass-approvals-and-sandbox"));
    assert!(log.contains(&worktrees.display().to_string()), "the agent ran in the worktree: {log}");
    assert!(log.contains("other (wip)"), "uncommitted work came along: {log}");

    // The project is untouched until something is accepted.
    assert_eq!(std::fs::read_to_string(repo.join("src/a.rs")).unwrap(), "fn a() {}\n");
    assert!(repo.join("keep.txt").exists());
    let job = s.agents.job(JobId(id)).unwrap();
    let wt_path = job.worktree.as_ref().unwrap().path.clone();
    let branch = job.worktree.as_ref().unwrap().branch.clone();
    assert!(wt_path.starts_with(&worktrees));
    let mut rels: Vec<&str> = job.review.iter().map(|f| f.rel.as_str()).collect();
    rels.sort_unstable();
    assert_eq!(rels, ["keep.txt", "src/a.rs", "src/new.rs"], "only the agent's changes, not other.txt");
    let a = job.review.iter().find(|f| f.rel == "src/a.rs").unwrap();
    assert_eq!(a.original.as_deref(), Some("fn a() {}\n"));
    assert_eq!(a.proposed.as_deref(), Some("fn a() { agent() }\n"));
    assert!(!a.conflict);

    // Accept the open file: an undoable edit in the editor, not a disk write.
    s.execute("agent.review.accept", &json!({ "job": id, "path": "src/a.rs" })).unwrap();
    assert_eq!(s.ws.active().map(|(_, d)| d.doc.text()).unwrap(), "fn a() { agent() }\n");
    assert_eq!(std::fs::read_to_string(repo.join("src/a.rs")).unwrap(), "fn a() {}\n");
    s.execute("edit.undo", &Value::Null).unwrap();
    assert_eq!(s.ws.active().map(|(_, d)| d.doc.text()).unwrap(), "fn a() {}\n");
    s.execute("edit.redo", &Value::Null).unwrap();
    // Accept the new file (disk), reject the deletion.
    s.execute("agent.review.accept", &json!({ "job": id, "path": repo.join("src/new.rs") })).unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("src/new.rs")).unwrap(), "new\n");
    assert_eq!(status(&s, id), JobStatus::Reviewing);
    s.execute("agent.review.rejectAll", &json!({ "job": id })).unwrap();
    assert!(repo.join("keep.txt").exists());
    let job = s.agents.job(JobId(id)).unwrap();
    assert_eq!(job.status, JobStatus::Done);
    assert_eq!(job.review.iter().find(|f| f.rel == "keep.txt").unwrap().decision, ReviewDecision::Rejected);
    assert!(job.worktree.is_none());
    assert_eq!(job.cwd, job.project, "after the review the job points back at the project");
    // A follow-up starts a new review round in a new worktree (the old one is being removed).
    s.execute("agent.followUp", &json!({ "job": id, "prompt": "more" })).unwrap();
    assert_eq!(status(&s, id), JobStatus::Preparing);

    // The worktree and its branch are cleaned up in the background.
    wait(&mut s, "cleanup", |s| !s.agents.any_active());
    let again = s.agents.job(JobId(id)).and_then(|j| j.worktree.as_ref()).map(|w| w.path.clone());
    assert!(again.is_some_and(|p| p != wt_path));
    assert!(!wt_path.exists());
    let branches = Command::new("git").arg("-C").arg(&repo).args(["branch", "--list", &branch]).output().unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty(), "branch {branch} remains");
    s.shutdown();
}

#[test]
fn review_with_no_changes_cleans_up_and_discard_removes_worktree() {
    if !have("git") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("f.txt"), "f\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let noop =
        script(tmp.path(), "claude", "echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n");
    let edit =
        script(tmp.path(), "grok", "printf 'g\\n' > f.txt\necho '{\"role\":\"assistant\",\"content\":\"ok\"}'\n");
    let worktrees = tmp.path().join("wt");
    let mut s = session();
    s.execute("agent.settings", &json!({ "binaries": { "claude": noop, "grok": edit }, "worktreesDir": worktrees }))
        .unwrap();

    let id = start(&mut s, json!({ "backend": "claude", "mode": "review", "prompt": "nothing", "cwd": repo }));
    wait(&mut s, "done", |s| status(s, id) == JobStatus::Done && !s.agents.any_active());
    assert!(std::fs::read_dir(&worktrees).map(|d| d.count()).unwrap_or(0) == 0, "no worktree left behind");

    let id = start(&mut s, json!({ "backend": "grok", "mode": "review", "prompt": "edit", "cwd": repo }));
    wait(&mut s, "review", |s| s.agents.job(JobId(id)).is_some_and(|j| j.review.len() == 1));
    let wt = s.agents.job(JobId(id)).unwrap().worktree.clone().unwrap();
    s.execute("agent.discard", &json!({ "job": id })).unwrap();
    wait(&mut s, "cleanup", |s| !s.agents.any_active());
    assert!(!wt.path.exists());
    assert_eq!(std::fs::read_to_string(repo.join("f.txt")).unwrap(), "f\n");
    s.shutdown();
}

#[test]
fn ask_mode_routes_permissions_and_writes_through_the_engine() {
    if !have("python3") {
        eprintln!("skipping: python3 not found");
        return;
    }
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/fake_acp.py");
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("hello.txt"), "hello\n").unwrap();
    let mut s = session();
    s.execute("file.open", &json!({ "path": proj })).unwrap();
    s.execute("file.open", &json!({ "path": proj.join("hello.txt") })).unwrap();
    // The open document has unsaved edits: the agent must read those, not the disk.
    s.execute("editor.type", &json!({ "text": "well, " })).unwrap();
    s.execute("agent.settings", &json!({ "acp": { "antigravity": ["python3", fake] } })).unwrap();
    let backends = s.execute("agent.backends", &Value::Null).unwrap();
    let gem = backends.as_array().unwrap().iter().find(|b| b["id"] == "antigravity").unwrap();
    assert!(gem["modes"].as_array().unwrap().contains(&json!("ask")));

    let id = start(&mut s, json!({ "backend": "antigravity", "mode": "ask", "prompt": "shout" }));
    wait(&mut s, "an approval", |s| !s.approvals.is_empty());
    let req = s.approvals.pending().next().cloned().unwrap();
    assert_eq!(req.source, format!("Antigravity (job {id})"));
    match &req.kind {
        ApprovalKind::FileEdit { path, original, proposed } => {
            assert_eq!(path, &proj.join("hello.txt"));
            assert_eq!(original, "well, hello\n");
            assert_eq!(proposed, "WELL, HELLO\n");
        }
        other => panic!("unexpected {other:?}"),
    }
    s.execute("agent.approve", &json!({ "id": req.id.0, "decision": "allow" })).unwrap();
    wait(&mut s, "the turn", |s| status(s, id) == JobStatus::Finished);
    assert_eq!(s.ws.active().map(|(_, d)| d.doc.text()).unwrap(), "WELL, HELLO\n");
    assert_eq!(
        std::fs::read_to_string(proj.join("hello.txt")).unwrap(),
        "WELL, HELLO\n",
        "saved for the agent's tools"
    );
    s.execute("edit.undo", &Value::Null).unwrap();
    assert_eq!(s.ws.active().map(|(_, d)| d.doc.text()).unwrap(), "well, hello\n", "the agent's write is undoable");
    let job = s.agents.job(JobId(id)).unwrap();
    assert!(job.activity.iter().any(|a| matches!(a, Activity::Plan { .. })));
    assert!(job.activity.iter().any(|a| matches!(a, Activity::Tool { summary, .. } if summary == "Edit hello.txt")));
    assert!(job.has_live_session());

    // Follow-up on the same ACP session: a command, denied.
    s.execute("agent.followUp", &json!({ "job": id, "prompt": "run the tests" })).unwrap();
    wait(&mut s, "a command approval", |s| !s.approvals.is_empty());
    let req = s.approvals.pending().next().cloned().unwrap();
    assert!(matches!(&req.kind, ApprovalKind::Command { command, .. } if command == "cargo test"));
    assert!(s.execute("agent.approve", &json!({ "id": req.id.0, "decision": "maybe" })).is_err());
    s.execute("agent.approve", &json!({ "id": req.id.0, "decision": "deny" })).unwrap();
    wait(&mut s, "the second turn", |s| status(s, id) == JobStatus::Finished);
    assert!(s.execute("agent.approve", &json!({ "id": req.id.0, "decision": "deny" })).is_err(), "already answered");

    s.execute("agent.discard", &json!({ "job": id })).unwrap();
    s.shutdown();
}
