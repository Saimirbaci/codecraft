//! The Spec View: docked under Project, lists features and requirements with their status, and
//! every click is a `spec.*` command.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use egui::Event;
use egui_kittest::Harness;
use egui_kittest::kittest::{NodeT, Queryable};

use codecraft_engine::agents::JobStatus;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

const REQS: &str = "# Requirements: Login\n\n## REQ-1: Sign in\nWHEN valid credentials are given THE SYSTEM SHALL open a session.\n\n## REQ-2: Lockout\nIF five attempts fail THEN THE SYSTEM SHALL lock the account.\n";

fn open_project() -> (Harness<'static, App>, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::mem::forget(dir);
    std::fs::create_dir_all(root.join("specs/login")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"d\"\nversion=\"0.1.0\"\n").unwrap();
    std::fs::write(root.join("specs/login/requirements.md"), REQS).unwrap();
    std::fs::write(
        root.join("specs/login/tasks.md"),
        "- [x] 1. Sign in _Requirements: REQ-1_\n- [ ] 2. Lock _Requirements: REQ-2_\n",
    )
    .unwrap();
    std::fs::write(root.join("src/auth.rs"), "// spec: REQ-1\nfn sign_in() {}\n").unwrap();
    std::fs::write(root.join("src/api.rs"), "// spec: required\npub fn bare() {}\n").unwrap();
    let r = root.clone();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(r)));
    h.run_steps(5);
    h.state_mut().session.ws.layout.show(ToolWindowId::Spec);
    // Tall enough for a form and its buttons to be on screen.
    h.state_mut().session.ws.layout.set_size(ToolWindowId::Spec, 520.0);
    for _ in 0..300 {
        h.run_steps(1);
        let s = &h.state().session.spec;
        if !s.busy() && !s.features.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(3);
    (h, root)
}

#[test]
fn the_spec_view_lists_features_requirements_and_statuses_under_project() {
    let (h, _root) = open_project();
    let layout = &h.state().session.ws.layout;
    assert_eq!((layout.left, layout.left_lower), (Some(ToolWindowId::Project), Some(ToolWindowId::Spec)));
    assert!(h.query_by_label_contains("login").is_some(), "the feature");
    assert!(h.query_by_label("REQ-1").is_some() && h.query_by_label("REQ-2").is_some());
    assert!(h.query_by_label("Sign in").is_some() && h.query_by_label("Lockout").is_some());
    assert!(h.query_by_label("Linked").is_some(), "REQ-1 has a reference in src/auth.rs");
    assert!(h.query_by_label("Unlinked").is_some(), "REQ-2 has none");
    assert!(h.query_by_label_contains("1/2 tasks").is_some());
    // Spec View sits below Project: its title is lower on the screen than the Project's.
    let project = h.get_by_label("Project").rect();
    let spec = h.get_by_label("Spec View").rect();
    assert!(spec.min.y > project.min.y + 100.0, "Spec View {spec:?} is under Project {project:?}");
}

#[test]
fn clicking_a_requirement_opens_it_and_the_link_toggle_lists_the_code() {
    let (mut h, root) = open_project();
    h.get_by_label("Lockout").click();
    h.run_steps(4);
    let s = &h.state().session;
    let (_, doc) = s.ws.active().unwrap();
    assert_eq!(doc.path.as_deref(), Some(root.join("specs/login/requirements.md").as_path()));
    h.get_by_label("⛓ 1").click();
    h.run_steps(3);
    h.get_by_label("src/auth.rs:1  fn sign_in()").click();
    h.run_steps(4);
    let (_, doc) = h.state().session.ws.active().unwrap();
    assert_eq!(doc.path.as_deref(), Some(root.join("src/auth.rs").as_path()));
}

#[test]
fn a_change_to_the_linked_code_shows_a_chip_and_what_changed() {
    let (mut h, root) = open_project();
    h.state_mut().session.execute("spec.markSynced", &serde_json::json!({ "feature": "login" })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("In sync").is_some());
    assert!(h.query_by_label("Code changed").is_none());

    std::fs::write(root.join("src/auth.rs"), "// spec: REQ-1\nfn sign_in() { changed() }\n").unwrap();
    h.state_mut().session.execute("spec.refresh", &serde_json::Value::Null).unwrap();
    for _ in 0..300 {
        h.run_steps(1);
        if h.query_by_label("Code changed").is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(h.query_by_label("Code changed").is_some());
    assert!(h.query_by_label_contains("1 code changed").is_some(), "the summary line counts it");
    h.get_by_label("⛓ 1").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("changed since the last sync").is_some());
    assert!(h.query_by_label("src/auth.rs:1  fn sign_in()").is_some(), "the link names the item it is attached to");
}

fn wait_for(h: &mut Harness<'static, App>, what: &str, mut done: impl FnMut(&Harness<'static, App>) -> bool) {
    for _ in 0..300 {
        h.run_steps(1);
        if done(h) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn requirements_are_added_and_edited_in_the_view_and_a_refused_edit_says_why() {
    let (mut h, root) = open_project();
    let file = root.join("specs/login/requirements.md");

    // Add: the form opens with the focus in the title; Save is off until something is typed.
    h.get_by_label("＋ Requirement").click();
    h.run_steps(3);
    assert!(h.query_by_label("New requirement").is_some());
    h.event(Event::Text("Reset password".into()));
    h.run_steps(2);
    h.get_by_label("Save").click();
    wait_for(&mut h, "the new requirement", |h| {
        h.state().session.spec.feature("login").is_some_and(|f| f.requirements.len() == 3)
    });
    assert!(std::fs::read_to_string(&file).unwrap().ends_with("## REQ-3: Reset password\n"));
    h.run_steps(3);
    assert!(h.query_by_label("Reset password").is_some(), "the new row");
    assert!(h.query_by_label("New requirement").is_none(), "the form closed");

    // Edit through the context menu: the fields start with the current text.
    h.get_by_label("Lockout").click_secondary();
    h.run_steps(3);
    h.get_by_label("Edit…").click();
    h.run_steps(3);
    assert!(h.query_by_label("Edit requirement").is_some());
    h.event(Event::Text(" out".into()));
    h.run_steps(2);
    h.get_by_label("Save").click();
    wait_for(&mut h, "the edited title", |h| h.query_by_label("Lockout out").is_some());
    assert!(std::fs::read_to_string(&file).unwrap().contains("## REQ-2: Lockout out\nIF five attempts"));
}

#[test]
fn an_edit_that_would_change_the_structure_stays_open_with_the_reason() {
    let (mut h, root) = open_project();
    let file = root.join("specs/login/requirements.md");
    let before = std::fs::read_to_string(&file).unwrap();
    h.get_by_label("Lockout").click_secondary();
    h.run_steps(3);
    h.get_by_label("Edit…").click();
    h.run_steps(3);
    // A heading typed into the text cannot split the requirement in two.
    h.get_all_by_role(egui::accesskit::Role::MultilineTextInput).next().unwrap().click();
    h.run_steps(2);
    h.event(Event::Text("\n## REQ-9: Sneaky".into()));
    h.run_steps(2);
    h.get_by_label("Save").click();
    h.run_steps(4);
    assert!(h.query_by_label("Edit requirement").is_some(), "still open");
    assert!(h.query_by_label_contains("change the structure").is_some(), "and it says why");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before, "nothing was written");
    // Cancel closes it.
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.query_by_label("Edit requirement").is_none());
}

#[test]
fn tasks_under_a_requirement_are_ticked_from_the_view() {
    let (mut h, root) = open_project();
    h.get_by_label("☰ 0/1").click();
    h.run_steps(3);
    h.get_by_label_contains("☐ 2 Lock").click();
    wait_for(&mut h, "the task to be ticked", |h| h.query_by_label_contains("☑ 2 Lock").is_some());
    assert!(std::fs::read_to_string(root.join("specs/login/tasks.md")).unwrap().contains("- [x] 2. Lock"));
}

#[test]
fn unspecified_public_items_are_listed_counted_and_open_on_click() {
    let (mut h, root) = open_project();
    assert!(h.query_by_label("Unspecified code (1)").is_some());
    assert!(h.query_by_label_contains("1 unspecified").is_some(), "the summary line counts it");
    h.get_by_label("Unspecified code (1)").click();
    h.run_steps(3);
    h.get_by_label("src/api.rs:2  fn bare").click();
    h.run_steps(4);
    let (_, doc) = h.state().session.ws.active().unwrap();
    assert_eq!(doc.path.as_deref(), Some(root.join("src/api.rs").as_path()));
}

#[test]
fn an_empty_project_explains_how_to_start_and_new_spec_creates_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::mem::forget(dir);
    std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"d\"\nversion=\"0.1.0\"\n").unwrap();
    let r = root.clone();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(r)));
    h.run_steps(5);
    h.state_mut().session.ws.layout.show(ToolWindowId::Spec);
    for _ in 0..300 {
        h.run_steps(1);
        let s = &h.state().session.spec;
        if s.scanned() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(3);
    assert!(h.query_by_label("No specs yet.").is_some());
    // The same command the button runs.
    h.state_mut().session.execute("spec.new", &serde_json::json!({ "name": "Billing" })).unwrap();
    for _ in 0..300 {
        h.run_steps(1);
        if !h.state().session.spec.features.is_empty() && !h.state().session.spec.busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    h.run_steps(3);
    assert!(h.query_by_label("billing  0/1 tasks").is_some(), "the new feature and its scaffolded task");
    assert!(h.query_by_label("REQ-1").is_some());
    assert!(root.join("specs/billing/requirements.md").is_file());
}

/// A fake `claude` that adds `src/lock.rs`, registered as the Claude backend (with its worktrees
/// in a temporary directory).
#[cfg(unix)]
fn install_fake_agent(h: &mut Harness<'static, App>) {
    use std::os::unix::fs::PermissionsExt;
    let bin = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(bin.path()).unwrap();
    std::mem::forget(bin);
    let fake = dir.join("claude");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '// spec: REQ-2\\npub fn lock() {}\\n' > src/lock.rs\necho '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"Done.\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let settings = serde_json::json!({ "binaries": { "claude": fake }, "worktreesDir": dir.join("worktrees") });
    h.state_mut().session.execute("agent.settings", &settings).unwrap();
}

/// Review mode needs a repository: commit the project as it is.
#[cfg(unix)]
fn git_init(root: &Path) -> bool {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=T", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .is_ok_and(|o| o.status.success())
    };
    git(&["init", "-q", "-b", "main"]) && git(&["add", "-A"]) && git(&["commit", "-q", "-m", "init"])
}

#[cfg(unix)]
#[test]
fn a_requirement_is_handed_to_an_agent_from_its_menu_and_its_chip_opens_the_job() {
    let (mut h, root) = open_project();
    if !git_init(&root) {
        eprintln!("skipping: git not available");
        return;
    }
    install_fake_agent(&mut h);

    h.get_by_label("Lockout").click_secondary();
    h.run_steps(3);
    // Reconcile is for code that moved: off for a requirement nobody changed code under.
    assert!(h.get_by_label("Reconcile with Agent").accesskit_node().is_disabled());
    assert!(!h.get_by_label("Implement with Agent").accesskit_node().is_disabled());
    h.get_by_label("Implement with Agent").click();
    wait_for(&mut h, "the review chip", |h| h.query_by_label("Implement · reviewing").is_some());

    let s = &h.state().session;
    assert_eq!(s.agents.jobs.len(), 1);
    let job = s.agents.jobs[0].id;
    assert!(s.agents.jobs[0].title.starts_with("Implement login/REQ-2: Lockout"), "{}", s.agents.jobs[0].title);
    assert!(s.ws.layout.is_visible(ToolWindowId::Agent), "the Agent window shows the new job");
    assert_eq!(h.state().ui.agent.selected, Some(job), "and has it selected");

    // The chip selects the job again after the user looked elsewhere.
    h.state_mut().ui.agent.selected = None;
    h.get_by_label("Implement · reviewing").click();
    h.run_steps(3);
    assert_eq!(h.state().ui.agent.selected, Some(job));

    // While the job waits for its review, nothing else is started for the requirement.
    h.get_by_label("Lockout").click_secondary();
    h.run_steps(3);
    assert!(h.get_by_label("Implement with Agent").accesskit_node().is_disabled());
    h.key_press(egui::Key::Escape);
    h.run_steps(2);
    assert_eq!(h.state().session.agents.job(job).map(|j| j.status), Some(JobStatus::Reviewing));
}

#[cfg(unix)]
#[test]
fn reconcile_is_offered_once_the_code_moved_and_starts_an_agent_on_the_spec() {
    let (mut h, root) = open_project();
    if !git_init(&root) {
        eprintln!("skipping: git not available");
        return;
    }
    install_fake_agent(&mut h);
    h.state_mut().session.execute("spec.markSynced", &serde_json::json!({ "feature": "login" })).unwrap();
    std::fs::write(root.join("src/auth.rs"), "// spec: REQ-1\nfn sign_in() { changed() }\n").unwrap();
    h.state_mut().session.execute("spec.refresh", &serde_json::Value::Null).unwrap();
    wait_for(&mut h, "the code change", |h| h.query_by_label("Code changed").is_some());

    h.get_by_label("Sign in").click_secondary();
    h.run_steps(3);
    assert!(!h.get_by_label("Reconcile with Agent").accesskit_node().is_disabled());
    h.get_by_label("Reconcile with Agent").click();
    wait_for(&mut h, "the reconcile chip", |h| h.query_by_label_contains("Reconcile · ").is_some());
    let s = &h.state().session;
    assert_eq!(s.agents.jobs.len(), 1);
    assert!(s.agents.jobs[0].title.starts_with("Reconcile login/REQ-1: Sign in"), "{}", s.agents.jobs[0].title);
}

#[cfg(unix)]
#[test]
fn a_refused_start_is_explained_in_the_view() {
    // A fake agent is there, but the project is not a git repository: review mode cannot run.
    let (mut h, _root) = open_project();
    install_fake_agent(&mut h);
    h.get_by_label("Lockout").click_secondary();
    h.run_steps(3);
    h.get_by_label("Implement with Agent").click();
    wait_for(&mut h, "the explanation", |h| h.query_by_label_contains("git repository").is_some());
    assert!(h.state().session.agents.jobs.is_empty());
    assert!(h.query_by_label_contains("Implement · ").is_none(), "no chip for a job that never started");
}

#[cfg(unix)]
#[test]
fn a_task_is_implemented_from_its_own_menu() {
    let (mut h, root) = open_project();
    if !git_init(&root) {
        eprintln!("skipping: git not available");
        return;
    }
    install_fake_agent(&mut h);
    h.get_by_label("☰ 0/1").click();
    h.run_steps(3);
    h.get_by_label_contains("☐ 2 Lock").click_secondary();
    h.run_steps(3);
    h.get_by_label("Implement Task with Agent").click();
    wait_for(&mut h, "the job", |h| !h.state().session.agents.jobs.is_empty());
    let job = h.state().session.agents.jobs[0].id;
    let detail = h.state_mut().session.execute("agent.job", &serde_json::json!({ "job": job.0 })).unwrap();
    let prompt = detail["prompt"].as_str().unwrap();
    assert!(prompt.contains("Implement requirement login/REQ-2"), "{prompt}");
    assert!(prompt.contains("Your task:\n- [ ] 2 Lock"), "{prompt}");
}
