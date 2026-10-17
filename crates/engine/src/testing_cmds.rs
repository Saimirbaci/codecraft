//! Test runner commands (track T): the test tree (select, navigate, filters, failed-test
//! stepping), rerun (failed), test history, import/export, and coverage (Run with Coverage, the
//! Coverage tool window's reports, gutter marks).

use serde_json::{Value, json};

use codecraft_cargo::{CargoCommand, CargoInvocation};
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, has_project, ok, simple, spec};
use crate::params::{opt_path, opt_str, opt_usize, req_path, req_str, req_usize};
use crate::testing;
use crate::{CommandError, RunConfig, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------------------ catalog (Run menu)
        spec(
            "run.runWithCoverage",
            "Run with Coverage",
            None,
            "{config?: int, path?: string, line?: int} — the selected (or given) run configuration, or the test at path/line, under cargo llvm-cov / pytest-cov",
            Context::Global,
            has_project,
            run_with_coverage,
        ),
        spec(
            "run.testHistory",
            "Show Test History…",
            None,
            "Open the test history (last 20 runs of the project); returns the list",
            Context::Global,
            has_project,
            test_history,
        ),
        spec(
            "run.importTestsFromFile",
            "Import Tests from File…",
            None,
            "{path?: string} — a JUnit XML report or an exported run (JSON); without a path the IDE asks for one",
            Context::Global,
            has_project,
            import_tests,
        ),
        spec(
            "run.manageCoverageReports",
            "Manage Coverage Reports…",
            None,
            "Open the stored coverage reports; returns the list",
            Context::Global,
            has_project,
            manage_reports,
        ),
        // ------------------------------------------------------------ test runner
        spec(
            "test.tree",
            "Test Tree",
            None,
            "{tab?: int} — the test run as JSON (rows, suites, counts)",
            Context::Global,
            has_test_run,
            |s, p| {
                let tab = tab_param(s, p)?;
                Ok(testing::run_json(s, tab))
            },
        ),
        spec("test.rerun", "Rerun Tests", None, "{tab?: int}", Context::Global, has_test_run, rerun),
        spec(
            "test.rerunFailed",
            "Rerun Failed Tests",
            None,
            "{tab?: int}",
            Context::Global,
            has_test_run,
            rerun_failed,
        ),
        spec(
            "test.select",
            "Select Test Tree Node",
            None,
            "{key: string, tab?: int} — keys come from test.tree rows",
            Context::Global,
            has_test_run,
            |s, p| {
                let tab = tab_param(s, p)?;
                testing::select(s, tab, req_str(p, "key")?)?;
                ok()
            },
        ),
        spec(
            "test.navigate",
            "Jump to Test Source",
            None,
            "{key?: string, tab?: int} — open the selected (or given) test's definition",
            Context::Global,
            has_test_run,
            navigate,
        ),
        spec(
            "test.toggleNode",
            "Expand/Collapse Test Tree Node",
            None,
            "{key: string, tab?: int, expanded?: bool}",
            Context::Global,
            has_test_run,
            toggle_node,
        ),
        spec("test.expandAll", "Expand All Tests", None, "{tab?: int}", Context::Global, has_test_run, |s, p| {
            let tab = tab_param(s, p)?;
            testing::set_all_collapsed(s, tab, false)?;
            ok()
        }),
        spec("test.collapseAll", "Collapse All Tests", None, "{tab?: int}", Context::Global, has_test_run, |s, p| {
            let tab = tab_param(s, p)?;
            testing::set_all_collapsed(s, tab, true)?;
            ok()
        }),
        spec(
            "test.toggleShowPassed",
            "Show Passed",
            None,
            "{value?: bool}",
            Context::Global,
            crate::commands::always,
            |s, p| {
                let v = toggle(p, s.testing.options.show_passed);
                s.testing.options.show_passed = v;
                Ok(json!(v))
            },
        ),
        spec(
            "test.toggleShowIgnored",
            "Show Ignored",
            None,
            "{value?: bool}",
            Context::Global,
            crate::commands::always,
            |s, p| {
                let v = toggle(p, s.testing.options.show_ignored);
                s.testing.options.show_ignored = v;
                Ok(json!(v))
            },
        ),
        spec(
            "test.toggleSortByDuration",
            "Sort by Duration",
            None,
            "{value?: bool}",
            Context::Global,
            crate::commands::always,
            |s, p| {
                let v = toggle(p, s.testing.options.sort_by_duration);
                s.testing.options.sort_by_duration = v;
                Ok(json!(v))
            },
        ),
        spec("test.nextFailed", "Next Failed Test", None, "{tab?: int}", Context::Global, has_test_run, |s, p| {
            let tab = tab_param(s, p)?;
            Ok(json!(testing::step_failed(s, tab, true)?))
        }),
        spec(
            "test.previousFailed",
            "Previous Failed Test",
            None,
            "{tab?: int}",
            Context::Global,
            has_test_run,
            |s, p| {
                let tab = tab_param(s, p)?;
                Ok(json!(testing::step_failed(s, tab, false)?))
            },
        ),
        spec(
            "test.openHistory",
            "Open Test History Entry",
            None,
            "{index: int} — index from run.testHistory; shows the stored run in a Run tab",
            Context::Global,
            has_project,
            open_history,
        ),
        simple("test.clearHistory", "Clear Test History", has_project, |s, _| {
            let root = project_root(s)?;
            testing::clear_history(&root).map_err(CommandError::Failed)?;
            ok()
        }),
        spec(
            "test.export",
            "Export Test Results",
            None,
            "{path: string, tab?: int} — write the run as JSON (importable)",
            Context::Global,
            has_test_run,
            export,
        ),
        // ------------------------------------------------------------ coverage
        spec(
            "coverage.openReport",
            "Open Coverage Report",
            None,
            "{id: string} — id from run.manageCoverageReports",
            Context::Global,
            has_project,
            |s, p| {
                let root = project_root(s)?;
                let report = testing::open_report(&root, req_str(p, "id")?)?;
                let summary = coverage_json(&report, &root, s.testing.coverage_sort);
                s.testing.coverage = Some(report);
                s.ws.layout.show(ToolWindowId::Coverage);
                Ok(summary)
            },
        ),
        spec(
            "coverage.deleteReport",
            "Delete Coverage Report",
            None,
            "{id: string}",
            Context::Global,
            has_project,
            |s, p| {
                let root = project_root(s)?;
                let id = req_str(p, "id")?.to_string();
                testing::delete_report(&root, &id)?;
                if s.testing.coverage.as_ref().is_some_and(|c| c.id == id) {
                    s.testing.coverage = None;
                }
                ok()
            },
        ),
        simple("coverage.closeReport", "Hide Coverage Data", crate::commands::always, |s, _| {
            s.testing.coverage = None;
            ok()
        }),
        spec(
            "coverage.toggleEditorMarks",
            "Show Coverage in Editor",
            None,
            "{value?: bool} — green/red gutter marks for covered/uncovered lines",
            Context::Global,
            crate::commands::always,
            |s, p| {
                let v = toggle(p, s.testing.coverage_in_editor);
                s.testing.coverage_in_editor = v;
                Ok(json!(v))
            },
        ),
        spec(
            "coverage.importLcov",
            "Import Coverage Report",
            None,
            "{path: string} — an LCOV file (stored with the project's reports)",
            Context::Global,
            has_project,
            import_lcov,
        ),
        simple("coverage.summary", "Coverage Summary", crate::commands::always, |s, _| {
            let root = s.project_root().unwrap_or_default();
            let sort = s.testing.coverage_sort;
            Ok(s.testing.coverage.as_ref().map(|c| coverage_json(c, &root, sort)).unwrap_or(Value::Null))
        }),
        spec(
            "coverage.sort",
            "Sort Coverage Table",
            None,
            "{column: name|line|function, descending?: bool} — order the Coverage tool window's rows (within \
             each directory); the same column again without `descending` flips the order → {column, descending}",
            Context::Global,
            crate::commands::always,
            |s, p| {
                let column = testing::CoverageColumn::parse(req_str(p, "column")?)
                    .ok_or_else(|| CommandError::BadParams("`column` is name, line or function".into()))?;
                let current = s.testing.coverage_sort;
                let descending = match p.get("descending").and_then(Value::as_bool) {
                    Some(d) => d,
                    None if current.column == column => !current.descending,
                    None => false,
                };
                s.testing.coverage_sort = testing::CoverageSort { column, descending };
                Ok(json!(s.testing.coverage_sort))
            },
        ),
    ]
}

fn has_test_run(s: &Session) -> Result<(), String> {
    if s.testing.runs.is_empty() { Err("no test run".into()) } else { Ok(()) }
}

fn project_root(s: &Session) -> Result<std::path::PathBuf, CommandError> {
    s.project_root().ok_or_else(|| CommandError::Disabled("no project is open".into()))
}

fn tab_param(s: &Session, p: &Value) -> Result<codecraft_workspace::RunTabId, CommandError> {
    let tab = opt_usize(p, "tab")?.map(|t| t as u64);
    testing::target_run(s, tab)
}

fn toggle(p: &Value, current: bool) -> bool {
    p.get("value").and_then(Value::as_bool).unwrap_or(!current)
}

fn coverage_json(report: &testing::CoverageReport, root: &std::path::Path, sort: testing::CoverageSort) -> Value {
    let (hit, found) = report.totals();
    let rows: Vec<Value> = testing::sort_rows(report.rows(root), sort)
        .into_iter()
        .map(|r| {
            json!({
                "path": r.path,
                "name": r.name,
                "depth": r.depth,
                "isDir": r.is_dir,
                "linesHit": r.lines_hit,
                "linesFound": r.lines_found,
                "linePercent": r.line_percent(),
                "functionPercent": r.function_percent(),
            })
        })
        .collect();
    json!({
        "id": report.id,
        "name": report.name,
        "linesHit": hit,
        "linesFound": found,
        "linePercent": report.line_percent(),
        "sort": sort,
        "rows": rows,
    })
}

fn rerun(s: &mut Session, p: &Value) -> CmdResult {
    let tab = tab_param(s, p)?;
    match testing::rerun_tab(s, tab) {
        Some(started) => Ok(json!({ "tab": started?.0 })),
        None => Err(CommandError::Failed("This test run cannot be rerun (it was imported)".into())),
    }
}

fn rerun_failed(s: &mut Session, p: &Value) -> CmdResult {
    let tab = tab_param(s, p)?;
    let run = s.testing.run(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    if !run.finished {
        return Err(CommandError::Disabled("the tests are still running".into()));
    }
    let title = run.title.clone();
    let target = run.target.clone();
    let origin = testing::rerun_failed_origin(run).map_err(CommandError::Failed)?;
    let new = testing::start_origin_on(s, origin, &title, target)?;
    Ok(json!({ "tab": new.0 }))
}

fn navigate(s: &mut Session, p: &Value) -> CmdResult {
    let tab = tab_param(s, p)?;
    let key = match opt_str(p, "key") {
        Some(k) => k.to_string(),
        None => s
            .testing
            .run(tab)
            .and_then(|r| r.view.selected.clone())
            .ok_or_else(|| CommandError::BadParams("no test is selected".into()))?,
    };
    let (si, ti) = testing::test_key(&key).ok_or_else(|| CommandError::BadParams(format!("`{key}` is not a test")))?;
    testing::navigate(s, tab, si, ti)
}

fn toggle_node(s: &mut Session, p: &Value) -> CmdResult {
    let tab = tab_param(s, p)?;
    let key = req_str(p, "key")?.to_string();
    let run = s.testing.run_mut(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    let collapsed = run.view.collapsed.contains(&key);
    let expand = p.get("expanded").and_then(Value::as_bool).unwrap_or(collapsed);
    if expand {
        run.view.collapsed.remove(&key);
    } else {
        run.view.collapsed.insert(key);
    }
    Ok(json!(expand))
}

fn test_history(s: &mut Session, _: &Value) -> CmdResult {
    let root = project_root(s)?;
    let list = testing::history_summaries(&root);
    s.testing.history_open = true;
    if list.is_empty() {
        s.status = "No test history yet".into();
    }
    Ok(json!(list))
}

fn open_history(s: &mut Session, p: &Value) -> CmdResult {
    let root = project_root(s)?;
    let index = req_usize(p, "index")?;
    let mut runs = testing::load_history(&root);
    if index >= runs.len() {
        return Err(CommandError::BadParams(format!("no history entry {index}")));
    }
    let run = runs.swap_remove(index);
    let note = format!("Test history: {} ({} tests)", run.title, run.counts().total());
    let tab = testing::show_imported(s, run, &note);
    s.testing.history_open = false;
    Ok(json!({ "tab": tab.0 }))
}

fn import_tests(s: &mut Session, p: &Value) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        s.testing.import_requested = true;
        s.ws.layout.show(ToolWindowId::Run);
        return Ok(json!({ "pick": true }));
    };
    let root = project_root(s)?;
    let run = testing::import_file(&path, &root).map_err(CommandError::Failed)?;
    let note = format!("Imported from {}", path.display());
    let tab = testing::show_imported(s, run, &note);
    Ok(json!({ "tab": tab.0 }))
}

fn export(s: &mut Session, p: &Value) -> CmdResult {
    let tab = tab_param(s, p)?;
    let path = req_path(p, "path")?;
    let run = s.testing.run(tab).ok_or_else(|| CommandError::Failed("the test run is gone".into()))?;
    let text = serde_json::to_string_pretty(run).map_err(|e| CommandError::Failed(e.to_string()))?;
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| CommandError::Failed(e.to_string()))?;
    s.status = format!("Exported test results to {}", path.display());
    ok()
}

fn manage_reports(s: &mut Session, _: &Value) -> CmdResult {
    let root = project_root(s)?;
    s.testing.reports_open = true;
    Ok(json!(testing::list_reports(&root)))
}

fn import_lcov(s: &mut Session, p: &Value) -> CmdResult {
    let root = project_root(s)?;
    let path = req_path(p, "path")?;
    let text =
        codecraft_fs::read_text_file(&path).map_err(|e| CommandError::Failed(format!("{}: {e}", path.display())))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "coverage".into());
    let (report, _) = testing::store_report(&root, &root, &text, &name, &format!("Imported {}", path.display()))
        .map_err(CommandError::Failed)?;
    let summary = coverage_json(&report, &root, s.testing.coverage_sort);
    s.testing.coverage = Some(report);
    s.ws.layout.show(ToolWindowId::Coverage);
    Ok(summary)
}

/// What `run.runWithCoverage` runs: the test at path/line, the given or selected configuration,
/// else the workspace tests.
fn coverage_config(s: &Session, p: &Value) -> Result<RunConfig, CommandError> {
    if let (Some(path), Some(line)) = (opt_path(p, "path"), opt_usize(p, "line")?) {
        if let Some((title, inv)) = crate::run_cmds::context_invocation(s, &path, line) {
            return Ok(RunConfig::cargo(title, inv));
        }
        let text = codecraft_fs::read_text_file(&path).map_err(|e| CommandError::Failed(e.to_string()))?;
        let root = s.project_root().unwrap_or_default();
        return crate::run_configs::context_config(&root, &path, &text, line)
            .ok_or_else(|| CommandError::Failed("Nothing to run here".into()));
    }
    if let Some(i) = opt_usize(p, "config")? {
        return s
            .run_configs
            .get(i)
            .cloned()
            .ok_or_else(|| CommandError::BadParams(format!("no run configuration {i}")));
    }
    if let Some(c) = s.run_configs.get(s.selected_run_config) {
        return Ok(c.clone());
    }
    let root =
        s.ws.project
            .as_ref()
            .map(|p| p.cargo.as_ref().map(|m| m.workspace_root.clone()).unwrap_or_else(|| p.root.clone()))
            .ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
    Ok(RunConfig::cargo("Test workspace", CargoInvocation::new(CargoCommand::Test, root)))
}

fn run_with_coverage(s: &mut Session, p: &Value) -> CmdResult {
    let config = coverage_config(s, p)?;
    let tab = testing::run_with_coverage(s, config)?;
    Ok(json!({ "tab": tab.0 }))
}
