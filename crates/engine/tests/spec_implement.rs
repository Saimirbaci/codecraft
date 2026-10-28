//! Implement and Reconcile: a requirement handed to a coding agent (a fake agent CLI here, a shell
//! script that edits files) and the agent's changes coming back as a diff to accept.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agents::{JobId, JobStatus};

const REQS: &str = "# Requirements: Login\n\n## REQ-1: Sign in\nWHEN valid credentials are given THE SYSTEM SHALL open a session.\n\n## REQ-2: Lockout\nIF five attempts fail THEN THE SYSTEM SHALL lock the account.\n";
const TASKS: &str = "# Tasks\n\n- [ ] 1. Count failures (REQ-2)\n- [ ] 2. Lock the account (REQ-2)\n";

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
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

/// A project with the login spec and a linked `sign_in`; a git repository when `repo`.
fn project(root: &Path, repo: bool) {
    std::fs::create_dir_all(root.join("specs/login")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("specs/login/requirements.md"), REQS).unwrap();
    std::fs::write(root.join("specs/login/tasks.md"), TASKS).unwrap();
    std::fs::write(root.join("src/auth.rs"), "// spec: REQ-1\nfn sign_in() {}\n").unwrap();
    if repo {
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", "init"]);
    }
}

fn session(home: &Path, root: &Path, fake: &Path, worktrees: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s.execute("file.open", &json!({ "path": root })).unwrap();
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake }, "worktreesDir": worktrees })).unwrap();
    s
}

fn wait(s: &mut Session, what: &str, mut cond: impl FnMut(&mut Session) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(90);
    while !cond(s) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `spec.list` once the scan has finished.
fn list(s: &mut Session) -> Value {
    let mut last = Value::Null;
    wait(s, "the spec scan", |s| {
        last = s.execute("spec.list", &json!({})).unwrap();
        last["pending"] == json!(false)
    });
    last
}

fn rescan(s: &mut Session) -> Value {
    s.execute("spec.refresh", &json!({})).unwrap();
    list(s)
}

fn status(v: &Value, id: &str) -> String {
    let reqs = v["features"][0]["requirements"].as_array().unwrap();
    reqs.iter().find(|r| r["id"] == id).unwrap()["status"].as_str().unwrap().to_string()
}

fn row<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["features"][0]["requirements"].as_array().unwrap().iter().find(|r| r["id"] == id).unwrap()
}

fn job_status(s: &Session, job: u64) -> JobStatus {
    s.agents.job(JobId(job)).map(|j| j.status).unwrap()
}

fn reviewing(s: &mut Session, job: u64) {
    wait(s, "the review", |s| {
        s.agents.job(JobId(job)).is_some_and(|j| j.status == JobStatus::Reviewing && !j.review.is_empty())
    });
}

fn review_paths(s: &Session, job: u64) -> Vec<String> {
    let mut rels: Vec<String> = s.agents.job(JobId(job)).unwrap().review.iter().map(|f| f.rel.clone()).collect();
    rels.sort_unstable();
    rels
}

#[test]
fn implementing_a_requirement_hands_it_to_the_agent_and_accepting_its_diff_links_the_code() {
    if !have_git() {
        eprintln!("skipping: git not found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("proj");
    project(&root, true);
    let log = tmp.path().join("args.log");
    // The agent builds the requirement, links it and ticks the first task.
    let fake = script(
        tmp.path(),
        "claude",
        &format!(
            r#"printf '%s\n' "$@" > '{log}'
printf '// spec: REQ-2\npub fn lock() {{}}\n' > src/lock.rs
printf '# Tasks\n\n- [x] 1. Count failures (REQ-2)\n- [ ] 2. Lock the account (REQ-2)\n' > specs/login/tasks.md
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Done."}}'
"#,
            log = log.display()
        ),
    );
    let mut s = session(&tmp.path().join("home"), &root, &fake, &tmp.path().join("worktrees"));
    let v = list(&mut s);
    assert_eq!(status(&v, "REQ-2"), "unlinked", "{v:#}");
    assert_eq!(row(&v, "REQ-2")["agentJob"], Value::Null);

    // No backend named: the first installed one runs it, in review mode.
    let r = s.execute("spec.implement", &json!({ "feature": "login", "id": "REQ-2" })).unwrap();
    assert_eq!(
        (r["kind"].as_str(), r["mode"].as_str(), r["backend"].as_str()),
        (Some("implement"), Some("review"), Some("claude"))
    );
    let job = r["job"].as_u64().unwrap();
    assert_eq!(r["id"], "REQ-2");

    // The row says who is on it, and a second job for the requirement is refused meanwhile.
    let v = s.execute("spec.list", &json!({})).unwrap();
    let agent = &row(&v, "REQ-2")["agentJob"];
    assert_eq!(
        (agent["id"].as_u64(), agent["kind"].as_str(), agent["working"].as_bool()),
        (Some(job), Some("implement"), Some(true)),
        "{agent:#}"
    );
    let err = s.execute("spec.implement", &json!({ "feature": "login", "id": "REQ-2" })).unwrap_err().to_string();
    assert!(err.contains("already has an agent job"), "{err}");
    // Another requirement is free.
    assert!(row(&v, "REQ-1")["agentJob"].is_null());

    reviewing(&mut s, job);
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains("Implement requirement login/REQ-2 in this project."), "{args}");
    assert!(args.contains("IF five attempts fail THEN THE SYSTEM SHALL lock the account."), "{args}");
    assert!(args.contains("// spec: REQ-2"), "the linking convention is in the brief: {args}");
    assert!(!args.contains("Active file"), "no editor context rides along: {args}");
    let detail = s.execute("agent.job", &json!({ "job": job })).unwrap();
    assert!(detail["title"].as_str().unwrap().starts_with("Implement login/REQ-2: Lockout"), "{detail:#}");

    // The project is untouched until the diff is accepted; the row says the job waits for it.
    assert!(!root.join("src/lock.rs").exists());
    let v = s.execute("spec.list", &json!({})).unwrap();
    assert_eq!(row(&v, "REQ-2")["agentJob"]["status"], "reviewing");
    assert_eq!(row(&v, "REQ-2")["agentJob"]["working"], true);
    assert_eq!(review_paths(&s, job), ["specs/login/tasks.md", "src/lock.rs"]);

    s.execute("agent.review.acceptAll", &json!({ "job": job })).unwrap();
    assert_eq!(job_status(&s, job), JobStatus::Done);
    assert_eq!(std::fs::read_to_string(root.join("src/lock.rs")).unwrap(), "// spec: REQ-2\npub fn lock() {}\n");

    // The code now points at the requirement and the task is ticked.
    let v = rescan(&mut s);
    assert_eq!(status(&v, "REQ-2"), "linked", "{v:#}");
    assert_eq!(v["features"][0]["tasks"], json!({ "done": 1, "total": 2 }));
    // The finished job stays on the row (click-through to its summary) but no longer blocks another.
    assert_eq!(row(&v, "REQ-2")["agentJob"]["status"], "done");
    assert_eq!(row(&v, "REQ-2")["agentJob"]["working"], false);
    let again = s.execute("spec.implement", &json!({ "feature": "login", "id": "REQ-2" })).unwrap();
    let again = again["job"].as_u64().unwrap();
    assert_ne!(again, job);
    // The agent has nothing left to change (the first job's files are already in the project), so
    // the job ends without a review.
    wait(&mut s, "the second job", |s| job_status(s, again) == JobStatus::Done && !s.agents.any_active());
    assert!(s.agents.job(JobId(again)).unwrap().review.is_empty());
    s.shutdown();
}

#[test]
fn reconciling_proposes_a_spec_edit_for_code_that_moved_and_the_user_accepts_it() {
    if !have_git() {
        eprintln!("skipping: git not found");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("proj");
    project(&root, true);
    let log = tmp.path().join("args.log");
    // The agent decides the change is an evolution and rewrites the requirement; it leaves code alone.
    let edited = REQS.replace("open a session.", "open a session and audit the attempt.");
    let fake = script(
        tmp.path(),
        "claude",
        &format!(
            r#"printf '%s\n' "$@" > '{log}'
cat > specs/login/requirements.md <<'SPEC'
{edited}SPEC
echo '{{"type":"result","subtype":"success","is_error":false,"result":"Classification: evolution"}}'
"#,
            log = log.display()
        ),
    );
    let mut s = session(&tmp.path().join("home"), &root, &fake, &tmp.path().join("worktrees"));
    list(&mut s);
    s.execute("spec.markSynced", &json!({ "feature": "login" })).unwrap();

    // Nothing moved yet: nothing to reconcile, and no job is left behind.
    let err = s.execute("spec.reconcile", &json!({ "feature": "login", "id": "REQ-1" })).unwrap_err().to_string();
    assert!(err.contains("has not changed since the last sync"), "{err}");
    assert!(s.agents.jobs.is_empty());

    // The user changes the code (not committed).
    std::fs::write(root.join("src/auth.rs"), "// spec: REQ-1\nfn sign_in() { audit(); }\n").unwrap();
    let v = rescan(&mut s);
    assert_eq!(status(&v, "REQ-1"), "codeChanged", "{v:#}");

    let r = s.execute("spec.reconcile", &json!({ "feature": "login", "id": "REQ-1" })).unwrap();
    assert_eq!(r["kind"], "reconcile");
    let job = r["job"].as_u64().unwrap();
    reviewing(&mut s, job);

    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains("The code that implements requirement login/REQ-1 changed after"), "{args}");
    assert!(args.contains("- modified: src/auth.rs:1 `fn sign_in()`"), "the change is listed: {args}");
    assert!(args.contains("Classification: <kind>"), "{args}");
    assert!(args.contains("Edit only files under specs/"), "{args}");
    // The user's uncommitted code change is in the agent's copy, so the diff is the spec edit alone.
    assert_eq!(review_paths(&s, job), ["specs/login/requirements.md"]);
    assert_eq!(std::fs::read_to_string(root.join("specs/login/requirements.md")).unwrap(), REQS);

    s.execute("agent.review.acceptAll", &json!({ "job": job })).unwrap();
    assert!(
        std::fs::read_to_string(root.join("specs/login/requirements.md")).unwrap().contains("and audit the attempt.")
    );
    assert_eq!(
        std::fs::read_to_string(root.join("src/auth.rs")).unwrap(),
        "// spec: REQ-1\nfn sign_in() { audit(); }\n"
    );

    // The text now differs from the synced one: the spec leads, so reconcile steps aside ...
    let v = rescan(&mut s);
    assert_eq!(status(&v, "REQ-1"), "specChanged", "{v:#}");
    let err = s.execute("spec.reconcile", &json!({ "feature": "login", "id": "REQ-1" })).unwrap_err().to_string();
    assert!(err.contains("the spec leads"), "{err}");
    // ... until the user records spec and code as in sync.
    s.execute("spec.markSynced", &json!({ "feature": "login", "id": "REQ-1" })).unwrap();
    let v = rescan(&mut s);
    assert_eq!(status(&v, "REQ-1"), "inSync", "{v:#}");
    assert_eq!(row(&v, "REQ-1")["codeChanges"], 0);
    s.shutdown();
}

#[test]
fn a_task_names_the_requirement_to_implement_and_leads_the_brief() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("proj");
    project(&root, false);
    let log = tmp.path().join("args.log");
    let fake = script(
        tmp.path(),
        "claude",
        &format!(
            "printf '%s\\n' \"$@\" > '{log}'\necho '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}}'\n",
            log = log.display()
        ),
    );
    let mut s = session(&tmp.path().join("home"), &root, &fake, &tmp.path().join("worktrees"));
    list(&mut s);

    // Not a repository: the default review mode says what to do, and nothing was started.
    let err = s.execute("spec.implement", &json!({ "feature": "login", "task": 4 })).unwrap_err().to_string();
    assert!(err.contains("git repository"), "{err}");
    assert!(s.agents.jobs.is_empty());

    // Auto mode works on the project itself. Line 4 of tasks.md is the second task.
    let r = s.execute("spec.implement", &json!({ "feature": "login", "task": 4, "mode": "auto" })).unwrap();
    assert_eq!(r["id"], "REQ-2", "the task's requirement: {r:#}");
    let job = r["job"].as_u64().unwrap();
    wait(&mut s, "the job", |s| job_status(s, job) == JobStatus::Finished);
    let args = std::fs::read_to_string(&log).unwrap();
    assert!(args.contains("Your task:\n- [ ] 2 Lock the account (REQ-2) (line 4 of tasks.md)"), "{args}");
    assert!(args.contains("Other tasks for the requirement:\n- [ ] 1 Count failures (REQ-2)"), "{args}");
    s.shutdown();
}

#[test]
fn bad_requests_start_no_job_and_say_why() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("proj");
    project(&root, false);
    let fake =
        script(tmp.path(), "claude", "echo '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false}'\n");
    let mut s = session(&tmp.path().join("home"), &root, &fake, &tmp.path().join("worktrees"));

    // Until the scan is there, the brief cannot know the code.
    let err = s.execute("spec.implement", &json!({ "feature": "login", "id": "REQ-1" })).unwrap_err().to_string();
    assert!(err.contains("still being scanned"), "{err}");
    list(&mut s);

    let cases = [
        (json!({ "feature": "login" }), "pass `id`"),
        (json!({ "feature": "login", "id": "REQ-9" }), "has no requirement REQ-9"),
        (json!({ "feature": "login", "task": 99 }), "no task at line 99"),
        (json!({ "feature": "nope", "id": "REQ-1" }), "no feature `nope`"),
        (json!({ "feature": "../x", "id": "REQ-1" }), "not a feature name"),
        (json!({ "feature": "login", "id": "REQ-1", "mode": "fast" }), "unknown mode `fast`"),
        (json!({ "feature": "login", "id": "REQ-1", "backend": "gpt" }), "unknown backend `gpt`"),
    ];
    for (params, expected) in cases {
        for cmd in ["spec.implement", "spec.reconcile"] {
            // `task` is for implementing only.
            if cmd == "spec.reconcile" && params.get("task").is_some() {
                continue;
            }
            let err = s.execute(cmd, &params).unwrap_err().to_string();
            assert!(err.contains(expected), "{cmd} {params}: {err}");
        }
    }
    assert!(s.agents.jobs.is_empty(), "no job was started");
    s.shutdown();
}
