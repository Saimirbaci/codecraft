//! Review labels and explicit confirmation, using a local git repo and fake CLI only.
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::panic)]
use codecraft_ui_egui::App;
use egui_kittest::{Harness, kittest::Queryable};
use serde_json::json;
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, Instant},
};
fn git(path: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .current_dir(path)
            .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success()
    );
}
#[test]
fn codecraft_review_requires_confirm_other_projects_keep_accept_all() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("file.rs"), "fn before() {}\n").unwrap();
    git(&repo, &["add", "--", "file.rs"]);
    git(&repo, &["commit", "-m", "initial"]);
    let fake = dir.path().join("fake codex");
    std::fs::write(
        &fake,
        r#"#!/bin/sh
echo '{"type":"thread.started","thread_id":"ui-feedback"}'
printf 'fn after() {}\n' > file.rs
echo '{"type":"turn.completed","usage":{"input_tokens":1}}'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let home = dir.path().join("home");
    let file = repo.join("file.rs");
    let mut h = Harness::builder().with_size(egui::vec2(1440.0, 1000.0)).build_eframe(move |_| {
        let mut app = App::ephemeral(None);
        app.session.isolate_home(&home);
        app.session.lsp_enabled = false;
        app.session.execute("file.open", &json!({"path":file})).unwrap();
        app
    });
    let s = &mut h.state_mut().session;
    s.execute("agent.settings", &json!({"binaries":{"codex":fake},"worktreesDir":dir.path().join("worktrees")}))
        .unwrap();
    let tool = dir.path().join("fake toolchain");
    std::fs::write(&tool, "#!/bin/sh\necho 'cargo 1.95.0'\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.selfupdate.tools.cargo = tool.clone();
    s.selfupdate.tools.rustc = tool;
    s.selfupdate.loaded = true;
    s.selfupdate.settings.disk_bytes = 0;
    s.execute("view.toolWindows.agent", &json!(null)).unwrap();
    let id = s
        .execute("agent.start", &json!({"backend":"codex","mode":"review","prompt":"feedback","cwd":repo}))
        .unwrap()["job"]
        .as_u64()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.run_steps(1);
        if h.state().session.agents.jobs.iter().any(|j| !j.review.is_empty()) {
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    h.run_steps(2);
    assert!(h.query_by_label("Accept All").is_some());
    assert!(h.query_by_label("Accept, Build & Restart").is_none());
    h.state_mut().session.feedback.source_dir = Some(repo.clone());
    h.run_steps(2);
    assert!(h.query_by_label("Accept All").is_none());
    assert!(h.query_by_label("Accept only").is_some());
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.state().session.selfupdate.busy || h.state().session.selfupdate.report.is_none() {
        assert!(Instant::now() < deadline);
        h.run_steps(1);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(h.state().session.selfupdate.report.as_ref().unwrap().ready());
    h.get_by_label("Accept, Build & Restart").click();
    h.run_steps(2);
    assert!(h.query_by_label("Build CodeCraft from feedback?").is_some());
    assert!(h.query_by_label("Build & Restart").is_some());
    assert!(h.query_by_label("Build only").is_some());
    assert!(!h.state().session.selfupdate.busy);
    assert_eq!(std::fs::read_to_string(repo.join("file.rs")).unwrap(), "fn before() {}\n");
    h.get_by_label("Cancel").click();
    h.run_steps(2);
    assert!(h.state().session.selfupdate.confirm.is_none());
    h.state_mut().session.selfupdate.report =
        Some(codecraft_selfupdate::preflight::evaluate(codecraft_selfupdate::preflight::Probes::default(), "1.95", 0));
    h.run_steps(2);
    assert!(h.query_by_label("Open rustup.rs").is_some());
    assert!(h.query_by_label("Check requirements again").is_some());
    h.state_mut().session.feedback.history.push(codecraft_engine::feedback::Summary {
        id: "stale record".into(),
        created: String::new(),
        title: "old request".into(),
        backend: "codex".into(),
        target: codecraft_engine::feedback::Target::CodeCraft,
        parent: None,
        job: Some(id),
        shapes: 0,
        session: None,
        root: String::new(),
        text: String::new(),
    });
    h.state_mut().session.feedback.source_dir = None;
    h.run_steps(2);
    assert!(h.query_by_label("Accept All").is_some(), "stale job ids must not identify a new CodeCraft job");
    assert!(h.query_by_label("Accept, Build & Restart").is_none());
    assert!(h.state_mut().session.execute("feedback.acceptAndBuild", &json!({"job":id,"confirm":true})).is_err());
    h.state_mut().session.shutdown();
}
