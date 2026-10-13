//! SQLite end to end: introspection, execution, paging, NULLs, statement splitting and cancel.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::mpsc;
use std::time::{Duration, Instant};

use codecraft_db::{
    Cell, Column, Connection, DataSourceConfig, DbError, DbKind, Dialect, Page, ResultSink, TableKind, connect,
    query_all, split_statements,
};

fn config(path: &std::path::Path) -> DataSourceConfig {
    DataSourceConfig {
        id: "t".into(),
        name: "test".into(),
        kind: DbKind::Sqlite,
        path: Some(path.to_path_buf()),
        ..Default::default()
    }
}

const SCHEMA: &str = "
create table authors (id integer primary key, name text not null, born integer default 1900);
create table books (
    id integer primary key,
    author_id integer references authors(id),
    title text,
    note text
);
create unique index books_title on books(title);
create view long_books as select * from books where length(title) > 10;
insert into authors(name) values ('Ann'), ('Bob');
";

fn setup() -> (tempfile::TempDir, Box<dyn Connection>) {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = connect(&config(&dir.path().join("t.db")), None).unwrap();
    for stmt in split_statements(SCHEMA, Dialect::Sqlite) {
        query_all(conn.as_mut(), &stmt.text).unwrap();
    }
    let (_, _, sum) = query_all(
        conn.as_mut(),
        "insert into books(author_id, title, note) \
         with recursive n(i) as (select 1 union all select i + 1 from n where i < 1234) \
         select 1 + i % 2, 'Book number ' || i, case when i % 3 = 0 then null when i % 3 = 1 then '' else 'x' end from n",
    )
    .unwrap();
    assert_eq!(sum.rows_affected, Some(1234));
    (dir, conn)
}

#[test]
fn introspection() {
    let (_dir, mut conn) = setup();
    assert!(conn.server_version().unwrap().starts_with("SQLite 3."));
    let info = conn.database_info().unwrap();
    assert_eq!(info.schemas, vec!["main".to_string()]);
    assert_eq!(info.default_schema.as_deref(), Some("main"));
    let tables = conn.tables("main").unwrap();
    let names: Vec<_> = tables.iter().map(|t| (t.name.as_str(), t.kind)).collect();
    assert_eq!(
        names,
        vec![("authors", TableKind::Table), ("books", TableKind::Table), ("long_books", TableKind::View)]
    );

    let d = conn.table_details("main", "authors").unwrap();
    assert_eq!(d.columns.len(), 3);
    assert!(d.columns[0].primary_key && d.columns[0].name == "id");
    assert!(!d.columns[1].nullable && d.columns[1].data_type == "TEXT");
    assert_eq!(d.columns[2].default.as_deref(), Some("1900"));

    let b = conn.table_details("main", "books").unwrap();
    assert_eq!(b.foreign_keys.len(), 1);
    assert_eq!(b.foreign_keys[0].ref_table, "authors");
    assert_eq!(b.foreign_keys[0].columns, vec!["author_id".to_string()]);
    assert_eq!(b.foreign_keys[0].ref_columns, vec!["id".to_string()]);
    let idx = b.indexes.iter().find(|i| i.name == "books_title").unwrap();
    assert!(idx.unique && idx.columns == vec!["title".to_string()]);

    assert_eq!(conn.row_count("main", "books").unwrap(), 1234);
    assert!(conn.table_details("main", "missing").unwrap().columns.is_empty());
    assert!(conn.tables("nope").is_err());
}

struct Pages {
    columns: Vec<Column>,
    pages: Vec<Page>,
    stop_after: usize,
}

impl ResultSink for Pages {
    fn columns(&mut self, columns: &[Column]) {
        self.columns = columns.to_vec();
    }
    fn page(&mut self, page: Page) -> bool {
        self.pages.push(page);
        self.pages.len() < self.stop_after
    }
}

#[test]
fn paging_and_nulls() {
    let (_dir, mut conn) = setup();
    let mut sink = Pages { columns: Vec::new(), pages: Vec::new(), stop_after: usize::MAX };
    let sum = conn.execute("select id, title, note from books order by id", 500, &mut sink).unwrap();
    assert_eq!(sink.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "title", "note"]);
    assert_eq!(sink.columns[0].type_name, "INTEGER");
    let sizes: Vec<usize> = sink.pages.iter().map(|p| p.rows.len()).collect();
    assert_eq!(sizes, vec![500, 500, 234]);
    assert!(sink.pages.last().unwrap().last && !sink.pages[0].last);
    assert!(sum.complete && sum.rows_fetched == 1234 && sum.rows_affected.is_none());
    let first = &sink.pages[0].rows;
    assert_eq!(first[0][0], Cell::Int(1));
    assert_eq!(first[0][2], Cell::Text(String::new()));
    assert_eq!(first[2][2], Cell::Null);
    assert_ne!(first[0][2], first[2][2]);

    // Stopping after the first page leaves the rest unfetched.
    let mut sink = Pages { columns: Vec::new(), pages: Vec::new(), stop_after: 1 };
    let sum = conn.execute("select * from books", 500, &mut sink).unwrap();
    assert_eq!(sink.pages.len(), 1);
    assert!(!sum.complete && sum.rows_fetched == 500);

    // The connection is still usable afterwards.
    let (_, rows, _) = query_all(conn.as_mut(), "select count(*), sum(note is null) from books").unwrap();
    assert_eq!(rows, vec![vec![Cell::Int(1234), Cell::Int(411)]]);
}

#[test]
fn errors_and_affected_rows() {
    let (_dir, mut conn) = setup();
    let (cols, rows, sum) = query_all(conn.as_mut(), "update books set note = 'y' where id <= 10").unwrap();
    assert!(cols.is_empty() && rows.is_empty());
    assert_eq!(sum.rows_affected, Some(10));
    match query_all(conn.as_mut(), "select * from no_such_table") {
        Err(DbError::Query(msg)) => assert!(msg.contains("no such table"), "{msg}"),
        other => panic!("{other:?}"),
    }
    assert!(query_all(conn.as_mut(), "insert into books(title) values ('Book number 1')").is_err());
    let (_, rows, _) = query_all(conn.as_mut(), "select x'00ff', 1.5, null").unwrap();
    assert_eq!(rows[0], vec![Cell::Bytes(vec![0, 255]), Cell::Float(1.5), Cell::Null]);
}

#[test]
fn cancel_interrupts_a_running_query() {
    let (_dir, mut conn) = setup();
    let canceller = conn.canceller();
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let r = query_all(
            conn.as_mut(),
            "with recursive c(x) as (select 1 union all select x + 1 from c) select count(*) from c",
        );
        tx.send(()).unwrap();
        (r.map(|_| ()), started.elapsed(), conn)
    });
    std::thread::sleep(Duration::from_millis(200));
    canceller.cancel().unwrap();
    rx.recv_timeout(Duration::from_secs(10)).expect("query did not stop");
    let (r, elapsed, mut conn) = worker.join().unwrap();
    assert_eq!(r, Err(DbError::Cancelled));
    assert!(elapsed < Duration::from_secs(10));
    // Usable after the interrupt.
    let (_, rows, _) = query_all(conn.as_mut(), "select 1").unwrap();
    assert_eq!(rows, vec![vec![Cell::Int(1)]]);
}

#[test]
fn bad_files_fail_to_connect() {
    let dir = tempfile::tempdir().unwrap();
    let not_db = dir.path().join("x.txt");
    std::fs::write(&not_db, "this is not a database, it is long enough to have a header....... 1234567890").unwrap();
    assert!(matches!(connect(&config(&not_db), None), Err(DbError::Connect(_))));
    let mut ro = config(&dir.path().join("missing.db"));
    ro.read_only = true;
    assert!(connect(&ro, None).is_err());
    assert!(connect(&DataSourceConfig { kind: DbKind::Sqlite, ..Default::default() }, None).is_err());
}

#[test]
fn script_runs_statement_by_statement() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = connect(&config(&dir.path().join("s.db")), None).unwrap();
    let script = "create table t(a text); -- make it\ninsert into t values ('a;b'), (\"x\");\n\
                  create trigger tr after insert on t begin insert into t select 'z' where new.a = 'q'; end;\n\
                  insert into t values ('q'); select count(*) from t;";
    let stmts = split_statements(script, Dialect::Sqlite);
    assert_eq!(stmts.len(), 5);
    let mut last = Vec::new();
    for s in &stmts {
        last = query_all(conn.as_mut(), &s.text).unwrap().1;
    }
    assert_eq!(last, vec![vec![Cell::Int(4)]]);
}

#[test]
fn explain_query_plan_and_transaction_state() {
    let (_dir, mut conn) = setup();
    let plan = codecraft_db::explain::explain(
        conn.as_mut(),
        "select b.title, a.name from books b join authors a on a.id = b.author_id order by b.title;",
        true,
    )
    .unwrap();
    assert!(!plan.analyzed);
    assert!(plan.note.as_deref().is_some_and(|n| n.contains("cannot analyze")));
    let relations: Vec<_> = plan.roots.iter().filter_map(|n| n.relation.clone()).collect();
    assert!(relations.iter().any(|r| r == "b" || r == "a"), "{:?}", plan.roots);
    assert!(codecraft_db::explain::explain(conn.as_mut(), "select * from nope", false).is_err());

    assert_eq!(conn.in_transaction(), Some(false));
    query_all(conn.as_mut(), "begin").unwrap();
    assert_eq!(conn.in_transaction(), Some(true));
    query_all(conn.as_mut(), "rollback").unwrap();
    assert_eq!(conn.in_transaction(), Some(false));
}

#[test]
fn import_csv_in_one_transaction() {
    use codecraft_db::import::{ImportOptions, auto_mapping, insert_statements, read_file, run_import};
    let (dir, mut conn) = setup();
    let csv = dir.path().join("authors.csv");
    std::fs::write(&csv, "name,born\nCy,1950\n\"D, Jr.\",\nEve,1970\n").unwrap();
    let data = read_file(&csv, &ImportOptions::default()).unwrap();
    let table = vec!["id".to_string(), "name".to_string(), "born".to_string()];
    let mapping = auto_mapping(&data.columns, &table);
    assert_eq!(mapping, [None, Some(0), Some(1)]);
    let statements = insert_statements(DbKind::Sqlite, "main", "authors", &table, &mapping, &data, 2).unwrap();
    assert_eq!(statements.len(), 2);
    let mut seen = Vec::new();
    let n = run_import(conn.as_mut(), &statements, &mut |i| seen.push(i)).unwrap();
    assert_eq!((n, seen), (3, vec![1, 2]));
    let (_, rows, _) = query_all(conn.as_mut(), "select name, born from authors order by id").unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[3], [Cell::Text("D, Jr.".into()), Cell::Null]);
    assert_eq!(rows[4][1], Cell::Int(1970));

    // A failing batch (NOT NULL) rolls back everything.
    std::fs::write(&csv, "name\nZed\n\n\"\"\n").unwrap();
    let data = read_file(&csv, &ImportOptions::default()).unwrap();
    let statements =
        insert_statements(DbKind::Sqlite, "main", "authors", &table, &[None, Some(0), None], &data, 1).unwrap();
    let e = run_import(conn.as_mut(), &statements, &mut |_| {}).unwrap_err();
    assert!(e.to_string().contains("nothing was imported"), "{e}");
    let (_, rows, _) = query_all(conn.as_mut(), "select count(*) from authors").unwrap();
    assert_eq!(rows[0][0], Cell::Int(5));
    assert_eq!(conn.in_transaction(), Some(false));
    assert!(read_file(&dir.path().join("missing.csv"), &ImportOptions::default()).is_err());
}

#[test]
fn read_only_sqlite_rejects_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ro.db");
    query_all(connect(&config(&path), None).unwrap().as_mut(), "create table t (a)").unwrap();
    let mut cfg = config(&path);
    cfg.read_only = true;
    let mut conn = connect(&cfg, None).unwrap();
    assert!(query_all(conn.as_mut(), "insert into t values (1)").is_err());
    assert!(query_all(conn.as_mut(), "select * from t").is_ok());
}
