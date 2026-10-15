//! M13 track RC driven headlessly: Edit Configurations' before-launch list, project-file option
//! and templates, the Zig and TypeScript tool windows (fake `zig`/`tsc`), the Modify Selected
//! Code diff popup and the Coverage table's sortable columns.
#![cfg(unix)]
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use serde_json::{Value, json};

use codecraft_engine::run_config_store::BeforeLaunchTask;
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

fn settle(h: &mut Harness<'_, App>, what: &str, mut done: impl FnMut(&App) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        h.run_steps(1);
        if done(h.state()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}: {}", h.state().session.status);
}

fn harness(root: &Path) -> Harness<'static, App> {
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 950.0)).build_eframe({
        let root = root.to_path_buf();
        move |_| App::ephemeral(Some(root))
    });
    h.state_mut().session.isolate_home(&root.join(".home"));
    settle(&mut h, "project", |a| a.session.ws.project.is_some() && !a.session.jobs_running());
    h
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap().join("demo");
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

#[test]
fn edit_configurations_before_launch_project_file_and_templates() {
    let (_dir, root) = project();
    let mut h = harness(&root);
    let s = &mut h.state_mut().session;
    s.execute("runConfig.create", &json!({ "name": "Gen", "kind": "command", "command": "make gen" })).unwrap();
    s.execute(
        "runConfig.create",
        &json!({ "name": "App", "kind": "command", "command": "make app", "beforeLaunch": ["build", "run:Gen"] }),
    )
    .unwrap();
    s.execute("run.editConfigurations", &Value::Null).unwrap();
    s.execute("runConfig.select", &json!({ "index": 1 })).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Before launch").is_some());
    assert!(h.query_by_label("🔨 Build").is_some());
    assert!(h.query_by_label("▶ Run 'Gen'").is_some());
    assert!(h.query_by_label("Debugger").is_some(), "the debugger options section");

    // Remove the Build task (the first ✕): the draft follows.
    h.query_all_by_label("✕").next().unwrap().click();
    h.run_steps(3);
    let draft = &h.state().session.run_store.dialog.as_ref().unwrap().draft;
    assert_eq!(draft[1].before_launch, vec![BeforeLaunchTask::RunConfiguration { name: "Gen".into() }]);

    // Store as project file.
    h.get_by_label("Store as project file").click();
    h.run_steps(3);
    assert!(h.state().session.run_store.dialog.as_ref().unwrap().draft[1].store_as_project_file);

    // Templates: open the section, pick Python, the form edits the template.
    h.get_by_label("Edit configuration templates…").click();
    h.run_steps(3);
    h.get_by_label("🔧 Python").click();
    h.run_steps(3);
    assert!(h.query_by_label_contains("Template: Python").is_some());
    assert_eq!(
        h.state().session.run_store.dialog.as_ref().unwrap().template,
        Some(codecraft_engine::run_config_store::RunKind::Python)
    );
    h.get_by_label("OK").click();
    h.run_steps(3);
    assert!(root.join(".codecraft/runConfigurations/App.json").is_file());
}

#[test]
fn zig_and_typescript_tool_windows() {
    let (dir, root) = project();
    std::fs::write(root.join("build.zig"), "// build\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.ts"), "let x: number = 'a';\n").unwrap();
    std::fs::write(root.join("tsconfig.json"), "{ \"compilerOptions\": { \"strict\": true } }").unwrap();
    let zig = dir.path().join("zig");
    script(
        &zig,
        "if [ \"$1 $2\" = \"build -l\" ]; then printf '  install (default)  Copy\\n  run  Run the app\\n'; else echo \"$*\" > zig-ran.txt; fi\n",
    );
    let tsc = dir.path().join("tsc");
    script(&tsc, "echo \"src/a.ts(1,5): error TS2322: Type 'string' is not assignable to type 'number'.\"\nexit 2\n");
    let mut h = harness(&root);
    h.state_mut().session.zig.program = Some(zig);
    h.state_mut().session.typescript.program = Some(tsc);

    h.state_mut().session.execute("view.toolWindows.zig", &Value::Null).unwrap();
    settle(&mut h, "zig steps", |a| !a.session.zig.loading && !a.session.zig.steps.is_empty());
    h.run_steps(3);
    assert!(h.query_by_label("Zig").is_some(), "the tool window header");
    assert!(h.query_by_label("install (default)").is_some());
    assert!(h.query_by_label("Run the app").is_some());
    h.get_by_label("run").click();
    h.run_steps(2);
    h.get_by_label("▶ Run").click();
    settle(&mut h, "zig build run", |_| root.join("zig-ran.txt").is_file());
    assert_eq!(std::fs::read_to_string(root.join("zig-ran.txt")).unwrap().trim(), "build run");

    h.state_mut().session.execute("view.toolWindows.typeScript", &Value::Null).unwrap();
    h.run_steps(3);
    assert!(h.state().session.ws.layout.is_visible(ToolWindowId::TypeScript));
    assert!(h.query_by_label("strict").is_some(), "the tsconfig options");
    h.get_by_label("▶ Check").click();
    settle(&mut h, "tsc", |a| a.session.typescript.checked && !a.session.typescript.running);
    h.run_steps(3);
    assert!(h.query_all_by_label_contains("1 error in").count() == 1, "the summary");
    let row = "1:5  TS2322  Type 'string' is not assignable to type 'number'.";
    h.get_by_label(row).click();
    h.run_steps(3);
    let (_, d) = h.state().session.ws.active().unwrap();
    assert!(d.path.as_deref().unwrap().ends_with("src/a.ts"));
}

#[test]
fn modify_selected_code_diff_is_accepted_from_the_popup() {
    let (_dir, root) = project();
    std::fs::write(root.join("m.rs"), "fn a() -> i32 {\n    1\n}\n").unwrap();
    let mut h = harness(&root);
    let file = root.join("m.rs");
    h.state_mut().session.execute("file.open", &json!({ "path": file })).unwrap();
    h.run_steps(2);
    // The prompt over the caret's line.
    h.state_mut().session.execute("editor.setSelection", &json!({ "anchor": 16, "head": 16 })).unwrap();
    h.state_mut().session.execute("code.modifySelectedCode", &Value::Null).unwrap();
    h.run_steps(3);
    assert!(h.query_by_label("Modify Selected Code").is_some());
    assert!(h.query_by_label("Send").is_some());
    // The model's answer (the engine fills it from the provider; set here).
    let (editor, doc) = {
        let (e, _) = h.state().session.ws.active().unwrap();
        (e.id, e.doc)
    };
    let p = codecraft_engine::modify_code::proposal(
        editor,
        doc,
        (16, 22),
        "two".into(),
        "    1\n".into(),
        "    2\n".into(),
    );
    h.state_mut().session.modify.prompt = None;
    h.state_mut().session.modify.proposal = Some(p);
    h.run_steps(3);
    assert!(h.query_by_label("Modified code").is_some());
    assert!(h.query_by_label("-     1").is_some() && h.query_by_label("+     2").is_some(), "the diff lines");
    h.get_by_label("Accept").click();
    h.run_steps(3);
    let (_, d) = h.state().session.ws.active().unwrap();
    assert_eq!(d.doc.text(), "fn a() -> i32 {\n    2\n}\n");
    assert!(h.state().session.modify.proposal.is_none());
    assert!(h.query_by_label("Modified code").is_none());
}

#[test]
fn coverage_columns_sort_the_table() {
    let (_dir, root) = project();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "a\nb\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "a\nb\n").unwrap();
    std::fs::write(
        root.join("cov.lcov"),
        "SF:src/a.rs\nDA:1,1\nDA:2,0\nend_of_record\nSF:src/b.rs\nDA:1,1\nDA:2,1\nend_of_record\n",
    )
    .unwrap();
    let mut h = harness(&root);
    h.state_mut().session.execute("coverage.importLcov", &json!({ "path": root.join("cov.lcov") })).unwrap();
    h.run_steps(3);
    h.get_by_label("Line, %").click();
    h.run_steps(3);
    let sort = h.state().session.testing.coverage_sort;
    assert_eq!(sort.column, codecraft_engine::testing::CoverageColumn::Line);
    assert!(!sort.descending);
    assert!(h.query_by_label("Line, % ▲").is_some());
    h.get_by_label("Line, % ▲").click();
    h.run_steps(3);
    assert!(h.state().session.testing.coverage_sort.descending);
    let summary = h.state_mut().session.execute("coverage.summary", &Value::Null).unwrap();
    let names: Vec<&str> = summary["rows"].as_array().unwrap().iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(names, ["src", "b.rs", "a.rs"], "100% before 50% descending");
}

#[test]
fn right_click_on_a_gutter_run_icon_offers_run_debug_and_the_whole_file() {
    let (_dir, root) = project();
    std::fs::write(root.join("main.py"), "def test_one():\n    assert True\n").unwrap();
    std::fs::rename(root.join("main.py"), root.join("test_main.py")).unwrap();
    let mut h = harness(&root);
    h.state_mut().session.execute("file.open", &json!({ "path": root.join("test_main.py") })).unwrap();
    h.run_steps(3);
    let rect = h.state().ui.editor_rect.unwrap();
    let line_h = h.state().ui.editor_line_h;
    let at = egui::pos2(rect.left() + 9.0, rect.top() + line_h * 0.5);
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Secondary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    h.event(egui::Event::PointerMoved(at));
    h.run_steps(1);
    h.event(button(true));
    h.run_steps(1);
    h.event(button(false));
    h.run_steps(3);
    for label in ["▶  Run", "🐞  Debug", "Run All Tests in File", "Debug All Tests in File"] {
        assert!(h.query_by_label(label).is_some(), "the menu offers {label}");
    }
    h.key_press(egui::Key::Escape);
    h.run_steps(3);
    assert!(h.query_by_label("Run All Tests in File").is_none(), "Escape closes the menu");
}
