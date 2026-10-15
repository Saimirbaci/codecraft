//! Code analysis views (M14 track CA) driven headlessly: the Inspection Results tool window
//! grouped by severity / analyzer / file with the source preview and the baseline's "Only new",
//! the custom scope in the scope dialog, Settings › Editor › Inspections (search, a rule switched
//! off from its checkbox), the Problems tool window's Project tab and the commit check's warning.
//! Only CodeCraft's built-in SQL inspections run, so no external tool is needed.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::analysis::commit_check::CommitWarning;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(60) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

fn snapshot(h: &mut Harness<'_, App>, name: &str) {
    let Some(dir) = std::env::var_os("CODECRAFT_SNAPSHOT_DIR") else { return };
    h.render().unwrap().save(PathBuf::from(dir).join(format!("{name}.png"))).unwrap();
}

fn harness(root: &Path, home: &Path) -> Harness<'static, App> {
    let (root, home) = (root.to_path_buf(), home.to_path_buf());
    Harness::builder().with_size(egui::vec2(1400.0, 1000.0)).build_eframe(move |_| {
        let mut app = App::ephemeral(Some(root));
        app.session.isolate_home(&home);
        app
    })
}

fn run_analysis(h: &mut Harness<'_, App>, params: Value) {
    h.state_mut().session.execute("code.inspectCode", &params).unwrap();
    settle(h, "analysis", |a| !a.session.inspections.busy());
    h.run_steps(3);
}

#[test]
fn results_grouping_preview_baseline_and_custom_scope() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(root.join("db")).unwrap();
    std::fs::write(root.join("db/a.sql"), "SELECT 1;\nDELETE FROM users;\n").unwrap();
    std::fs::write(root.join("db/b.sql"), "UPDATE users SET x = 1;\n").unwrap();
    std::fs::write(root.join("other.sql"), "DELETE FROM t;\n").unwrap();
    let mut h = harness(&root, &base.join("home"));
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    h.state_mut().session.ws.layout.remember_size(ToolWindowId::InspectionResults, 560.0);

    // The scope dialog's custom scope and profile.
    h.state_mut().session.execute("code.inspectCode", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Custom scope").is_some());
    assert!(h.query_by_label("Inspection profile:").is_some());
    h.get_by_label("Scope pattern").click();
    h.run_steps(1);
    h.event(egui::Event::Text("db/*.sql".into()));
    h.run_steps(2);
    assert_eq!(h.state().session.inspections.dialog.as_ref().unwrap().pattern, "db/*.sql");
    h.get_by_label("Analyze").click();
    settle(&mut h, "analysis", |a| !a.session.inspections.busy() && a.session.inspections.results.is_some());
    h.run_steps(3);
    let r = h.state().session.inspections.results.clone().unwrap();
    assert_eq!(r.findings.len(), 2, "only db/*.sql: {}", r.scope_label);
    assert!(h.query_all_by_label_contains("SqlWithoutWhere").count() > 0);
    assert!(h.query_all_by_label_contains("Profile: Default").count() > 0);

    // Group by severity, analyzer, file.
    h.state_mut().session.execute("inspections.groupBy", &json!({ "by": "analyzer" })).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("SQL").count() > 0);
    h.state_mut().session.execute("inspections.groupBy", &json!({ "by": "file" })).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("a.sql").count() > 0 && h.query_all_by_label_contains("b.sql").count() > 0);
    h.state_mut().session.execute("inspections.groupBy", &json!({ "by": "severity" })).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("Warning").count() > 0);

    // Select a problem: the description shows the source around it.
    let id = r.findings.iter().find(|f| f.path.ends_with("db/a.sql")).unwrap().id;
    h.state_mut().session.execute("inspections.select", &json!({ "id": id })).unwrap();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("2  DELETE FROM users;").count() > 0, "the preview line");
    snapshot(&mut h, "inspect-ca-results");

    // Baseline, then only new problems.
    h.state_mut().session.execute("analysis.baseline.save", &json!({})).unwrap();
    std::fs::write(root.join("db/b.sql"), "UPDATE users SET x = 1;\nDELETE FROM more;\n").unwrap();
    run_analysis(&mut h, json!({ "scope": "glob", "pattern": "db/*.sql" }));
    let r = h.state().session.inspections.results.clone().unwrap();
    assert!(r.baseline.is_some());
    let new = r.findings.iter().filter(|f| !f.baseline).count();
    assert_eq!(new, 1, "{:?}", r.findings.iter().map(|f| (&f.path, f.baseline)).collect::<Vec<_>>());
    h.get_by_label("Only new").click();
    h.run_steps(3);
    assert!(h.state().session.inspections.only_new);
    assert_eq!(codecraft_engine::inspections::grouped(&h.state().session.inspections).iter().map(|g| g.children.iter().map(|c| c.ids.len()).sum::<usize>()).sum::<usize>(), 1);
}

#[test]
fn inspections_settings_page_problems_project_tab_and_commit_warning() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("q.sql"), "SELECT 1;\nDELETE FROM users;\n").unwrap();
    let mut h = harness(&root, &base.join("home"));
    settle(&mut h, "project", |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));

    // Settings › Editor › Inspections: search, then switch a rule off from its checkbox.
    h.state_mut().session.execute("settings.inspections", &json!({})).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Search inspections").is_some());
    h.get_by_label("Search inspections").click();
    h.run_steps(1);
    h.event(egui::Event::Text("WithoutWhere".into()));
    h.run_steps(3);
    assert!(h.query_by_label("Analyzer SQL").is_some(), "the SQL analyzer matches the search");
    assert!(h.query_by_label("Analyzer Clippy").is_none(), "Rust analyzers are filtered out");
    h.get_by_label("Analyzer SQL").click();
    h.run_steps(2);
    let p = codecraft_engine::analysis::profiles::resolve(&mut h.state_mut().session, None).unwrap();
    assert!(!p.analyzer_enabled(codecraft_engine::inspections::Tool::Sql), "the analyzer checkbox turned SQL off");
    h.state_mut().session.execute("analysis.profile.setAnalyzer", &json!({ "analyzer": "sql", "enabled": true })).unwrap();
    h.state_mut().session.execute("analysis.profile.setRule", &json!({ "analyzer": "sql", "rule": "SqlWithoutWhere", "severity": "error" })).unwrap();
    h.run_steps(3);
    snapshot(&mut h, "inspect-ca-settings");
    assert!(root.join(".codecraft/inspection-profiles.json").is_file());
    h.state_mut().session.execute("settings.dialog.cancel", &Value::Null).ok();
    h.run_steps(3);

    // The Problems tool window's Project tab lists the analysis.
    run_analysis(&mut h, json!({ "scope": "project" }));
    h.state_mut().session.ws.layout.show(ToolWindowId::Problems);
    h.run_steps(3);
    let project_tab = h.query_all_by_label_contains("Project  1").count();
    assert!(project_tab > 0, "the Project tab with its count");
    h.get_by_label("Project  1").click();
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("q.sql   1 problem(s)").count() > 0, "the file group");
    h.get_by_label("Unsafe query: 'DELETE' statement without 'WHERE' clears all data in the table :2:1").click();
    h.run_steps(3);
    let (e, d) = h.state().session.ws.active().unwrap();
    assert!(d.path.as_deref().unwrap().ends_with("q.sql"));
    assert_eq!(d.doc.char_to_line(e.selections.primary().head), 1);

    // The commit check's warning: Review / Commit Anyway / Cancel.
    h.state_mut().session.analysis.commit.warning =
        Some(CommitWarning { job: 1, errors: 1, warnings: 2, files: 1, params: json!({ "message": "x" }) });
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("found 1 error(s) and 2 warning(s) in 1 file(s)").count() > 0);
    assert!(h.query_by_label("Commit Anyway").is_some());
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(h.state().session.analysis.commit.warning.is_none());
}
