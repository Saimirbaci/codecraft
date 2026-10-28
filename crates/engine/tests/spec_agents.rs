//! Agents and the Spec View: `spec.list` waits for its scan over `codecraft_execute`, the
//! commands carry the right capabilities, and a spec edit made through the file tools shows up.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_engine::agent_access::{AgentAccess, Capability};
use codecraft_engine::agent_tools::{self, Caller, command_capability, result_json, result_text};
use codecraft_engine::mcp::ToolResult;

const LONG: Duration = Duration::from_secs(120);

const REQS: &str = "# Requirements: Login\n\n## REQ-1: Sign in\nWHEN valid credentials are given THE SYSTEM SHALL open a session.\n\n## REQ-2: Lockout\nIF five attempts fail THEN THE SYSTEM SHALL lock the account.\n";

fn project(root: &Path) {
    std::fs::create_dir_all(root.join("specs/login")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("specs/login/requirements.md"), REQS).unwrap();
    std::fs::write(root.join("src/auth.rs"), "// spec: REQ-1\nfn sign_in() {}\n// spec: REQ-9\n").unwrap();
}

fn session(home: &Path, root: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(home);
    s.execute("file.open", &json!({ "path": root })).unwrap();
    let start = Instant::now();
    while s.jobs_running() {
        assert!(start.elapsed() < LONG, "timed out waiting for the project scan");
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    s
}

fn execute(s: &mut Session, id: &str, params: Value) -> ToolResult {
    agent_tools::call_blocking(
        s,
        Caller::new("test agent"),
        "codecraft_execute",
        json!({ "id": id, "params": params }),
        LONG,
    )
}

#[test]
fn spec_commands_have_the_right_capabilities() {
    for id in ["spec.list", "spec.show", "spec.open", "spec.openLink", "spec.refresh"] {
        assert_eq!(command_capability(id), Some(Capability::Read), "{id}");
    }
    for id in ["spec.new", "spec.markSynced"] {
        assert_eq!(command_capability(id), Some(Capability::Write), "{id}");
    }
}

#[test]
fn spec_list_waits_for_its_scan_when_asked_to() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("proj");
    project(&root);
    let mut s = session(&tmp.path().join("home"), &root);

    // Without waiting: the scan has just started, so nothing is there yet.
    let first = s.execute("spec.list", &json!({})).unwrap();
    assert_eq!(first["pending"], json!(true), "{first:#}");
    assert_eq!(first["features"], json!([]));

    // Over MCP with waitMs: the answer is the finished scan.
    let r = execute(&mut s, "spec.list", json!({ "waitMs": 30_000 }));
    assert!(!r.is_error, "{}", result_text(&r));
    let v = result_json(&r)["result"].clone();
    assert_eq!(v["pending"], json!(false), "{v:#}");
    assert!(v.get("timedOut").is_none(), "{v:#}");
    assert_eq!(v["features"][0]["name"], "login");
    assert_eq!(v["features"][0]["requirements"][0]["status"], "linked");
    assert_eq!(v["features"][0]["requirements"][1]["status"], "unlinked");
    assert_eq!(v["danglingLinks"][0]["reference"], "REQ-9");
    assert_eq!(v["counts"], json!({ "linked": 1, "unlinked": 1 }));
    s.shutdown();
}

#[test]
fn an_agents_edit_of_a_requirement_shows_as_a_spec_change() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("proj");
    project(&root);
    let mut s = session(&tmp.path().join("home"), &root);
    // Nobody is there to approve a card: allow what the agent does here.
    s.agent_access = AgentAccess::headless(&[Capability::Write]);
    execute(&mut s, "spec.list", json!({ "waitMs": 30_000 }));
    let r = execute(&mut s, "spec.markSynced", json!({ "feature": "login" }));
    assert!(!r.is_error, "{}", result_text(&r));

    // The agent edits the requirement with the ordinary file tool; the watcher is not running in
    // this test, so tell the engine what the watcher would.
    let edit = agent_tools::call_blocking(
        &mut s,
        Caller::new("test agent"),
        "file_edit",
        json!({ "path": "specs/login/requirements.md", "oldText": "open a session", "newText": "open a session within 2 s" }),
        LONG,
    );
    assert!(!edit.is_error, "{}", result_text(&edit));
    s.execute("spec.refresh", &json!({})).unwrap();
    let r = execute(&mut s, "spec.list", json!({ "waitMs": 30_000 }));
    let v = result_json(&r)["result"].clone();
    assert_eq!(v["features"][0]["requirements"][0]["status"], "specChanged", "{v:#}");
    let shown =
        result_json(&execute(&mut s, "spec.show", json!({ "feature": "login", "id": "REQ-1" })))["result"].clone();
    assert!(shown["text"].as_str().unwrap().contains("within 2 s"));
    assert_ne!(shown["hash"], shown["syncedHash"]);
    s.shutdown();
}

#[test]
fn an_agents_edit_of_linked_code_shows_as_code_changed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap().join("proj");
    project(&root);
    let mut s = session(&tmp.path().join("home"), &root);
    s.agent_access = AgentAccess::headless(&[Capability::Write]);
    execute(&mut s, "spec.list", json!({ "waitMs": 30_000 }));
    let r = execute(&mut s, "spec.markSynced", json!({ "feature": "login" }));
    assert!(!r.is_error, "{}", result_text(&r));
    let v = result_json(&execute(&mut s, "spec.list", json!({})))["result"].clone();
    assert_eq!(v["features"][0]["requirements"][0]["status"], "inSync", "{v:#}");

    let edit = agent_tools::call_blocking(
        &mut s,
        Caller::new("test agent"),
        "file_edit",
        json!({ "path": "src/auth.rs", "oldText": "fn sign_in() {}", "newText": "fn sign_in() { audit(); }" }),
        LONG,
    );
    assert!(!edit.is_error, "{}", result_text(&edit));
    s.execute("spec.refresh", &json!({})).unwrap();
    let v = result_json(&execute(&mut s, "spec.list", json!({ "waitMs": 30_000 })))["result"].clone();
    assert_eq!(v["features"][0]["requirements"][0]["status"], "codeChanged", "{v:#}");
    let shown =
        result_json(&execute(&mut s, "spec.show", json!({ "feature": "login", "id": "REQ-1" })))["result"].clone();
    assert_eq!(shown["codeChanges"][0]["change"], "modified");
    assert_eq!(shown["codeChanges"][0]["symbol"], "fn sign_in()");
    s.shutdown();
}
