//! Schema tools against real databases: DDL generation and execution (create, alter, the SQLite
//! rebuild, drop/rename/truncate), catalog introspection, Copy DDL and schema comparison with
//! migrations. SQLite in temp dirs always; PostgreSQL only when a local server accepts
//! connections (a throwaway schema, dropped at the end).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use codecraft_db::catalog::{object_ddl, schema_ddl, schema_model, table_def};
use codecraft_db::ddl::{
    CheckDef, ColumnDef, DdlScript, FkAction, ForeignKeyDef, IndexDef, ObjectKind, ObjectRef, TableDef, alter_table,
    create_table, drop_object, rename_object, run_script, truncate_table,
};
use codecraft_db::schema_diff::{diff, migration};
use codecraft_db::{Cell, Connection, DataSourceConfig, DbKind, connect, query_all};

fn sqlite(path: &std::path::Path) -> Box<dyn Connection> {
    connect(&DataSourceConfig { kind: DbKind::Sqlite, path: Some(path.to_path_buf()), ..Default::default() }, None)
        .unwrap()
}

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef { name: name.into(), data_type: ty.into(), ..Default::default() }
}

fn rows(c: &mut dyn Connection, sql: &str) -> Vec<Vec<Cell>> {
    query_all(c, sql).unwrap_or_else(|e| panic!("{sql}: {e}")).1
}

fn run(c: &mut dyn Connection, s: &DdlScript) {
    run_script(c, s).unwrap_or_else(|e| panic!("{e}\n{}", s.to_sql()));
}

fn library(schema: &str, int: &str) -> (TableDef, TableDef) {
    let authors = TableDef {
        schema: schema.into(),
        name: "authors".into(),
        columns: vec![
            ColumnDef { nullable: false, auto_increment: true, ..col("id", int) },
            ColumnDef { nullable: false, comment: "full name".into(), ..col("name", "varchar(100)") },
        ],
        primary_key: vec!["id".into()],
        comment: "Writers".into(),
        ..Default::default()
    };
    let books = TableDef {
        schema: schema.into(),
        name: "books".into(),
        columns: vec![
            ColumnDef { nullable: false, ..col("id", int) },
            col("author_id", int),
            ColumnDef { nullable: false, ..col("title", "varchar(200)") },
            ColumnDef { default: "0".into(), ..col("price", "numeric(10, 2)") },
        ],
        primary_key: vec!["id".into()],
        indexes: vec![IndexDef { name: "books_title".into(), columns: vec!["title".into()], ..Default::default() }],
        foreign_keys: vec![ForeignKeyDef {
            name: "books_author".into(),
            columns: vec!["author_id".into()],
            ref_table: "authors".into(),
            ref_columns: vec!["id".into()],
            on_delete: FkAction::Cascade,
            ..Default::default()
        }],
        checks: vec![CheckDef { name: "price_positive".into(), expression: "price >= 0".into() }],
        ..Default::default()
    };
    (authors, books)
}

#[test]
fn sqlite_create_introspect_alter_and_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let mut conn = sqlite(&dir.path().join("lib.db"));
    let c = conn.as_mut();
    let (authors, books) = library("main", "INTEGER");
    run(c, &create_table(DbKind::Sqlite, &authors).unwrap());
    run(c, &create_table(DbKind::Sqlite, &books).unwrap());
    for sql in [
        "INSERT INTO authors(name) VALUES ('Ann'), ('Bob')",
        "INSERT INTO books VALUES (1, 1, 'Dune', 9.5), (2, 2, 'Emma', 0), (3, NULL, 'Odd', 1)",
        "CREATE VIEW cheap AS SELECT title FROM books WHERE price < 5",
        "CREATE TRIGGER books_ai AFTER INSERT ON books BEGIN UPDATE books SET price = price WHERE id = new.id; END",
    ] {
        rows(c, sql);
    }

    // Introspection sees everything the dialog edits.
    let a = table_def(c, "main", "authors").unwrap();
    assert!(a.columns[0].auto_increment, "{a:?}");
    assert_eq!(a.primary_key, vec!["id"]);
    let b = table_def(c, "main", "books").unwrap();
    assert_eq!(b.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "author_id", "title", "price"]);
    assert!(!b.columns[2].nullable && b.columns[3].default == "0");
    assert_eq!(b.foreign_keys.len(), 1);
    assert_eq!(b.foreign_keys[0].on_delete, FkAction::Cascade);
    assert_eq!(
        (b.foreign_keys[0].ref_table.as_str(), b.foreign_keys[0].ref_columns.clone()),
        ("authors", vec!["id".to_string()])
    );
    assert_eq!(b.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["books_title"]);
    assert_eq!(b.checks, vec![CheckDef { name: "price_positive".into(), expression: "price >= 0".into() }]);
    assert_eq!(b.triggers.len(), 1);

    // An unchanged definition alters nothing.
    assert!(alter_table(DbKind::Sqlite, &b, &b.clone().for_modify()).unwrap().is_empty());

    // A type change and a renamed column rebuild the table and keep rows, index, trigger, view.
    let mut new = b.clone().for_modify();
    new.columns[3].data_type = "REAL".into();
    new.columns[2].name = "book title".into();
    new.columns.push(ColumnDef { default: "'n/a'".into(), ..col("isbn", "TEXT") });
    let script = alter_table(DbKind::Sqlite, &b, &new).unwrap();
    assert_eq!(script.setup, ["PRAGMA foreign_keys = OFF", "PRAGMA legacy_alter_table = OFF"]);
    run(c, &script);
    let after = table_def(c, "main", "books").unwrap();
    assert_eq!(after.columns[2].name, "book title");
    assert_eq!(after.columns[3].data_type, "REAL");
    assert_eq!(after.indexes[0].columns, vec!["book title"]);
    assert_eq!(after.triggers.len(), 1);
    assert_eq!(after.foreign_keys[0].on_delete, FkAction::Cascade);
    assert_eq!(rows(c, "SELECT count(*) FROM books"), vec![vec![Cell::Int(3)]]);
    assert_eq!(rows(c, "SELECT isbn FROM books WHERE id = 1"), vec![vec![Cell::Text("n/a".into())]]);
    assert_eq!(rows(c, "PRAGMA foreign_keys"), vec![vec![Cell::Int(1)]], "foreign keys are back on");
    assert_eq!(rows(c, "PRAGMA legacy_alter_table"), vec![vec![Cell::Int(0)]]);
    rows(c, "SELECT * FROM cheap");
    // Foreign keys still hold after the rebuild.
    assert!(query_all(c, "INSERT INTO books(id, author_id, \"book title\") VALUES (9, 99, 'x')").is_err());
    rows(c, "DELETE FROM authors WHERE id = 2");
    assert_eq!(rows(c, "SELECT count(*) FROM books"), vec![vec![Cell::Int(2)]], "ON DELETE CASCADE");

    // A change that fails half way leaves nothing behind: NOT NULL on a column with NULLs.
    let before = table_def(c, "main", "books").unwrap();
    let mut bad = before.clone().for_modify();
    bad.columns[1].nullable = false;
    let script = alter_table(DbKind::Sqlite, &before, &bad).unwrap();
    let err = run_script(c, &script).unwrap_err();
    assert!(err.rolled_back, "{err}");
    assert!(err.message().contains("NOT NULL"), "{}", err.message());
    assert!(err.message().contains("rolled back"));
    assert_eq!(table_def(c, "main", "books").unwrap(), before);
    assert!(rows(c, "SELECT name FROM sqlite_master WHERE name LIKE 'codecraft_new%'").is_empty());
    assert_eq!(rows(c, "PRAGMA foreign_keys"), vec![vec![Cell::Int(1)]]);

    // In place: rename table, add a plain column; then rename, drop, truncate.
    let a = table_def(c, "main", "authors").unwrap();
    let mut new = a.clone().for_modify();
    new.name = "writers".into();
    new.columns.push(col("born", "INTEGER"));
    let script = alter_table(DbKind::Sqlite, &a, &new).unwrap();
    assert!(script.setup.is_empty(), "{}", script.to_sql());
    run(c, &script);
    let fk = table_def(c, "main", "books").unwrap().foreign_keys[0].clone();
    assert_eq!(fk.ref_table, "writers", "SQLite follows the rename in references");
    let o = |kind, name: &str, table: &str| ObjectRef {
        kind,
        schema: "main".into(),
        name: name.into(),
        table: table.into(),
    };
    run(c, &rename_object(DbKind::Sqlite, &o(ObjectKind::Column, "born", "writers"), "year").unwrap());
    run(c, &drop_object(DbKind::Sqlite, &o(ObjectKind::Column, "year", "writers"), false).unwrap());
    assert!(table_def(c, "main", "writers").unwrap().column("year").is_none());
    run(c, &drop_object(DbKind::Sqlite, &o(ObjectKind::Index, "books_title", "books"), false).unwrap());
    run(c, &truncate_table(DbKind::Sqlite, "main", "books", false).unwrap());
    assert_eq!(rows(c, "SELECT count(*) FROM books"), vec![vec![Cell::Int(0)]]);
    run(c, &drop_object(DbKind::Sqlite, &o(ObjectKind::View, "cheap", ""), false).unwrap());
    run(c, &drop_object(DbKind::Sqlite, &o(ObjectKind::Table, "books", ""), false).unwrap());
    assert!(table_def(c, "main", "books").is_err());
}

#[test]
fn sqlite_copy_ddl_and_compare() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = sqlite(&dir.path().join("src.db"));
    let mut dst = sqlite(&dir.path().join("dst.db"));
    let (authors, books) = library("main", "INTEGER");
    run(src.as_mut(), &create_table(DbKind::Sqlite, &authors).unwrap());
    run(src.as_mut(), &create_table(DbKind::Sqlite, &books).unwrap());
    rows(src.as_mut(), "CREATE VIEW titles AS SELECT title FROM books");
    rows(src.as_mut(), "CREATE INDEX books_price ON books(price) WHERE price > 0");

    let ddl = object_ddl(
        src.as_mut(),
        &ObjectRef { kind: ObjectKind::Table, schema: "main".into(), name: "books".into(), table: String::new() },
    )
    .unwrap();
    // SQLite keeps statements without the schema name.
    assert!(ddl.starts_with("CREATE TABLE \"books\" (\n"), "{ddl}");
    assert!(ddl.contains("CREATE INDEX \"books_title\" ON \"books\"") && ddl.contains("books_price"), "{ddl}");
    let ddl = object_ddl(
        src.as_mut(),
        &ObjectRef { kind: ObjectKind::View, schema: "main".into(), name: "titles".into(), table: String::new() },
    )
    .unwrap();
    assert_eq!(ddl, "CREATE VIEW titles AS SELECT title FROM books;\n");
    let all = schema_ddl(src.as_mut(), "main").unwrap();
    assert!(all.find("\"authors\"").unwrap() < all.find("\"books\"").unwrap(), "{all}");

    // The empty database becomes like the source; then nothing differs.
    let s = schema_model(src.as_mut(), "main", None).unwrap();
    let t = schema_model(dst.as_mut(), "main", None).unwrap();
    let d = diff(&s, &t);
    assert_eq!(d.counts(), (3, 0, 0), "{d:#?}");
    let m = migration(&s, &t).unwrap();
    run(dst.as_mut(), &m);
    let t = schema_model(dst.as_mut(), "main", None).unwrap();
    assert!(diff(&s, &t).is_empty(), "{:#?}\n{}", diff(&s, &t), m.to_sql());

    // Change both sides; the migration brings the target back in line.
    rows(dst.as_mut(), "ALTER TABLE books ADD COLUMN extra TEXT");
    rows(dst.as_mut(), "CREATE TABLE junk (x)");
    rows(src.as_mut(), "CREATE TABLE fresh (id INTEGER PRIMARY KEY, book_id INTEGER REFERENCES books(id))");
    let s = schema_model(src.as_mut(), "main", None).unwrap();
    let t = schema_model(dst.as_mut(), "main", None).unwrap();
    assert_eq!(diff(&s, &t).counts(), (1, 1, 1));
    run(dst.as_mut(), &migration(&s, &t).unwrap());
    let t = schema_model(dst.as_mut(), "main", None).unwrap();
    assert!(diff(&s, &t).is_empty(), "{:#?}", diff(&s, &t));
}

// ---------------------------------------------------------------- PostgreSQL

fn pg() -> Option<Box<dyn Connection>> {
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let mut cfg = DataSourceConfig {
        id: "pg".into(),
        name: "pg".into(),
        kind: DbKind::Postgres,
        host: env("CODECRAFT_TEST_PG_HOST", "localhost"),
        port: env("CODECRAFT_TEST_PG_PORT", "5432").parse().ok(),
        user: env("CODECRAFT_TEST_PG_USER", &std::env::var("USER").unwrap_or_default()),
        database: env("CODECRAFT_TEST_PG_DB", "postgres"),
        ..Default::default()
    };
    cfg.options.insert("connect_timeout".into(), "3".into());
    match connect(&cfg, std::env::var("CODECRAFT_TEST_PG_PASSWORD").ok().as_deref()) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("skipping: no PostgreSQL server ({e})");
            None
        }
    }
}

struct Schemas(Box<dyn Connection>, Vec<String>);

impl Drop for Schemas {
    fn drop(&mut self) {
        for s in &self.1 {
            let _ = query_all(self.0.as_mut(), &format!("DROP SCHEMA IF EXISTS \"{s}\" CASCADE"));
        }
    }
}

#[test]
fn postgres_schema_tools() {
    let Some(conn) = pg() else { return };
    let a = format!("cc_ddl_a_{}", std::process::id());
    let b = format!("cc_ddl_b \"{}\"", std::process::id());
    let mut g = Schemas(conn, vec![a.clone(), b.replace('"', "\"\"")]);
    let c = g.0.as_mut();
    for s in [&a, &b] {
        let q = codecraft_db::sql::quote_ident(DbKind::Postgres, s);
        rows(c, &format!("DROP SCHEMA IF EXISTS {q} CASCADE"));
        rows(c, &format!("CREATE SCHEMA {q}"));
    }
    let (authors, mut books) = library(&a, "integer");
    books.indexes.push(IndexDef {
        name: "books_uq".into(),
        columns: vec!["author_id".into(), "title".into()],
        unique: true,
        constraint: true,
        ..Default::default()
    });
    run(c, &create_table(DbKind::Postgres, &authors).unwrap());
    run(c, &create_table(DbKind::Postgres, &books).unwrap());
    rows(c, &format!("INSERT INTO \"{a}\".authors(name) VALUES ('Ann'), ('Bob')"));
    rows(c, &format!("INSERT INTO \"{a}\".books VALUES (1, 1, 'Dune', 9.5), (2, 2, 'x1', 0)"));
    rows(c, &format!("CREATE VIEW \"{a}\".cheap AS SELECT title FROM \"{a}\".books WHERE price < 5"));

    let got = table_def(c, &a, "authors").unwrap();
    assert!(got.columns[0].auto_increment, "identity: {got:?}");
    assert_eq!(got.comment, "Writers");
    assert_eq!(got.columns[1].comment, "full name");
    assert_eq!(got.primary_key, vec!["id"]);
    let got = table_def(c, &a, "books").unwrap();
    assert_eq!(got.foreign_keys[0].on_delete, FkAction::Cascade);
    assert_eq!(got.foreign_keys[0].ref_schema, "", "same schema");
    assert_eq!(got.checks[0].name, "price_positive");
    assert!(got.indexes.iter().any(|i| i.name == "books_title" && i.columns == vec!["title"] && !i.unique));
    assert!(got.indexes.iter().any(|i| i.name == "books_uq" && i.constraint && i.unique));
    // Introspected definitions round-trip: nothing to alter.
    let none = alter_table(DbKind::Postgres, &got, &got.clone().for_modify()).unwrap();
    assert!(none.is_empty(), "{}", none.to_sql());

    // Alter: rename, type change, new column, dropped check, new FK action.
    let mut new = got.clone().for_modify();
    new.columns[2].name = "book title".into();
    new.columns[3].data_type = "numeric(12, 3)".into();
    new.columns.push(ColumnDef { default: "now()".into(), ..col("added", "timestamp") });
    new.checks.clear();
    new.foreign_keys[0].on_delete = FkAction::SetNull;
    run(c, &alter_table(DbKind::Postgres, &got, &new).unwrap());
    let after = table_def(c, &a, "books").unwrap();
    assert_eq!(after.columns[2].name, "book title");
    assert_eq!(after.columns[3].data_type, "numeric(12,3)");
    assert_eq!(after.foreign_keys[0].on_delete, FkAction::SetNull);
    assert!(after.checks.is_empty());
    assert_eq!(after.indexes.iter().find(|i| i.name == "books_title").unwrap().columns, vec!["book title"]);

    // A failing statement rolls back the earlier ones: nothing changes.
    let mut bad = after.clone().for_modify();
    bad.columns[0].name = "renamed_first".into();
    bad.columns[2].data_type = "integer".into(); // 'Dune' is not a number
    let err = run_script(c, &alter_table(DbKind::Postgres, &after, &bad).unwrap()).unwrap_err();
    assert!(err.rolled_back && err.index.is_some(), "{err}");
    assert!(err.error.contains("invalid input syntax"), "{err}");
    assert_eq!(table_def(c, &a, "books").unwrap(), after);

    // Copy DDL.
    let o =
        |kind, name: &str, table: &str| ObjectRef { kind, schema: a.clone(), name: name.into(), table: table.into() };
    let ddl = object_ddl(c, &o(ObjectKind::Table, "books", "")).unwrap();
    assert!(ddl.contains(&format!("CREATE TABLE \"{a}\".\"books\"")) && ddl.contains("ON DELETE SET NULL"), "{ddl}");
    assert!(ddl.contains("CONSTRAINT \"books_uq\" UNIQUE (\"author_id\", \"book title\")"), "{ddl}");
    let ddl = object_ddl(c, &o(ObjectKind::View, "cheap", "")).unwrap();
    assert!(ddl.starts_with(&format!("CREATE OR REPLACE VIEW \"{a}\".\"cheap\" AS")), "{ddl}");
    let ddl = object_ddl(c, &o(ObjectKind::Index, "books_title", "books")).unwrap();
    assert!(ddl.starts_with("CREATE INDEX books_title ON"), "{ddl}");
    let ddl = object_ddl(c, &o(ObjectKind::ForeignKey, "books_author", "books")).unwrap();
    assert!(ddl.contains("ADD CONSTRAINT \"books_author\" FOREIGN KEY"), "{ddl}");
    let all = schema_ddl(c, &a).unwrap();
    assert!(all.starts_with(&format!("CREATE SCHEMA \"{a}\";")), "{all}");

    // Compare schema a with the empty (and awkwardly named) schema b; migrate b; equal.
    let sa = schema_model(c, &a, None).unwrap();
    let sb = schema_model(c, &b, None).unwrap();
    assert_eq!(diff(&sa, &sb).counts(), (3, 0, 0));
    let m = migration(&sa, &sb).unwrap();
    run(c, &m);
    let sb = schema_model(c, &b, None).unwrap();
    assert!(diff(&sa, &sb).is_empty(), "{:#?}\n{}", diff(&sa, &sb), m.to_sql());

    // Into SQLite (another dialect): the migration runs and the schemas compare equal.
    let dir = tempfile::tempdir().unwrap();
    let mut lite = sqlite(&dir.path().join("x.db"));
    let tl = schema_model(lite.as_mut(), "main", None).unwrap();
    let mut sa_tables = sa.clone();
    sa_tables.views.clear();
    let m = migration(&sa_tables, &tl).unwrap();
    run(lite.as_mut(), &m);
    let tl = schema_model(lite.as_mut(), "main", None).unwrap();
    assert!(diff(&sa_tables, &tl).is_empty(), "{:#?}\n{}", diff(&sa_tables, &tl), m.to_sql());

    // Rename, truncate, drop with cascade.
    run(c, &rename_object(DbKind::Postgres, &o(ObjectKind::Table, "authors", ""), "writers").unwrap());
    run(c, &truncate_table(DbKind::Postgres, &a, "writers", true).unwrap());
    assert_eq!(rows(c, &format!("SELECT count(*) FROM \"{a}\".books")), vec![vec![Cell::Int(0)]]);
    run(c, &drop_object(DbKind::Postgres, &o(ObjectKind::Schema, &a, ""), true).unwrap());
    g.1.retain(|s| *s != a);
}
