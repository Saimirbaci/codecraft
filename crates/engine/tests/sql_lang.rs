//! SQL in Rust strings and SQL editing through engine commands (M11 track Q): a SQLite data
//! source set up through the database commands, injected SQL in a Rust file, an `.sql` file.
#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use codecraft_engine::db::MemoryStore;
use codecraft_engine::workspace::{ProblemSource, Severity};
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
    panic!("timed out waiting for {what}; output: {:?}", s.db.output);
}

/// Pump for a while (longer than the settle delay).
fn pump_for(s: &mut Session, ms: u64) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(ms) {
        s.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
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
        "create table users (id integer primary key, name text not null, email text)",
        "create table orders (id integer primary key, user_id integer references users(id), total real)",
    ] {
        codecraft_engine::db::query_all(conn.as_mut(), sql).unwrap();
    }
}

fn session(dir: &Path) -> Session {
    let mut s = Session::new();
    s.lsp_enabled = false;
    s.db.set_secret_store(Arc::new(MemoryStore::default()));
    // Never write the query history to the real ~/.codecraft.
    s.db.history.set_dir(dir.join(".codecraft/test-history"));
    s.execute("file.open", &json!({ "path": dir })).unwrap();
    pump_until(&mut s, "project", |s| s.ws.project.is_some() && !s.jobs_running());
    s
}

/// A project with a SQLite database, `.env` pointing at it, and a data source for it.
fn project(connect: bool) -> (tempfile::TempDir, std::path::PathBuf, Session) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    make_db(&root.join("app.db"));
    std::fs::write(root.join(".env"), "DATABASE_URL=sqlite://app.db\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let mut s = session(&root);
    s.execute("db.addDataSource", &json!({ "kind": "sqlite", "name": "App", "path": "app.db" })).unwrap();
    if connect {
        s.execute("db.connect", &json!({ "dataSource": "app" })).unwrap();
        pump_until(&mut s, "tables", |s| {
            s.db.source("app").is_some_and(|d| d.tree.schemas.get("main").is_some_and(|n| n.tables.is_some()))
        });
    }
    (dir, root, s)
}

const RUST: &str = r##"fn main() {
    let a = sqlx::query!("SELECT u.nme, u.name FROM users u WHERE u.id = $1", 1);
    let b = conn.execute("INSERT INTO ordrs (id) VALUES (1)", []);
    let c = sqlx::query_as!(User, r#"select id from users"#);
}
"##;

fn open(s: &mut Session, path: &Path) {
    s.execute("file.open", &json!({ "path": path })).unwrap();
}

fn caret_at(s: &mut Session, offset: usize) {
    s.execute("editor.setSelection", &json!({ "anchor": offset, "head": offset })).unwrap();
}

fn char_offset(text: &str, needle: &str) -> usize {
    text[..text.find(needle).unwrap()].chars().count()
}

fn completion_labels(s: &Session) -> Vec<String> {
    match &s.popup {
        Some(Popup::Completion(c)) => c.items.iter().map(|i| i.label.clone()).collect(),
        other => panic!("no completion popup: {other:?}"),
    }
}

#[test]
fn injected_sql_is_found_highlighted_completed_and_checked() {
    let (_dir, root, mut s) = project(true);
    let rs = root.join("src/main.rs");
    std::fs::write(&rs, RUST).unwrap();
    open(&mut s, &rs);

    // The literals holding SQL, for agents.
    let r = s.execute("sql.injections", &Value::Null).unwrap();
    let list = r["injections"].as_array().unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["api"], "query!");
    assert_eq!(list[1]["api"], ".execute");
    assert_eq!(list[2]["raw"], true);
    assert_eq!(list[0]["line"], 1);
    let r = s.execute("sql.injections", &json!({ "path": rs })).unwrap();
    assert_eq!(r["injections"].as_array().unwrap().len(), 3);

    // Highlighting over the string colour (the UI layers these spans).
    s.pump();
    let did = s.ws.active().unwrap().1.id;
    let rev = s.ws.doc(did).unwrap().doc.revision();
    let hl = s.sql_lang.highlights_for(did, rev).expect("SQL highlights");
    let line = RUST.lines().nth(1).unwrap();
    let col = line.find("SELECT").unwrap();
    assert!(
        hl.line(1).iter().any(|sp| sp.start == col && sp.kind == codecraft_engine::syntax::HighlightKind::Keyword),
        "{:?}",
        hl.line(1)
    );

    // The data source comes from DATABASE_URL in .env.
    let r = s.execute("sql.settings", &Value::Null).unwrap();
    assert_eq!(r["dataSource"], "app");
    assert_eq!(r["dialect"], "sqlite");

    // Unresolved names become warnings once the text settled and the columns loaded.
    pump_until(&mut s, "warnings", |s| s.ws.problems_for(&rs).len() == 2);
    let problems: Vec<_> = s.ws.problems_for(&rs).into_iter().cloned().collect();
    assert!(problems.iter().all(|p| p.severity == Severity::Warning && p.source == ProblemSource::Inspection));
    assert_eq!(problems[0].message, "Unable to resolve column 'nme'");
    assert_eq!((problems[0].start.line, problems[0].start.column), (1, line.find("nme").unwrap()));
    assert_eq!(problems[0].end.column, line.find("nme").unwrap() + 3);
    assert_eq!(problems[1].message, "Unable to resolve table 'ordrs'");
    let r = s.execute("sql.problems", &Value::Null).unwrap();
    assert_eq!(r["problems"].as_array().unwrap().len(), 2);

    // Fixing the typo clears the warning.
    let nme = char_offset(RUST, "nme");
    s.execute("editor.setSelection", &json!({ "anchor": nme, "head": nme + 3 })).unwrap();
    s.execute("editor.type", &json!({ "text": "email" })).unwrap();
    pump_until(&mut s, "one warning", |s| s.ws.problems_for(&rs).len() == 1);

    // Completion after `u.`: the aliased table's columns (Basic completion works without a
    // language server inside SQL).
    let text = s.ws.active().unwrap().1.doc.text();
    let after_dot = char_offset(&text, "u.name") + 2;
    caret_at(&mut s, after_dot);
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    assert_eq!(completion_labels(&s), ["id", "name", "email"]);
    s.popup = None;
    // Typing `.` after an alias pops the columns up by itself.
    caret_at(&mut s, after_dot + 4);
    s.execute("editor.type", &json!({ "text": ", u" })).unwrap();
    assert!(s.popup.is_none());
    s.execute("editor.type", &json!({ "text": "." })).unwrap();
    assert!(completion_labels(&s).contains(&"email".to_string()));
    s.popup = None;
    // Tables after FROM; keywords elsewhere.
    let text = s.ws.active().unwrap().1.doc.text();
    caret_at(&mut s, char_offset(&text, "users u WHERE"));
    s.execute("sql.complete", &Value::Null).unwrap();
    let labels = completion_labels(&s);
    assert_eq!(&labels[..2], ["orders", "users"]);
    assert!(labels.contains(&"SELECT".to_string()));
    s.popup = None;

    // Outside SQL strings the commands are disabled.
    caret_at(&mut s, 0);
    assert!(s.execute("sql.reformatInjected", &Value::Null).is_err());
    assert!(s.execute("sql.complete", &Value::Null).is_err());
}

#[test]
fn reformat_injected_and_sql_files() {
    let (_dir, root, mut s) = project(true);
    let rs = root.join("src/q.rs");
    let text = "fn f() {\n    let q = sqlx::query!(\n        r#\"\n        select id, name from users where id = $1 and name = 'x'\n        \"#,\n        1\n    );\n    conn.execute(\"select \\\"id\\\" from users where id in (select user_id from orders)\", []);\n}\n";
    std::fs::write(&rs, text).unwrap();
    open(&mut s, &rs);
    caret_at(&mut s, char_offset(text, "select id"));
    let r = s.execute("sql.reformatInjected", &Value::Null).unwrap();
    assert_eq!(r["changed"], true);
    let after = s.ws.active().unwrap().1.doc.text();
    assert!(
        after.contains(
            "        r#\"\n        SELECT id, name\n        FROM users\n        WHERE id = $1\n            AND name = 'x'\n        \"#,"
        ),
        "{after}"
    );
    // Again: nothing to do.
    caret_at(&mut s, char_offset(&after, "SELECT id"));
    assert_eq!(s.execute("sql.reformatInjected", &Value::Null).unwrap()["changed"], false);
    // A normal string: escapes kept, continuation lines aligned with the content.
    caret_at(&mut s, char_offset(&after, "select \\\"id"));
    s.execute("sql.reformatInjected", &Value::Null).unwrap();
    let after = s.ws.active().unwrap().1.doc.text();
    let col = "    conn.execute(\"".chars().count();
    let pad = " ".repeat(col);
    assert!(
        after.contains(&format!(
            "conn.execute(\"SELECT \\\"id\\\"\n{pad}FROM users\n{pad}WHERE id IN (\n{pad}    SELECT user_id\n{pad}    FROM orders\n{pad})\", [])"
        )),
        "{after}"
    );
    // Undo restores the original.
    s.execute("edit.undo", &Value::Null).unwrap();
    assert!(s.ws.active().unwrap().1.doc.text().contains("select \\\"id\\\" from users"));

    // Reformat Code in an .sql file, with the project's keyword case.
    s.execute("sql.settings", &json!({ "keywordCase": "lower", "indent": 2 })).unwrap();
    let saved = std::fs::read_to_string(root.join(".codecraft/sql.json")).unwrap();
    assert!(saved.contains("\"lower\""), "{saved}");
    let sql = root.join("queries.sql");
    std::fs::write(&sql, "SELECT nme FROM users WHERE id = 1 AND email IS NULL;\n").unwrap();
    open(&mut s, &sql);
    s.execute("code.reformatCode", &Value::Null).unwrap();
    assert_eq!(s.ws.active().unwrap().1.doc.text(), "select nme\nfrom users\nwhere id = 1\n  and email is null;\n");
    // `.sql` files are checked against their data source too.
    pump_until(&mut s, "sql file warning", |s| s.ws.problems_for(&sql).len() == 1);
    assert_eq!(s.ws.problems_for(&sql)[0].message, "Unable to resolve column 'nme'");
    assert_eq!(s.ws.problems_for(&sql)[0].start.line, 0);

    // The formatter for agents.
    let r = s.execute("sql.formatText", &json!({ "text": "select 1 from t", "keywordCase": "upper" })).unwrap();
    assert_eq!(r["text"], "SELECT 1\nFROM t");
    assert!(s.execute("sql.formatText", &json!({ "text": "x", "keywordCase": "shouting" })).is_err());
    assert!(s.execute("sql.formatText", &json!({ "text": "x", "indent": 99 })).is_err());
}

#[test]
fn without_a_connection_only_keywords_and_no_warnings() {
    let (_dir, root, mut s) = project(false);
    let rs = root.join("src/main.rs");
    std::fs::write(&rs, RUST).unwrap();
    open(&mut s, &rs);
    pump_for(&mut s, 600);
    assert!(s.ws.problems_for(&rs).is_empty());
    assert_eq!(s.execute("sql.settings", &Value::Null).unwrap()["dataSource"], Value::Null);
    caret_at(&mut s, char_offset(RUST, "u.name") + 2);
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    // After `u.` nothing is known; at a word, keywords only.
    assert!(s.popup.is_none());
    caret_at(&mut s, char_offset(RUST, "FROM users u") + 2);
    s.execute("code.codeCompletion.basic", &Value::Null).unwrap();
    let Some(Popup::Completion(c)) = &s.popup else { panic!("no popup") };
    assert!(c.items.iter().all(|i| i.kind == "keyword"));
    assert!(s.db.source("app").is_some_and(|d| d.tree.schemas.is_empty()), "completion never connects");
}

#[test]
fn choosing_the_data_source() {
    let (_dir, root, mut s) = project(true);
    // Without params: a chooser listing Automatic and each data source.
    s.execute("sql.chooseDataSource", &Value::Null).unwrap();
    let Some(Popup::Actions { items, .. }) = &s.popup else { panic!("no chooser") };
    assert_eq!(items.len(), 2);
    assert!(items[0].title.contains("Automatic"));
    s.popup = None;
    // Choose explicitly: saved per project.
    let r = s.execute("sql.chooseDataSource", &json!({ "dataSource": "app" })).unwrap();
    assert_eq!(r["resolved"], "app");
    let saved = std::fs::read_to_string(root.join(".codecraft/sql.json")).unwrap();
    assert!(saved.contains("\"dataSource\": \"app\""), "{saved}");
    assert!(s.execute("sql.chooseDataSource", &json!({ "dataSource": "nope" })).is_err());
    assert!(s.execute("sql.chooseDataSource", &json!({ "dataSource": 5 })).is_err());
    // Back to automatic: DATABASE_URL still finds it.
    let r = s.execute("sql.chooseDataSource", &json!({ "dataSource": null })).unwrap();
    assert_eq!(r["resolved"], "app");
    assert!(!std::fs::read_to_string(root.join(".codecraft/sql.json")).unwrap().contains("dataSource"));
    // Dialect override.
    let r = s.execute("sql.settings", &json!({ "dialect": "postgres" })).unwrap();
    assert_eq!(r["dialect"], "postgres");
    assert!(s.execute("sql.settings", &json!({ "dialect": "oracle" })).is_err());
}
