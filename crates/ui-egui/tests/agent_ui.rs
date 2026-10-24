//! The Agent tool window and the approval cards, driven headlessly with fake agent CLIs.
//! Set `CODECRAFT_SNAPSHOT_DIR` to also write PNGs of each state for a visual check.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::json;

use codecraft_engine::agents::{JobId, JobStatus, ReviewDecision};
use codecraft_ui_egui::App;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            h.run_steps(2);
            return;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    panic!("timed out waiting for {what}");
}

fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    let image = h.render().unwrap();
    image.save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

fn repo(dir: &Path) -> Option<PathBuf> {
    let repo = dir.join("demo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")
        .unwrap();
    std::fs::write(
        repo.join("src/main.rs"),
        "fn main() {\n    let name = \"world\";\n    println!(\"hello {name}\");\n}\n\nfn helper() -> u32 {\n    41\n}\n",
    )
    .unwrap();
    std::fs::write(repo.join("NOTES.md"), "notes\n").unwrap();
    (git(&repo, &["init", "-q", "-b", "main"])
        && git(&repo, &["add", "-A"])
        && git(&repo, &["commit", "-q", "-m", "init"]))
    .then_some(repo)
}

#[test]
fn review_job_in_the_agent_window() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(repo) = repo(tmp.path()) else {
        eprintln!("skipping: git not available");
        return;
    };
    let fake = script(
        tmp.path(),
        "claude",
        r#"cat <<'EOF'
{"type":"system","subtype":"init","session_id":"s-ui"}
{"type":"assistant","message":{"content":[{"type":"text","text":"I'll make `helper` return **42** and add a test."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/main.rs"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"fn main() {}"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"src/main.rs"}}]}}
EOF
cat > src/main.rs <<'EOF'
fn main() {
    let name = "world";
    println!("hello {name}!");
}

fn helper() -> u32 {
    42
}

#[test]
fn helper_is_42() {
    assert_eq!(helper(), 42);
}
EOF
rm NOTES.md
printf 'pub fn extra() {}\n' > src/extra.rs
cat <<'EOF'
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","content":"ok"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"cargo test"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t3","content":"test result: ok"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"Done: `helper` now returns 42.\n\n- added a test\n- removed NOTES.md"}]}}
{"type":"result","subtype":"success","is_error":false,"duration_ms":8123,"total_cost_usd":0.42,"session_id":"s-ui"}
EOF
"#,
    );
    let worktrees = tmp.path().join("worktrees");
    let open = repo.join("src/main.rs");
    let mut h = Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .with_pixels_per_point(2.0)
        .build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    let s = &mut h.state_mut().session;
    s.execute("agent.settings", &json!({ "binaries": { "claude": fake }, "worktreesDir": worktrees })).unwrap();
    s.execute("view.toolWindows.agent", &json!(null)).unwrap();
    h.state_mut().ui.agent.prompt = "Make helper return 42 and add a test".into();
    h.run_steps(2);
    h.get_by_label("Send").click();
    settle(&mut h, "the review", |a| a.session.agents.jobs.first().is_some_and(|j| !j.review.is_empty()));
    snapshot(&mut h, "agent-review");
    let job = h.state().session.agents.jobs.first().unwrap();
    assert_eq!(job.status, JobStatus::Reviewing);
    assert_eq!(job.review.len(), 3);
    let id = job.id;

    h.state_mut().ui.agent.side_by_side = true;
    h.state_mut().ui.agent.review_file = Some("src/main.rs".into());
    h.run_steps(2);
    snapshot(&mut h, "agent-review-side-by-side");

    h.get_by_label("Accept All").click();
    settle(&mut h, "the decisions", |a| {
        a.session.agents.job(id).is_some_and(|j| j.review.iter().all(|f| f.decision == ReviewDecision::Accepted))
    });
    let text = h.state().session.ws.active().map(|(_, d)| d.doc.text()).unwrap();
    assert!(text.contains("helper_is_42"), "the open file got the agent's version");
    assert!(repo.join("src/extra.rs").exists());
    assert!(!repo.join("NOTES.md").exists());
    settle(&mut h, "cleanup", |a| {
        a.session.agents.job(id).is_some_and(|j| j.status == JobStatus::Done) && !a.session.agents.any_active()
    });
    snapshot(&mut h, "agent-done");
    h.state_mut().session.shutdown();
}

#[test]
fn approval_cards_answer_ask_mode_requests() {
    let ok = Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: python3 not found");
        return;
    }
    let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/fake_acp.py");
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("p");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("hello.txt"), "hello agent\nsecond line\n").unwrap();
    let open = proj.join("hello.txt");
    let mut h = Harness::builder()
        .with_size(egui::vec2(1440.0, 900.0))
        .with_pixels_per_point(2.0)
        .build_eframe(move |_| App::ephemeral(Some(open)));
    h.run_steps(2);
    let s = &mut h.state_mut().session;
    s.execute("agent.settings", &json!({ "acp": { "antigravity": ["python3", fake] } })).unwrap();
    s.execute("file.open", &json!({ "path": proj })).ok();
    let id = s
        .execute("agent.start", &json!({ "backend": "antigravity", "mode": "ask", "prompt": "shout", "cwd": proj }))
        .unwrap()["job"]
        .as_u64()
        .unwrap();
    settle(&mut h, "an approval card", |a| !a.session.approvals.is_empty());
    assert!(h.query_by_label_contains("1 pending approval").is_some());
    snapshot(&mut h, "approval-card");
    h.get_by_label("Allow").click();
    settle(&mut h, "the turn", |a| a.session.agents.job(JobId(id)).is_some_and(|j| j.status == JobStatus::Finished));
    assert_eq!(std::fs::read_to_string(proj.join("hello.txt")).unwrap(), "HELLO AGENT\nSECOND LINE\n");
    assert!(h.state().session.approvals.is_empty());
    snapshot(&mut h, "agent-ask-finished");

    // A command approval, denied with Esc after focusing the card.
    h.state_mut().session.execute("agent.followUp", &json!({ "job": id, "prompt": "run the tests" })).unwrap();
    settle(&mut h, "a command card", |a| !a.session.approvals.is_empty());
    h.get_by_label_contains("wants to").click();
    h.run_steps(2);
    h.key_press(egui::Key::Escape);
    settle(&mut h, "the denial", |a| a.session.approvals.is_empty());
    settle(&mut h, "the second turn", |a| {
        a.session.agents.job(JobId(id)).is_some_and(|j| j.status == JobStatus::Finished)
    });
    h.state_mut().session.shutdown();
}
