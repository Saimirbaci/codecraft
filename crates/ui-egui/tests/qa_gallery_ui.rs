//! M14 QA: the main window, every tool window and the main dialogs with realistic content (a
//! Rust + Python + SQL project, a stopped debugger, inspection results, a profiler snapshot, a
//! database grid, an agent's approval card, settings pages), painted at 1280×800 and 900×700 in
//! the dark and light themes. Set `CODECRAFT_QA_RENDER_DIR` to write PNGs for a visual review;
//! without it the states are still built and painted (a smoke test that nothing panics).
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use serde_json::{Value, json};

use codecraft_engine::agent_tools::{self, Caller};
use codecraft_ui_egui::App;
use codecraft_workspace::ToolWindowId;

const SIZES: [(f32, f32); 2] = [(1280.0, 800.0), (900.0, 700.0)];
const THEMES: [&str; 2] = ["Dark", "Light"];

fn render_dir() -> Option<PathBuf> {
    std::env::var_os("CODECRAFT_QA_RENDER_DIR").map(PathBuf::from)
}

fn settle(h: &mut Harness<'_, App>, what: &str, secs: u64, mut done: impl FnMut(&App) -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) {
        h.run_steps(1);
        if done(h.state()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("gallery: gave up waiting for {what}: {}", h.state().session.status);
    false
}

/// Paint the current state in every size and theme (PNGs only with `CODECRAFT_QA_RENDER_DIR`).
fn shoot(h: &mut Harness<'_, App>, name: &str) {
    let dir = render_dir();
    for (w, ht) in SIZES {
        for theme in THEMES {
            h.state_mut().session.ws.settings.theme = theme.to_string();
            h.set_size(egui::vec2(w, ht));
            h.run_steps(4);
            if let Some(dir) = &dir {
                let _ = std::fs::create_dir_all(dir);
                let file = dir.join(format!("{name}-{}x{}-{}.png", w as u32, ht as u32, theme.to_lowercase()));
                h.render().unwrap().save(file).unwrap();
            }
        }
    }
    h.state_mut().session.ws.settings.theme = "Dark".into();
    h.set_size(egui::vec2(SIZES[0].0, SIZES[0].1));
    h.run_steps(2);
}

fn has(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd).args(args).output().is_ok_and(|o| o.status.success())
}

const LIB: &str = "use std::collections::HashMap;

/// Sum of the prices with 10% off for 3 items or more.
pub fn total(prices: &[u32]) -> u32 {
    let mut sum = 0;
    for i in 1..prices.len() {
        sum += prices[i];
    }
    if prices.len() >= 3 { sum - sum / 10 } else { sum }
}

pub fn stock() -> HashMap<String, u32> {
    let mut m = HashMap::new();
    m.insert(\"apple\".to_string(), 3);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_of_two() {
        assert_eq!(total(&[10, 20]), 30);
    }
}
";

const APP_PY: &str = "import os


def work(items):
    total = 0
    for i, price in enumerate(items):
        total += price * (i + 1)
    return total


if __name__ == \"__main__\":
    print(work([3, 5, 7]))
";

fn project(base: &Path) -> PathBuf {
    let root = base.join("shop");
    for (rel, text) in [
        ("Cargo.toml", "[package]\nname = \"shop\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n"),
        ("src/lib.rs", LIB),
        ("src/main.rs", "fn main() {\n    println!(\"{}\", shop::total(&[10, 20, 30]));\n}\n"),
        ("app.py", APP_PY),
        (
            "tests/test_app.py",
            "from app import work\n\n\ndef test_work():\n    assert work([1, 2]) == 5\n\n\ndef test_bad():\n    assert work([1]) == 2\n",
        ),
        ("db/cleanup.sql", "SELECT name FROM items;\nDELETE FROM items;\nUPDATE items SET price = 0;\n"),
        (
            "profile.txt",
            "main (app.py:12);work (app.py:6) 60\nmain (app.py:12);work (app.py:7) 25\nmain (app.py:12);print 10\nidle 5\n",
        ),
    ] {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(root.join("shop.db")),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    for sql in [
        "create table items (id integer primary key, name text not null, price integer, note text)",
        "insert into items(name, price, note) values ('apple', 3, 'red'), ('pear', 5, null), ('plum', 7, 'a rather long note that needs room in the grid')",
    ] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
    root.canonicalize().unwrap()
}

fn harness(root: &Path, home: &Path) -> Harness<'static, App> {
    let (root, home) = (root.to_path_buf(), home.to_path_buf());
    Harness::builder().with_size(egui::vec2(SIZES[0].0, SIZES[0].1)).build_eframe(move |_| {
        let mut app = App::ephemeral(Some(root));
        app.session.isolate_home(&home);
        app.session.db.set_secret_store(std::sync::Arc::new(codecraft_engine::db::MemoryStore::default()));
        app.session.ws.settings.keymap = "macOS".into();
        app
    })
}

fn exec(h: &mut Harness<'_, App>, id: &str, params: Value) -> Option<Value> {
    match h.state_mut().session.execute(id, &params) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("gallery: {id} failed: {e}");
            None
        }
    }
}

fn only(h: &mut Harness<'_, App>, tw: ToolWindowId) {
    for other in ToolWindowId::ALL {
        h.state_mut().session.ws.layout.hide(other);
    }
    h.state_mut().session.ws.layout.show(tw);
    h.run_steps(2);
}

#[test]
fn gallery() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = project(&base);
    let mut h = harness(&root, &base.join("home"));
    settle(&mut h, "the project", 60, |a| a.session.ws.project.as_ref().is_some_and(|p| !p.tree.is_empty()));
    exec(&mut h, "file.open", json!({ "path": root.join("src/lib.rs") }));
    only(&mut h, ToolWindowId::Project);
    shoot(&mut h, "01-main-editor");

    // Every tool window, empty or with what a fresh project has.
    for tw in ToolWindowId::ALL {
        if matches!(
            tw,
            ToolWindowId::Terminal
                | ToolWindowId::PythonConsole
                | ToolWindowId::RustRepl
                | ToolWindowId::PythonPackages
        ) {
            // They start processes (Python Packages asks PyPI for outdated packages); covered by
            // their own tests.
            continue;
        }
        only(&mut h, tw);
        shoot(&mut h, &format!("02-tw-{tw:?}"));
    }

    // Inspection results and problems: CodeCraft's own SQL inspections (no external tool).
    exec(&mut h, "code.inspectCode", json!({ "scope": "project", "tools": ["sql"] }));
    settle(&mut h, "the inspection", 60, |a| !a.session.inspections.busy() && a.session.inspections.results.is_some());
    if let Some(id) = h.state().session.inspections.results.as_ref().and_then(|r| r.findings.first()).map(|f| f.id) {
        exec(&mut h, "inspections.select", json!({ "id": id }));
    }
    only(&mut h, ToolWindowId::InspectionResults);
    shoot(&mut h, "03-inspection-results");
    only(&mut h, ToolWindowId::Problems);
    shoot(&mut h, "03-problems");

    // A profiler snapshot: flame graph and method list.
    exec(&mut h, "run.openProfilerSnapshot", json!({ "path": root.join("profile.txt") }));
    settle(&mut h, "the snapshot", 60, |a| {
        a.session.profiler.snapshot.is_some() && a.session.profiler.loading.is_none()
    });
    only(&mut h, ToolWindowId::Profiler);
    shoot(&mut h, "04-profiler-flame");
    exec(&mut h, "profiler.selectTab", json!({ "tab": "methodList" }));
    shoot(&mut h, "04-profiler-methods");
    exec(&mut h, "profiler.selectTab", json!({ "tab": "callTree" }));
    shoot(&mut h, "04-profiler-calltree");

    // A database with a query result grid.
    if let Some(v) = exec(&mut h, "db.addDataSource", json!({ "kind": "sqlite", "name": "Shop", "path": "shop.db" })) {
        let id = v["dataSource"].as_str().unwrap_or("shop").to_string();
        exec(&mut h, "db.connect", json!({ "dataSource": id }));
        settle(&mut h, "the connection", 30, |a| !a.session.db.busy());
        exec(&mut h, "db.newConsole", json!({ "dataSource": id }));
        exec(&mut h, "db.execute", json!({ "dataSource": id, "sql": "select * from items order by id" }));
        settle(&mut h, "the query", 30, |a| !a.session.db.busy() && !a.session.db.results.is_empty());
        only(&mut h, ToolWindowId::Database);
        shoot(&mut h, "05-database-grid");
        only(&mut h, ToolWindowId::Services);
        shoot(&mut h, "05-services");
    }

    // An agent asks to write a file: the approval card.
    h.state_mut().session.agent_access = codecraft_engine::agent_access::AgentAccess::default();
    agent_tools::submit(
        &mut h.state_mut().session,
        Caller::new("Claude Code"),
        "file_write",
        json!({ "path": "notes.md", "content": "# Notes\n" }),
        Box::new(|_| {}),
    );
    settle(&mut h, "the approval card", 10, |a| !a.session.approvals.is_empty());
    only(&mut h, ToolWindowId::Project);
    shoot(&mut h, "06-agent-approval");
    let pending = h.state().session.approvals.pending().next().map(|r| r.id);
    if let Some(id) = pending {
        h.state_mut().session.approvals.decide(id, codecraft_engine::approvals::Decision::Deny);
    }
    h.run_steps(3);

    // Dialogs.
    for (name, id, params) in [
        ("07-dialog-inspect-code", "code.inspectCode", json!({})),
        ("07-dialog-edit-configurations", "run.editConfigurations", json!({})),
        ("07-dialog-breakpoints", "run.viewBreakpoints", json!({})),
        ("07-dialog-search-everywhere", "navigate.searchEverywhere", json!({})),
        ("07-dialog-find-in-files", "edit.find.findInFiles", json!({})),
        ("07-dialog-find-action", "help.findAction", json!({})),
    ] {
        if exec(&mut h, id, params).is_some() {
            h.run_steps(3);
            shoot(&mut h, name);
        }
        for _ in 0..3 {
            h.key_press(egui::Key::Escape);
            h.run_steps(2);
        }
        h.state_mut().session.popup = None;
        h.state_mut().session.inspections.dialog = None;
        h.run_steps(3);
    }

    // Settings pages that matter for M14.
    for page in [
        codecraft_engine::settings::PAGE_AGENT_ACCESS,
        codecraft_engine::settings::PAGE_INSPECTIONS,
        codecraft_engine::settings::PAGE_PYTHON,
        codecraft_engine::settings::PAGE_RUST,
        codecraft_engine::settings::PAGE_APPEARANCE,
        codecraft_engine::settings::PAGE_KEYMAP,
    ] {
        exec(&mut h, "app.settings", json!({ "page": page }));
        h.run_steps(3);
        shoot(&mut h, &format!("08-settings-{}", page.replace([' ', '›', '&', '/'], "")));
    }
    let _ = h.state_mut().session.execute("settings.dialog.cancel", &Value::Null);
    h.run_steps(3);

    // A stopped Python debug session (debugpy).
    if has("python3", &["-c", "import debugpy"]) {
        exec(&mut h, "debug.breakpoint.set", json!({ "path": root.join("app.py"), "line": 7 }));
        exec(&mut h, "debug.atCaret", json!({ "path": root.join("app.py") }));
        if settle(&mut h, "the debugger to stop", 90, |a| a.session.debug.is_stopped() && !a.session.debug.settling()) {
            only(&mut h, ToolWindowId::Debug);
            shoot(&mut h, "09-debug-stopped");
        }
        let _ = h.state_mut().session.execute("debug.stop", &Value::Null);
        h.run_steps(5);
    }

    // Tests: a pytest run with a failure in the Run tool window.
    if has("python3", &["-c", "import pytest"]) {
        exec(&mut h, "run.testsInFile", json!({ "path": root.join("tests/test_app.py") }));
        settle(&mut h, "the tests", 120, |a| {
            !a.session.jobs_running()
                && a.session.ws.runs.iter().any(|r| r.status != codecraft_workspace::RunStatus::Running)
        });
        only(&mut h, ToolWindowId::Run);
        shoot(&mut h, "10-tests-run");
    }
    h.state_mut().session.shutdown();
}
