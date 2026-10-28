//! The Spec View: docked under Project, lists features and requirements with their status, and
//! every click is a `spec.*` command.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

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
    let r = root.clone();
    let mut h = Harness::builder().with_size(egui::vec2(1280.0, 800.0)).build_eframe(move |_| App::ephemeral(Some(r)));
    h.run_steps(5);
    h.state_mut().session.ws.layout.show(ToolWindowId::Spec);
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
    h.get_by_label("src/auth.rs:1").click();
    h.run_steps(4);
    let (_, doc) = h.state().session.ws.active().unwrap();
    assert_eq!(doc.path.as_deref(), Some(root.join("src/auth.rs").as_path()));
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
