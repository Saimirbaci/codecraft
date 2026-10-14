//! Track DC through engine commands (SQLite in a temp dir; PostgreSQL only when a local
//! server accepts connections): Go to Declaration and Quick Documentation from SQL, SQL
//! inspections and quick-fixes, alias rename, Find Usages across .sql and Rust files, attached
//! files and Run 'file.sql' in each error mode, Go to Database Object, the explorer, pinned
//! result tabs, result titles and SQL live templates.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::database::{ScriptPhase, StmtStatus, node_key};
use codecraft_engine::db::{MemoryStore, query_all};
use codecraft_engine::{Popup, Session};

fn pump_until(s: &mut Session, what: &str, mut done: impl FnMut(&Session) -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        s.pump();
        if done(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}; status: {}; output: {:?}", s.status, s.db.output);
}

fn make_db(path: &Path) {
    let mut conn = codecraft_engine::db::connect(
        &codecraft_engine::db::DataSourceConfig {
            kind: codecraft_engine::db::DbKind::Sqlite,
            path: Some(path.to_path_buf()),
            ..Default::default()
        },
        None,
    )
    .unwrap();
    for sql in [
        "create table authors (id integer primary key, name text not null)",
        "create table books (id integer primary key, author_id integer references authors(id), title text, note text)",
        "create index books_by_author on books(author_id)",
        "create view titles as select title from books",
        "insert into authors(name) values ('Ann'), ('Bob')",
        "insert into books(author_id, title) values (1, 'Dune'), (2, 'Emma'), (1, 'Ulysses')",
    ] {
        query_all(conn.as_mut(), sql).unwrap();
    }
}

fn session(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.isolate_home(&dir.join(".home"));
    s.db.set_secret_store(Arc::new(MemoryStore::default()));
    s.db.history.set_dir(dir.join(".codecraft/test-history"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "project", |s| s.ws.project.is_some() && !s.jobs_running());
    s
}

/// A project with lib.db, a data source `library` (connected, `main` loaded) and `files`.
fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf, Session) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("lib.db"));
    for (name, text) in files {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    let mut s = session(&root);
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "Library", "path": "lib.db" })).unwrap();
    s.execute("db.connect", &json!({ "dataSource": "library" })).unwrap();
    pump_until(&mut s, "schema", |s| {
        s.db.source("library").and_then(|d| d.tree.schemas.get("main")).is_some_and(|n| n.tables.is_some())
    });
    (dir, root, s)
}

fn open(s: &mut Session, path: &Path) {
    s.execute("file.open", &json!({ "path": path })).unwrap();
}

/// Put the caret on the `nth` (0-based) occurrence of `needle` (plus `extra` chars).
fn caret_at(s: &mut Session, needle: &str, nth: usize, extra: usize) -> usize {
    let text = s.ws.active().unwrap().1.doc.text();
    let byte = text.match_indices(needle).nth(nth).unwrap_or_else(|| panic!("no {needle} in {text}")).0;
    let at = text[..byte].chars().count() + extra;
    let (e, _) = s.ws.active().unwrap();
    let id = e.id;
    s.ws.set_selections(id, codecraft_text::SelectionSet::caret(at));
    at
}

fn active_path(s: &Session) -> PathBuf {
    s.ws.active().unwrap().1.path.clone().unwrap()
}

#[test]
fn navigation_quick_doc_and_rename_from_sql() {
    let sql = "select b.title, a.name from books b join authors a on a.id = b.author_id where b.id = 1;\n";
    let (_dir, root, mut s) = project(&[("q.sql", sql)]);
    let q = root.join("q.sql");
    open(&mut s, &q);
    s.execute("db.attachFile", &json!({ "dataSource": "library" })).unwrap();

    // Cmd+B on a table name: the tree selects it, its DDL opens read-only.
    caret_at(&mut s, "books", 0, 2);
    s.execute("navigate.declarationOrUsages", &Value::Null).unwrap();
    pump_until(&mut s, "DDL", |s| {
        s.ws.active().and_then(|(_, d)| d.path.clone()).is_some_and(|p| p.ends_with("books.sql"))
    });
    let (_, d) = s.ws.active().unwrap();
    assert!(d.read_only);
    let ddl = d.doc.text();
    assert!(ddl.contains("CREATE TABLE books") || ddl.contains("create table books"), "{ddl}");
    assert!(ddl.contains("books_by_author"), "{ddl}");
    assert_eq!(s.db.selected.as_deref(), Some(node_key("library", Some("main"), Some("books")).as_str()));

    // Back in the query: an alias use jumps to its definition.
    open(&mut s, &q);
    caret_at(&mut s, "a.name", 0, 0);
    let r = s.execute("sql.gotoDeclaration", &Value::Null).unwrap();
    let def = sql.find("authors a").unwrap() + "authors ".len();
    assert_eq!(r["offset"], def, "{r}");
    assert_eq!(s.ws.active().unwrap().0.selections.primary().head, def);

    // A column: the DDL of its table, caret on the column's line.
    caret_at(&mut s, "a.name", 0, 3);
    s.execute("sql.gotoDeclaration", &Value::Null).unwrap();
    pump_until(&mut s, "authors DDL", |s| active_path(s).ends_with("authors.sql"));
    let (e, d) = s.ws.active().unwrap();
    let line = d.doc.char_to_line(e.selections.primary().head);
    assert!(d.doc.line_text(line).unwrap().contains("name"), "{:?}", d.doc.line_text(line));

    // Quick Documentation (F1): columns and DDL.
    open(&mut s, &q);
    caret_at(&mut s, "authors", 0, 1);
    s.execute("view.quickDocumentation", &Value::Null).unwrap();
    pump_until(&mut s, "doc", |s| matches!(&s.popup, Some(Popup::Hover { .. })));
    let Some(Popup::Hover { text, .. }) = &s.popup else { panic!("no documentation popup") };
    assert!(text.starts_with("**authors** · table in `main`"), "{text}");
    assert!(text.to_lowercase().contains("- `name` text, not null"), "{text}");
    s.popup = None;
    let r =
        s.execute("sql.objectInfo", &json!({ "dataSource": "library", "schema": "main", "name": "titles" })).unwrap();
    let r = if r["loading"] == true {
        pump_until(&mut s, "view info", |s| {
            s.db.nav.info.get(&("library".into(), "main".into(), "titles".into())).is_some_and(|i| !i.loading)
                && s.db.source("library").unwrap().table("main", "titles").unwrap().details.is_some()
        });
        s.execute("sql.objectInfo", &json!({ "dataSource": "library", "schema": "main", "name": "titles" })).unwrap()
    } else {
        r
    };
    assert!(r["ddl"].as_str().unwrap().to_lowercase().contains("create view titles"), "{r}");

    // Rename (Shift+F6) of an alias renames it in the statement only.
    caret_at(&mut s, "b.title", 0, 0);
    let r = s.execute("refactor.rename", &Value::Null).unwrap();
    assert_eq!(r["rename"], "b");
    assert!(matches!(s.popup, Some(Popup::Rename { .. })));
    s.execute("refactor.rename", &json!({ "newName": "bk" })).unwrap();
    assert_eq!(
        s.ws.active().unwrap().1.doc.text(),
        "select bk.title, a.name from books bk join authors a on a.id = bk.author_id where bk.id = 1;\n"
    );
    s.shutdown();
}

#[test]
fn inspections_and_quick_fixes() {
    let (_dir, root, mut s) =
        project(&[("fix.sql", "delete from books;\nselect id from books b join authors a on a.id = b.author_id;\n")]);
    open(&mut s, &root.join("fix.sql"));
    s.execute("db.attachFile", &json!({ "dataSource": "library" })).unwrap();
    // Columns are loaded on demand; check until the ambiguous column is found.
    let mut codes = Vec::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let r = s.execute("sql.problems", &Value::Null).unwrap();
        codes =
            r["problems"].as_array().unwrap().iter().map(|p| p["code"].as_str().unwrap_or("").to_string()).collect();
        if codes.contains(&"SqlAmbiguousColumn".to_string()) {
            break;
        }
        s.pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(codes.contains(&"SqlWithoutWhere".to_string()), "{codes:?}");
    assert!(codes.contains(&"SqlAmbiguousColumn".to_string()), "{codes:?}");

    // Alt+Enter in the DELETE: Add WHERE clause.
    caret_at(&mut s, "delete", 0, 1);
    s.execute("code.showContextActions", &Value::Null).unwrap();
    let Some(Popup::Actions { items, .. }) = &s.popup else { panic!("no actions: {}", s.status) };
    let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles[0], "Add WHERE clause", "{titles:?}");
    assert!(titles.contains(&"Suppress for statement"));
    s.execute("code.applyContextAction", &json!({ "index": 0 })).unwrap();
    let text = s.ws.active().unwrap().1.doc.text();
    assert!(text.starts_with("delete from books WHERE ;"), "{text}");
    let caret = s.ws.active().unwrap().0.selections.primary().head;
    assert_eq!(caret, "delete from books WHERE ".len());

    // Qualify the ambiguous column.
    caret_at(&mut s, "select id", 0, 7);
    let fixes = s.execute("sql.quickFixes", &Value::Null).unwrap();
    let titles: Vec<String> =
        fixes["fixes"].as_array().unwrap().iter().map(|f| f["title"].as_str().unwrap().to_string()).collect();
    assert!(titles.contains(&"Qualify with 'b'".to_string()), "{titles:?}");
    s.execute("sql.applyFix", &json!({ "title": "Qualify with 'b'" })).unwrap();
    assert!(s.ws.active().unwrap().1.doc.text().contains("select b.id from books b"));

    // Suppression comment.
    caret_at(&mut s, "delete", 0, 1);
    let r = s.execute("sql.problems", &Value::Null).unwrap();
    assert!(r["problems"].as_array().unwrap().iter().all(|p| p["code"] != "SqlWithoutWhere"), "{r}");
    s.execute("edit.selectAll", &Value::Null).unwrap();
    s.execute("editor.type", &json!({ "text": "update books set note = 'x'" })).unwrap();
    s.execute("sql.applyFix", &json!({ "title": "Suppress for statement", "offset": 1 })).unwrap();
    assert_eq!(s.ws.active().unwrap().1.doc.text(), "-- noinspection SqlWithoutWhere\nupdate books set note = 'x'");
    let r = s.execute("sql.problems", &Value::Null).unwrap();
    assert!(r["problems"].as_array().unwrap().is_empty(), "{r}");
    s.shutdown();
}

#[test]
fn find_usages_across_sql_and_rust() {
    let (_dir, root, mut s) = project(&[
        ("a.sql", "select title from books;\nselect * from authors;\n"),
        ("sql/b.sql", "update books set note = null where id = 1;\n"),
        ("src/main.rs", "fn main() {\n    let _ = sqlx::query(\"SELECT title FROM books WHERE id = $1\");\n}\n"),
    ]);
    open(&mut s, &root.join("a.sql"));
    s.execute("db.attachFile", &json!({ "dataSource": "library" })).unwrap();
    caret_at(&mut s, "books", 0, 1);
    let r = s.execute("edit.findUsages.findUsages", &Value::Null).unwrap();
    assert_eq!(r["title"], "Usages of table 'books'");
    pump_until(&mut s, "usages", |s| s.ws.find.as_ref().is_some_and(|f| f.title.contains("books")));
    let f = s.ws.find.clone().unwrap();
    let files: std::collections::BTreeSet<String> =
        f.matches.iter().map(|m| m.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(files.into_iter().collect::<Vec<_>>(), ["a.sql", "b.sql", "main.rs"], "{:?}", f.matches);
    // A column through the parameters.
    s.ws.find = None;
    s.execute("sql.findUsages", &json!({ "table": "books", "column": "title" })).unwrap();
    pump_until(&mut s, "column usages", |s| s.ws.find.is_some());
    let f = s.ws.find.clone().unwrap();
    assert_eq!(f.matches.len(), 2, "{:?}", f.matches);
    assert!(f.matches.iter().any(|m| m.path.ends_with("main.rs") && m.line == 1));
    s.shutdown();
}

fn run_script(s: &mut Session, path: &Path, mode: &str) -> u64 {
    let r = s.execute("db.runFile", &json!({ "path": path, "mode": mode })).unwrap();
    let id = r["script"].as_u64().unwrap_or_else(|| panic!("{r}"));
    pump_until(s, "script", |s| s.db.scripts.get(id).is_some_and(|r| r.state != ScriptPhase::Running) && !s.db.busy());
    id
}

fn statuses(s: &Session, id: u64) -> Vec<&'static str> {
    s.db.scripts
        .get(id)
        .unwrap()
        .statements
        .iter()
        .map(|st| match st.status {
            StmtStatus::Pending => "pending",
            StmtStatus::Done { .. } => "done",
            StmtStatus::Failed { .. } => "failed",
            StmtStatus::Skipped => "skipped",
        })
        .collect()
}

#[test]
fn run_sql_file_in_each_error_mode() {
    let script = "select count(*) from books;\n\ninsert into nope values (1);\nselect name from authors order by name;\nupdate books set note = 'n';\n";
    let (_dir, root, mut s) = project(&[("script.sql", script)]);
    let path = root.join("script.sql");
    // A file outside the project or unknown: a clear error.
    assert!(s.execute("db.runFile", &json!({ "path": root.join("missing.sql") })).is_err());
    s.execute("db.attachFile", &json!({ "path": path, "dataSource": "library" })).unwrap();

    let id = run_script(&mut s, &path, "stop");
    assert_eq!(statuses(&s, id), ["done", "failed", "skipped", "skipped"]);
    let run = s.db.scripts.get(id).unwrap();
    assert_eq!(run.statements.iter().map(|st| st.line).collect::<Vec<_>>(), [0, 2, 3, 4]);
    assert_eq!(run.state, ScriptPhase::Stopped);
    assert!(run.summary().starts_with("4 statements: 1 succeeded, 1 failed, 2 skipped"), "{}", run.summary());
    let log = s.execute("db.scriptLog", &Value::Null).unwrap();
    assert_eq!(log["statements"][1]["status"]["state"], "failed");
    assert!(log["statements"][1]["status"]["error"].as_str().unwrap().contains("no such table"), "{log}");

    let id = run_script(&mut s, &path, "continue");
    assert_eq!(statuses(&s, id), ["done", "failed", "done", "done"]);
    // One result tab per query, titled after its table.
    let titles: Vec<String> =
        s.db.results.iter().filter(|r| r.owner.starts_with("script:")).map(|r| r.title.clone()).collect();
    assert_eq!(titles, ["books", "authors"]);
    match &s.db.scripts.get(id).unwrap().statements[3].status {
        StmtStatus::Done { affected, .. } => assert_eq!(*affected, Some(3)),
        other => panic!("{other:?}"),
    }

    let id = run_script(&mut s, &path, "ask");
    assert_eq!(s.db.scripts.get(id).unwrap().state, ScriptPhase::Asking);
    assert_eq!(s.db.scripts.get(id).unwrap().asking_at(), Some(1));
    s.execute("db.scriptDecision", &json!({ "action": "continue" })).unwrap();
    pump_until(&mut s, "rest", |s| s.db.scripts.get(id).is_some_and(|r| r.state == ScriptPhase::Done) && !s.db.busy());
    assert_eq!(statuses(&s, id), ["done", "failed", "done", "done"]);
    let id = run_script(&mut s, &path, "ask");
    s.execute("db.scriptDecision", &json!({ "script": id, "action": "stop" })).unwrap();
    assert_eq!(statuses(&s, id), ["done", "failed", "skipped", "skipped"]);
    assert!(s.execute("db.scriptDecision", &json!({ "script": id, "action": "continue" })).is_err());

    // The error mode is a project setting.
    s.execute("db.setScriptMode", &json!({ "mode": "continue" })).unwrap();
    assert!(std::fs::read_to_string(root.join(".codecraft/sql-editor.json")).unwrap().contains("\"continue\""));
    let r = s.execute("db.runFile", &json!({ "path": path })).unwrap();
    assert_eq!(r["mode"], "continue");
    pump_until(&mut s, "default mode", |s| !s.db.scripts.busy() && !s.db.busy());

    // Production data sources ask first.
    s.execute("db.editDataSource", &json!({ "dataSource": "library", "confirmWrites": true })).unwrap();
    let r = s.execute("db.runFile", &json!({ "path": path, "mode": "stop" })).unwrap();
    assert_eq!(r["confirm"], true);
    assert_eq!(s.db.scripts.confirm.as_ref().unwrap().writes.len(), 2);
    s.execute("db.scriptConfirm", &json!({ "confirm": false })).unwrap();
    assert!(s.db.scripts.confirm.is_none());
    s.shutdown();
}

#[test]
fn database_objects_explorer_pins_titles_and_templates() {
    let (_dir, root, mut s) = project(&[("t.sql", "")]);
    // Go to Database Object.
    s.execute("db.gotoObject", &Value::Null).unwrap();
    assert!(s.db.nav.goto.is_some());
    let r = s.execute("db.gotoObjectResults", &json!({ "query": "book" })).unwrap();
    assert_eq!(r["objects"][0]["name"], "books", "{r}");
    let r = s.execute("db.gotoObjectResults", &json!({ "query": "main.ti" })).unwrap();
    assert_eq!(r["objects"][0]["name"], "titles");
    assert_eq!(r["objects"][0]["kind"], "view");
    s.db.nav.goto.as_mut().unwrap().query = "authors".into();
    s.execute("db.gotoObjectAccept", &Value::Null).unwrap();
    assert!(s.db.nav.goto.is_none());
    assert_eq!(s.db.selected.as_deref(), Some(node_key("library", Some("main"), Some("authors")).as_str()));
    s.execute("db.gotoObject", &json!({ "query": "books" })).unwrap();
    let r = s.execute("db.gotoObjectAccept", &json!({ "open": "data" })).unwrap();
    assert!(r["run"].as_u64().is_some(), "{r}");
    pump_until(&mut s, "table", |s| s.db.active().is_some_and(|r| !r.is_running() && r.rows.len() == 3));

    // Explorer: filter, schemas to introspect, refresh one node.
    s.execute("db.explorer.filter", &json!({ "text": "auth" })).unwrap();
    assert!(s.db.nav.filter_matches("authors") && !s.db.nav.filter_matches("books"));
    s.execute("db.explorer.filter", &json!({ "text": "" })).unwrap();
    let r = s.execute("db.introspectSchemas", &json!({ "dataSource": "library" })).unwrap();
    assert_eq!(r["schemas"], json!(["main"]));
    assert!(s.db.nav.schemas_dialog.is_some());
    s.execute("db.introspectSchemas", &json!({ "dataSource": "library", "schemas": ["other"] })).unwrap();
    pump_until(&mut s, "introspection", |s| {
        s.db.source("library").is_some_and(|d| !d.tree.loading && d.tree.schemas.is_empty())
    });
    s.execute("db.introspectSchemas", &json!({ "dataSource": "library", "schemas": null })).unwrap();
    pump_until(&mut s, "all schemas", |s| s.db.source("library").is_some_and(|d| d.tree.schemas.contains_key("main")));
    s.execute("db.refreshNode", &json!({ "dataSource": "library", "schema": "main" })).unwrap();
    pump_until(&mut s, "refresh", |s| {
        s.db.source("library")
            .and_then(|d| d.tree.schemas.get("main"))
            .is_some_and(|n| n.tables.as_ref().is_some_and(|t| t.len() == 3))
    });
    let r = s.execute("db.routines", &json!({ "dataSource": "library", "schema": "main" })).unwrap();
    assert_eq!(r["routines"], json!([]), "SQLite has no routines");

    // Result tabs: titled after the table; a pinned tab survives the next run.
    let t = root.join("t.sql");
    open(&mut s, &t);
    s.execute("db.attachFile", &json!({ "dataSource": "library" })).unwrap();
    s.execute("editor.type", &json!({ "text": "select count(*) from books" })).unwrap();
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "first", |s| !s.db.busy() && s.db.results.iter().any(|r| r.owner.starts_with("console:")));
    let first = s.db.results.iter().find(|r| r.owner.starts_with("console:")).unwrap().id;
    assert_eq!(s.db.result(first).unwrap().title, "books");
    assert_eq!(s.execute("db.results.pin", &json!({ "result": first })).unwrap()["pinned"], true);
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "second", |s| {
        !s.db.busy() && s.db.results.iter().filter(|r| r.owner.starts_with("console:")).count() == 2
    });
    s.execute("db.results.pin", &json!({ "result": first, "pinned": false })).unwrap();
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "third", |s| {
        !s.db.busy() && s.db.results.iter().filter(|r| r.owner.starts_with("console:")).count() == 1
    });

    // SQL live templates.
    s.execute("edit.selectAll", &Value::Null).unwrap();
    s.execute("editor.type", &json!({ "text": "sel" })).unwrap();
    s.execute("editor.expandLiveTemplate", &Value::Null).unwrap();
    assert!(
        s.ws.active().unwrap().1.doc.text().starts_with("SELECT *\nFROM "),
        "{}",
        s.ws.active().unwrap().1.doc.text()
    );
    s.shutdown();
}

#[test]
fn hostile_sql_never_panics_the_commands() {
    let nasty = "select 'unterminated\n; $$ dollar ; /* open\n\"q ; WITH a AS ( ; é.ü(";
    let (_dir, root, mut s) = project(&[("bad.sql", nasty)]);
    open(&mut s, &root.join("bad.sql"));
    s.execute("db.attachFile", &json!({ "dataSource": "library" })).unwrap();
    let n = nasty.chars().count();
    for at in [0, 3, 10, 20, n / 2, n.saturating_sub(1), n, n + 5] {
        for (id, p) in [
            ("sql.gotoDeclaration", json!({ "offset": at })),
            ("sql.quickDoc", json!({ "offset": at })),
            ("sql.quickFixes", json!({ "offset": at })),
            ("sql.findUsages", json!({ "offset": at })),
            ("sql.renameAlias", json!({ "offset": at, "newName": "x" })),
            ("sql.applyFix", json!({ "offset": at })),
        ] {
            let _ = s.execute(id, &p);
        }
    }
    let _ = s.execute("sql.problems", &Value::Null);
    let _ = s.execute("db.runFile", &json!({ "mode": "continue" }));
    pump_until(&mut s, "settle", |s| !s.db.busy() && !s.db.scripts.busy());
    for (id, p) in [
        ("db.gotoObjectAccept", json!({ "index": 9999 })),
        ("db.scriptDecision", json!({ "action": "maybe" })),
        ("db.results.pin", json!({ "result": 424242 })),
        ("db.introspectSchemas", json!({ "dataSource": "library", "schemas": 5 })),
        ("sql.openDdl", json!({ "dataSource": "library", "schema": "nope", "name": "x" })),
        ("db.attachFile", json!({ "dataSource": "nope" })),
        ("db.runFile", json!({ "mode": "sideways" })),
    ] {
        assert!(s.execute(id, &p).is_err(), "{id} accepted {p}");
    }
    s.shutdown();
}

// ---------------------------------------------------------------- PostgreSQL (optional)

fn pg_params() -> Value {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    json!({
        "kind": "postgres",
        "name": "pg",
        "host": env("CODECRAFT_TEST_PG_HOST", "localhost"),
        "port": env("CODECRAFT_TEST_PG_PORT", "5432"),
        "user": env("CODECRAFT_TEST_PG_USER", &std::env::var("USER").unwrap_or_default()),
        "database": env("CODECRAFT_TEST_PG_DB", "postgres"),
        "password": std::env::var("CODECRAFT_TEST_PG_PASSWORD").unwrap_or_default(),
        "savePassword": false,
        "options": { "connect_timeout": "3" },
    })
}

#[test]
fn postgres_schema_switch_routines_and_comments() {
    let p = pg_params();
    let cfg = codecraft_engine::db::DataSourceConfig {
        kind: codecraft_engine::db::DbKind::Postgres,
        host: p["host"].as_str().unwrap().into(),
        port: p["port"].as_str().unwrap().parse().ok(),
        user: p["user"].as_str().unwrap().into(),
        database: p["database"].as_str().unwrap().into(),
        options: [("connect_timeout".to_string(), "3".to_string())].into_iter().collect(),
        ..Default::default()
    };
    let password = std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok();
    let Ok(mut conn) = codecraft_engine::db::connect(&cfg, password.as_deref()) else {
        eprintln!("skipping: no PostgreSQL server");
        return;
    };
    let schema = format!("codecraft_dc_{}", std::process::id());
    struct Drop2(Box<dyn codecraft_engine::db::Connection>, String);
    impl Drop for Drop2 {
        fn drop(&mut self) {
            let _ = query_all(self.0.as_mut(), &format!("DROP SCHEMA IF EXISTS {} CASCADE", self.1));
        }
    }
    for sql in [
        format!("DROP SCHEMA IF EXISTS {schema} CASCADE"),
        format!("CREATE SCHEMA {schema}"),
        format!("CREATE TABLE {schema}.gadgets (id int PRIMARY KEY, label text)"),
        format!("COMMENT ON TABLE {schema}.gadgets IS 'Things we sell'"),
        format!("COMMENT ON COLUMN {schema}.gadgets.label IS 'Shown to customers'"),
        format!("CREATE FUNCTION {schema}.twice(x int) RETURNS int AS $$ SELECT x * 2 $$ LANGUAGE sql"),
    ] {
        query_all(conn.as_mut(), &sql).unwrap();
    }
    let _guard = Drop2(conn, schema.clone());

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("pg.sql"), "select current_schema();\nselect twice(label::int) from gadgets;\n").unwrap();
    let mut s = session(&root);
    let id = s.execute("db.addDataSource", &pg_params()).unwrap()["dataSource"].as_str().unwrap().to_string();
    s.execute("db.connect", &json!({ "dataSource": id })).unwrap();
    pump_until(&mut s, "pg", |s| s.db.source(&id).is_some_and(|d| d.tree.schemas.contains_key(&schema)));
    open(&mut s, &root.join("pg.sql"));
    s.execute("db.attachFile", &json!({ "dataSource": id, "schema": schema })).unwrap();
    // Running in the file switches its connection to the schema first.
    caret_at(&mut s, "current_schema", 0, 0);
    s.execute("db.execute", &Value::Null).unwrap();
    pump_until(&mut s, "current_schema", |s| !s.db.busy() && s.db.active().is_some_and(|r| !r.rows.is_empty()));
    assert_eq!(s.db.active().unwrap().rows[0][0].display(), schema);
    // Routines and comments from the catalog.
    let r = s.execute("db.routines", &json!({ "dataSource": id, "schema": schema })).unwrap();
    if r["loading"] == true {
        pump_until(&mut s, "routines", |s| s.db.nav.routines_of(&id, &schema).is_some_and(|r| r.items.is_some()));
    }
    let objects = s.execute("db.gotoObjectResults", &json!({ "query": "twice" })).unwrap();
    assert_eq!(objects["objects"][0]["kind"], "routine", "{objects}");
    caret_at(&mut s, "gadgets", 0, 1);
    s.execute("sql.quickDoc", &Value::Null).unwrap();
    pump_until(&mut s, "pg doc", |s| matches!(&s.popup, Some(Popup::Hover { .. })));
    let Some(Popup::Hover { text, .. }) = &s.popup else { panic!("no documentation popup") };
    assert!(text.contains("Things we sell") && text.contains("Shown to customers"), "{text}");
    s.popup = None;
    caret_at(&mut s, "twice", 0, 1);
    s.execute("sql.gotoDeclaration", &Value::Null).unwrap();
    pump_until(&mut s, "routine DDL", |s| active_path(s).ends_with("twice.sql"));
    assert!(s.ws.active().unwrap().1.doc.text().contains("CREATE OR REPLACE FUNCTION"));
    s.shutdown();
}
