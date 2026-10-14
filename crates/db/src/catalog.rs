//! Catalog introspection for the schema tools: complete table definitions ([`TableDef`]: identity
//! and auto-increment, comments, foreign key actions, checks, index definitions), view
//! definitions, whole-schema models for diagrams and comparison, and the DDL of any object
//! (DataGrip's SQL Generator / Copy DDL).
//!
//! PostgreSQL is read from `pg_catalog`, MySQL from `information_schema` and `SHOW CREATE …`,
//! SQLite from `sqlite_master` and its pragmas. Names are passed as string literals escaped for the
//! dialect ([`crate::edit::literal`]), never pasted into the SQL.

use crate::ddl::{
    CheckDef, ColumnDef, DependentView, FkAction, ForeignKeyDef, IndexDef, ObjectKind, ObjectRef, TableDef, column_sql,
    create_index, foreign_key_clause, join_statements, table_ddl,
};
use crate::edit::literal;
use crate::schema_diff::{SchemaModel, ViewDef, dependency_order};
use crate::sql::{qualified, quote_ident};
use crate::{Cell, Connection, DbError, DbKind, DbResult, TableKind, query_all};

type Row = Vec<Option<String>>;

fn text_of(c: Cell) -> Option<String> {
    match c {
        Cell::Null => None,
        Cell::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        other => Some(other.display()),
    }
}

fn rows(conn: &mut dyn Connection, sql: &str) -> DbResult<Vec<Row>> {
    let (_, rows, _) = query_all(conn, sql)?;
    Ok(rows.into_iter().map(|r| r.into_iter().map(text_of).collect()).collect())
}

fn get(row: &Row, i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn opt(row: &Row, i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

fn truthy(row: &Row, i: usize) -> bool {
    matches!(get(row, i).to_ascii_lowercase().as_str(), "t" | "true" | "1" | "yes")
}

fn lit(kind: DbKind, s: &str) -> DbResult<String> {
    literal(kind, &Cell::Text(s.to_string()))
}

/// The complete definition of a table.
pub fn table_def(conn: &mut dyn Connection, schema: &str, table: &str) -> DbResult<TableDef> {
    let t = match conn.kind() {
        DbKind::Postgres => pg_table(conn, schema, table)?,
        DbKind::Mysql | DbKind::Mariadb => my_table(conn, schema, table)?,
        DbKind::Sqlite => sqlite_table(conn, schema, table)?,
        DbKind::SqlServer | DbKind::ClickHouse | DbKind::DuckDb => generic_table(conn, schema, table)?,
        DbKind::Redis => return Err(DbError::Unsupported("Redis has no tables".into())),
    };
    if t.columns.is_empty() {
        return Err(DbError::Query(format!("no table {}", qualified(conn.kind(), schema, table))));
    }
    Ok(t)
}

/// A definition from the driver's [`Connection::table_details`] (SQL Server, ClickHouse, DuckDB):
/// columns, primary key, indexes and foreign keys without actions, checks or comments.
fn generic_table(conn: &mut dyn Connection, schema: &str, table: &str) -> DbResult<TableDef> {
    let d = conn.table_details(schema, table)?;
    let mut def = TableDef { schema: schema.into(), name: table.into(), ..Default::default() };
    for c in &d.columns {
        let identity = c.data_type.ends_with(" identity");
        def.columns.push(ColumnDef {
            name: c.name.clone(),
            data_type: c.data_type.trim_end_matches(" identity").to_string(),
            nullable: c.nullable,
            default: c.default.clone().unwrap_or_default(),
            auto_increment: identity,
            original: c.name.clone(),
            ..Default::default()
        });
    }
    if let Some(pk) = d.indexes.iter().find(|i| i.primary) {
        def.primary_key = pk.columns.clone();
        def.primary_key_name = pk.name.clone();
    } else {
        def.primary_key = d.columns.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    }
    for i in d.indexes.iter().filter(|i| !i.primary) {
        def.indexes.push(IndexDef {
            name: i.name.clone(),
            columns: i.columns.clone(),
            unique: i.unique,
            ..Default::default()
        });
    }
    for f in &d.foreign_keys {
        def.foreign_keys.push(ForeignKeyDef {
            name: f.name.clone(),
            columns: f.columns.clone(),
            ref_schema: f.ref_schema.clone().unwrap_or_default(),
            ref_table: f.ref_table.clone(),
            ref_columns: f.ref_columns.clone(),
            ..Default::default()
        });
    }
    Ok(def)
}

// ---------------------------------------------------------------- PostgreSQL

fn pg_table(conn: &mut dyn Connection, schema: &str, table: &str) -> DbResult<TableDef> {
    let k = DbKind::Postgres;
    let (s, t) = (lit(k, schema)?, lit(k, table)?);
    let rel = format!(
        "FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {s} AND c.relname = {t}"
    );
    let mut def = TableDef { schema: schema.into(), name: table.into(), ..Default::default() };
    def.comment = rows(conn, &format!("SELECT obj_description(c.oid, 'pg_class') {rel}"))?
        .first()
        .map(|r| get(r, 0))
        .unwrap_or_default();
    for r in rows(
        conn,
        &format!(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                    pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, col_description(c.oid, a.attnum) \
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
             WHERE n.nspname = {s} AND c.relname = {t} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
        ),
    )? {
        def.columns.push(ColumnDef {
            name: get(&r, 0),
            data_type: get(&r, 1),
            nullable: !truthy(&r, 2),
            default: get(&r, 3),
            auto_increment: matches!(get(&r, 4).as_str(), "a" | "d"),
            comment: get(&r, 5),
            original: String::new(),
        });
    }
    // Primary key, unique constraints and foreign keys, one row per column.
    let mut uniques: Vec<String> = Vec::new();
    for r in rows(
        conn,
        &format!(
            "SELECT con.conname::text, con.contype::text, a.attname::text, con.confdeltype::text, \
                    con.confupdtype::text, fn.nspname::text, fc.relname::text, fa.attname::text \
             FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             CROSS JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS k(num, ord) \
             JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.num \
             LEFT JOIN pg_class fc ON fc.oid = con.confrelid \
             LEFT JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
             LEFT JOIN pg_attribute fa ON fa.attrelid = con.confrelid AND fa.attnum = con.confkey[k.ord] \
             WHERE n.nspname = {s} AND c.relname = {t} AND con.contype IN ('p', 'f', 'u') \
             ORDER BY con.contype, con.conname, k.ord"
        ),
    )? {
        let name = get(&r, 0);
        match get(&r, 1).as_str() {
            "p" => {
                def.primary_key_name = name;
                def.primary_key.push(get(&r, 2));
            }
            "u" => uniques.push(name),
            "f" => {
                if def.foreign_keys.last().is_none_or(|f| f.name != name) {
                    let ref_schema = get(&r, 5);
                    def.foreign_keys.push(ForeignKeyDef {
                        name: name.clone(),
                        ref_schema: if ref_schema == schema { String::new() } else { ref_schema },
                        ref_table: get(&r, 6),
                        on_delete: FkAction::parse(&get(&r, 3)).unwrap_or_default(),
                        on_update: FkAction::parse(&get(&r, 4)).unwrap_or_default(),
                        ..Default::default()
                    });
                }
                if let Some(f) = def.foreign_keys.last_mut() {
                    f.columns.push(get(&r, 2));
                    f.ref_columns.push(get(&r, 7));
                }
            }
            _ => {}
        }
    }
    for r in rows(
        conn,
        &format!(
            "SELECT con.conname::text, pg_get_constraintdef(con.oid, true) FROM pg_constraint con \
             JOIN pg_class c ON c.oid = con.conrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = {s} AND c.relname = {t} AND con.contype = 'c' ORDER BY con.conname"
        ),
    )? {
        def.checks.push(CheckDef { name: get(&r, 0), expression: check_expression(&get(&r, 1)) });
    }
    // Indexes other than the primary key's; expression, partial and non-btree indexes keep their
    // catalog definition.
    for r in rows(
        conn,
        &format!(
            "SELECT ic.relname::text, i.indisunique, pg_get_indexdef(i.indexrelid), \
                    (i.indexprs IS NULL AND i.indpred IS NULL AND am.amname = 'btree'), \
                    (SELECT string_agg(quote_ident(a.attname), ',' ORDER BY k.ord) \
                     FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(num, ord) \
                     JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.num \
                     WHERE k.ord <= i.indnkeyatts) \
             FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid \
             JOIN pg_class c ON c.oid = i.indrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
             JOIN pg_am am ON am.oid = ic.relam \
             WHERE n.nspname = {s} AND c.relname = {t} AND NOT i.indisprimary ORDER BY ic.relname"
        ),
    )? {
        let name = get(&r, 0);
        let simple = truthy(&r, 3);
        let columns = if simple { split_quoted_list(&get(&r, 4)) } else { Vec::new() };
        def.indexes.push(IndexDef {
            constraint: uniques.contains(&name),
            name,
            unique: truthy(&r, 1),
            columns,
            definition: get(&r, 2),
        });
    }
    for r in rows(
        conn,
        &format!(
            "SELECT DISTINCT vn.nspname::text, v.relname::text, v.relkind::text, pg_get_viewdef(v.oid, true) \
             FROM pg_depend d JOIN pg_rewrite r ON r.oid = d.objid JOIN pg_class v ON v.oid = r.ev_class \
             JOIN pg_namespace vn ON vn.oid = v.relnamespace \
             WHERE d.classid = 'pg_rewrite'::regclass AND d.refclassid = 'pg_class'::regclass \
               AND d.refobjid = (SELECT c.oid {rel}) AND v.oid <> d.refobjid \
             ORDER BY 1, 2"
        ),
    )? {
        let view = ViewDef {
            name: get(&r, 1),
            materialized: get(&r, 2) == "m",
            query: get(&r, 3).trim().trim_end_matches(';').trim().to_string(),
        };
        let vs = get(&r, 0);
        let what = if view.materialized { "MATERIALIZED VIEW" } else { "VIEW" };
        def.dependent_views.push(DependentView {
            name: format!("{vs}.{}", view.name),
            drop: format!("DROP {what} {}", qualified(k, &vs, &view.name)),
            create: view_ddl(k, &vs, &view).trim_end().trim_end_matches(';').replace("CREATE OR REPLACE", "CREATE"),
        });
    }
    Ok(def)
}

/// `CHECK ((price > 0))` → `(price > 0)`.
fn check_expression(def: &str) -> String {
    let d = def.trim();
    let body = if d.len() >= 5 && d.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("check")) {
        d.get(5..).unwrap_or_default().trim()
    } else {
        d
    };
    // Drop the outer parentheses CHECK adds and NOT VALID / NO INHERIT suffixes.
    let mut depth = 0i32;
    let mut end = None;
    for (i, c) in body.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    match (body.starts_with('('), end) {
        (true, Some(e)) => body.get(1..e).unwrap_or(body).trim().to_string(),
        _ => body.to_string(),
    }
}

/// `a,"b c","d""e"` (as `quote_ident` writes it) → names.
fn split_quoted_list(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                cur.push('"');
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || !out.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------- MySQL / MariaDB

fn is_mariadb(conn: &mut dyn Connection) -> bool {
    conn.kind() == DbKind::Mariadb || conn.server_version().is_ok_and(|v| v.to_ascii_lowercase().contains("mariadb"))
}

/// MySQL 8 reports literal defaults unquoted; MariaDB quotes them. Turn either into SQL.
fn my_default(raw: Option<String>, extra: &str, data_type: &str, mariadb: bool) -> String {
    let Some(d) = raw else { return String::new() };
    if mariadb {
        return if d.eq_ignore_ascii_case("NULL") { String::new() } else { d };
    }
    let extra = extra.to_ascii_lowercase();
    if extra.contains("default_generated") {
        return d;
    }
    let numeric = ["int", "decimal", "numeric", "float", "double", "real", "bit", "bool", "year"].iter().any(|t| {
        data_type.to_ascii_lowercase().starts_with(t) || data_type.to_ascii_lowercase().contains(&format!(" {t}"))
    });
    if numeric && (d.parse::<f64>().is_ok() || d.starts_with("b'")) {
        return d;
    }
    if d.eq_ignore_ascii_case("CURRENT_TIMESTAMP") {
        return d;
    }
    literal(DbKind::Mysql, &Cell::Text(d.clone())).unwrap_or(d)
}

fn my_table(conn: &mut dyn Connection, schema: &str, table: &str) -> DbResult<TableDef> {
    let k = conn.kind();
    let mariadb = is_mariadb(conn);
    let (s, t) = (lit(k, schema)?, lit(k, table)?);
    let mut def = TableDef { schema: schema.into(), name: table.into(), ..Default::default() };
    def.comment = rows(
        conn,
        &format!("SELECT TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = {s} AND TABLE_NAME = {t}"),
    )?
    .first()
    .map(|r| get(r, 0))
    .unwrap_or_default();
    for r in rows(
        conn,
        &format!(
            "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT, EXTRA, COLUMN_COMMENT \
             FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = {s} AND TABLE_NAME = {t} ORDER BY ORDINAL_POSITION"
        ),
    )? {
        let extra = get(&r, 4);
        let data_type = get(&r, 1);
        def.columns.push(ColumnDef {
            name: get(&r, 0),
            default: my_default(opt(&r, 3), &extra, &data_type, mariadb),
            data_type,
            nullable: get(&r, 2) == "YES",
            auto_increment: extra.to_ascii_lowercase().contains("auto_increment"),
            comment: get(&r, 5),
            original: String::new(),
        });
    }
    for r in rows(
        conn,
        &format!(
            "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = {s} AND TABLE_NAME = {t} ORDER BY INDEX_NAME, SEQ_IN_INDEX"
        ),
    )? {
        let name = get(&r, 0);
        let column = opt(&r, 2);
        if name == "PRIMARY" {
            def.primary_key.extend(column);
            continue;
        }
        if def.indexes.last().is_none_or(|i| i.name != name) {
            def.indexes.push(IndexDef { unique: get(&r, 1) == "0", name, ..Default::default() });
        }
        if let Some(i) = def.indexes.last_mut() {
            match column {
                Some(c) => i.columns.push(c),
                // A functional key part: keep the index as SHOW CREATE TABLE has it.
                None => i.definition = "(expression)".into(),
            }
        }
    }
    def.indexes.retain(|i| i.definition.is_empty());
    for r in rows(
        conn,
        &format!(
            "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME, \
                    k.REFERENCED_COLUMN_NAME, r.UPDATE_RULE, r.DELETE_RULE \
             FROM information_schema.KEY_COLUMN_USAGE k \
             JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA \
                  AND r.CONSTRAINT_NAME = k.CONSTRAINT_NAME AND r.TABLE_NAME = k.TABLE_NAME \
             WHERE k.TABLE_SCHEMA = {s} AND k.TABLE_NAME = {t} AND k.REFERENCED_TABLE_NAME IS NOT NULL \
             ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION"
        ),
    )? {
        let name = get(&r, 0);
        if def.foreign_keys.last().is_none_or(|f| f.name != name) {
            let ref_schema = get(&r, 2);
            def.foreign_keys.push(ForeignKeyDef {
                name,
                ref_schema: if ref_schema == schema { String::new() } else { ref_schema },
                ref_table: get(&r, 3),
                on_update: FkAction::parse(&get(&r, 5)).unwrap_or_default(),
                on_delete: FkAction::parse(&get(&r, 6)).unwrap_or_default(),
                ..Default::default()
            });
        }
        if let Some(f) = def.foreign_keys.last_mut() {
            f.columns.push(get(&r, 1));
            f.ref_columns.push(get(&r, 4));
        }
    }
    // CHECK_CONSTRAINTS exists from MySQL 8.0.16 and MariaDB 10.2; older servers have no checks.
    if let Ok(checks) = rows(
        conn,
        &format!(
            "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc \
             JOIN information_schema.TABLE_CONSTRAINTS tc ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA \
                  AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME \
             WHERE tc.TABLE_SCHEMA = {s} AND tc.TABLE_NAME = {t} AND tc.CONSTRAINT_TYPE = 'CHECK' \
             ORDER BY cc.CONSTRAINT_NAME"
        ),
    ) {
        for r in checks {
            def.checks.push(CheckDef { name: get(&r, 0), expression: check_expression(&get(&r, 1)) });
        }
    }
    Ok(def)
}

// ---------------------------------------------------------------- SQLite

fn master(kind: DbKind, schema: &str) -> String {
    if schema.is_empty() { "sqlite_master".into() } else { format!("{}.sqlite_master", quote_ident(kind, schema)) }
}

fn sqlite_table(conn: &mut dyn Connection, schema: &str, table: &str) -> DbResult<TableDef> {
    let k = DbKind::Sqlite;
    let (s, t) = (lit(k, if schema.is_empty() { "main" } else { schema })?, lit(k, table)?);
    let mut def = TableDef { schema: schema.into(), name: table.into(), ..Default::default() };
    let sql = rows(conn, &format!("SELECT sql FROM {} WHERE type = 'table' AND name = {t}", master(k, schema)))?
        .first()
        .map(|r| get(r, 0))
        .unwrap_or_default();
    let mut pk: Vec<(i64, String)> = Vec::new();
    for r in rows(
        conn,
        &format!(
            "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_xinfo({t}, {s}) WHERE hidden <> 1 ORDER BY cid"
        ),
    )? {
        let name = get(&r, 0);
        if let Ok(n) = get(&r, 4).parse::<i64>()
            && n > 0
        {
            pk.push((n, name.clone()));
        }
        def.columns.push(ColumnDef {
            name,
            data_type: get(&r, 1),
            nullable: get(&r, 2) != "1",
            default: get(&r, 3),
            ..Default::default()
        });
    }
    pk.sort();
    def.primary_key = pk.into_iter().map(|(_, n)| n).collect();
    if def.primary_key.len() == 1 && contains_word(&sql, "AUTOINCREMENT") {
        let key = def.primary_key.first().cloned().unwrap_or_default();
        if let Some(c) = def.columns.iter_mut().find(|c| c.name == key) {
            c.auto_increment = true;
        }
    }
    for r in
        rows(conn, &format!("SELECT name, \"unique\", origin, partial FROM pragma_index_list({t}, {s}) ORDER BY name"))?
    {
        let name = get(&r, 0);
        let origin = get(&r, 2);
        if origin == "pk" {
            continue;
        }
        let parts =
            rows(conn, &format!("SELECT name, cid FROM pragma_index_info({}, {s}) ORDER BY seqno", lit(k, &name)?))?;
        let expression = parts.iter().any(|p| opt(p, 0).is_none());
        let definition = rows(
            conn,
            &format!("SELECT sql FROM {} WHERE type = 'index' AND name = {}", master(k, schema), lit(k, &name)?),
        )?
        .first()
        .map(|r| get(r, 0))
        .unwrap_or_default();
        let custom = expression || get(&r, 3) == "1";
        def.indexes.push(IndexDef {
            name,
            columns: if custom { Vec::new() } else { parts.iter().map(|p| get(p, 0)).collect() },
            unique: get(&r, 1) == "1",
            constraint: origin == "u",
            definition,
        });
    }
    let mut last_id = None;
    for r in rows(
        conn,
        &format!(
            "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete FROM pragma_foreign_key_list({t}, {s}) \
             ORDER BY id, seq"
        ),
    )? {
        let id = get(&r, 0);
        if last_id.as_ref() != Some(&id) {
            def.foreign_keys.push(ForeignKeyDef {
                name: format!("{table}_fk_{id}"),
                ref_table: get(&r, 1),
                on_update: FkAction::parse(&get(&r, 4)).unwrap_or_default(),
                on_delete: FkAction::parse(&get(&r, 5)).unwrap_or_default(),
                ..Default::default()
            });
            last_id = Some(id);
        }
        if let Some(f) = def.foreign_keys.last_mut() {
            f.columns.push(get(&r, 2));
            f.ref_columns.push(get(&r, 3));
        }
    }
    // `REFERENCES parent` without columns means the parent's primary key.
    for i in 0..def.foreign_keys.len() {
        let Some(f) = def.foreign_keys.get(i) else { continue };
        if f.ref_columns.iter().all(|c| !c.is_empty()) {
            continue;
        }
        let parent = f.ref_table.clone();
        let keys: Vec<String> = rows(
            conn,
            &format!("SELECT name FROM pragma_table_info({}, {s}) WHERE pk > 0 ORDER BY pk", lit(k, &parent)?),
        )?
        .iter()
        .map(|r| get(r, 0))
        .collect();
        if let Some(f) = def.foreign_keys.get_mut(i) {
            f.ref_columns = keys;
        }
    }
    def.checks = sqlite_checks(&sql);
    def.triggers = rows(
        conn,
        &format!("SELECT sql FROM {} WHERE type = 'trigger' AND tbl_name = {t} ORDER BY name", master(k, schema)),
    )?
    .iter()
    .map(|r| get(r, 0))
    .filter(|s| !s.is_empty())
    .collect();
    Ok(def)
}

fn contains_word(sql: &str, word: &str) -> bool {
    tokens(sql).iter().any(|t| match t {
        Tok::Word(w) => w.eq_ignore_ascii_case(word),
        Tok::Group(g) => contains_word(g, word),
        _ => false,
    })
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    /// A quoted identifier or string, unquoted.
    Quoted(String),
    /// A parenthesized group's inner text.
    Group(String),
    Comma,
    Other,
}

/// Top-level tokens of an SQL fragment (groups are kept whole).
fn tokens(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while chars.get(i).is_some_and(|c| *c != '\n') {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars.get(i) == Some(&'*') && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if matches!(c, '\'' | '"' | '`' | '[') {
            let close = if c == '[' { ']' } else { c };
            let mut s = String::new();
            i += 1;
            while let Some(&d) = chars.get(i) {
                i += 1;
                if d == close {
                    if close != ']' && chars.get(i) == Some(&close) {
                        s.push(d);
                        i += 1;
                        continue;
                    }
                    break;
                }
                s.push(d);
            }
            out.push(Tok::Quoted(s));
        } else if c == '(' {
            let start = i + 1;
            let mut depth = 0i32;
            let mut quote: Option<char> = None;
            while let Some(&d) = chars.get(i) {
                match quote {
                    Some(q) if d == q => quote = None,
                    Some(_) => {}
                    None => match d {
                        '\'' | '"' | '`' => quote = Some(d),
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    },
                }
                i += 1;
            }
            out.push(Tok::Group(chars.get(start..i.min(chars.len())).unwrap_or_default().iter().collect()));
            i += 1;
        } else if c == ',' {
            out.push(Tok::Comma);
            i += 1;
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let mut w = String::new();
            while let Some(&d) = chars.get(i).filter(|d| d.is_alphanumeric() || **d == '_' || **d == '$') {
                w.push(d);
                i += 1;
            }
            out.push(Tok::Word(w));
        } else {
            out.push(Tok::Other);
            i += 1;
        }
    }
    out
}

/// The CHECK constraints of a SQLite `CREATE TABLE` statement (column and table constraints).
pub fn sqlite_checks(create_sql: &str) -> Vec<CheckDef> {
    let toks = tokens(create_sql);
    let Some(Tok::Group(body)) = toks.into_iter().find(|t| matches!(t, Tok::Group(_))) else { return Vec::new() };
    let items = tokens(&body);
    let mut out = Vec::new();
    for (i, t) in items.iter().enumerate() {
        let Tok::Word(w) = t else { continue };
        if !w.eq_ignore_ascii_case("check") {
            continue;
        }
        let Some(Tok::Group(expr)) = items.get(i + 1) else { continue };
        let name = match (i.checked_sub(2).and_then(|j| items.get(j)), i.checked_sub(1).and_then(|j| items.get(j))) {
            (Some(Tok::Word(c)), Some(Tok::Word(n) | Tok::Quoted(n))) if c.eq_ignore_ascii_case("constraint") => {
                n.clone()
            }
            _ => String::new(),
        };
        out.push(CheckDef { name, expression: expr.trim().to_string() });
    }
    out
}

// ---------------------------------------------------------------- views and schemas

/// The views of a schema (or the named ones).
pub fn views(conn: &mut dyn Connection, schema: &str, only: Option<&[String]>) -> DbResult<Vec<ViewDef>> {
    let k = conn.kind();
    let s = lit(k, schema)?;
    let mut out: Vec<ViewDef> = match k {
        DbKind::Postgres => rows(
            conn,
            &format!(
                "SELECT c.relname::text, c.relkind::text, pg_get_viewdef(c.oid, true) FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = {s} AND c.relkind IN ('v', 'm') \
                 ORDER BY c.relname"
            ),
        )?
        .iter()
        .map(|r| ViewDef {
            name: get(r, 0),
            materialized: get(r, 1) == "m",
            query: get(r, 2).trim().trim_end_matches(';').trim().to_string(),
        })
        .collect(),
        DbKind::Mysql | DbKind::Mariadb => rows(
            conn,
            &format!(
                "SELECT TABLE_NAME, VIEW_DEFINITION FROM information_schema.VIEWS WHERE TABLE_SCHEMA = {s} \
                 ORDER BY TABLE_NAME"
            ),
        )?
        .iter()
        .map(|r| ViewDef { name: get(r, 0), materialized: false, query: get(r, 1).trim().to_string() })
        .collect(),
        DbKind::Sqlite => {
            rows(conn, &format!("SELECT name, sql FROM {} WHERE type = 'view' ORDER BY name", master(k, schema)))?
                .iter()
                .map(|r| ViewDef { name: get(r, 0), materialized: false, query: sqlite_view_query(&get(r, 1)) })
                .collect()
        }
        DbKind::SqlServer => rows(
            conn,
            &format!(
                "SELECT v.name, OBJECT_DEFINITION(v.object_id) FROM sys.views v JOIN sys.schemas s \
                 ON s.schema_id = v.schema_id WHERE s.name = {s} ORDER BY v.name"
            ),
        )?
        .iter()
        .map(|r| ViewDef { name: get(r, 0), materialized: false, query: sqlite_view_query(&get(r, 1)) })
        .collect(),
        DbKind::ClickHouse => rows(
            conn,
            &format!(
                "SELECT name, engine, as_select FROM system.tables WHERE database = {s} \
                 AND engine IN ('View', 'MaterializedView') ORDER BY name"
            ),
        )?
        .iter()
        .map(|r| ViewDef { name: get(r, 0), materialized: get(r, 1) == "MaterializedView", query: get(r, 2) })
        .collect(),
        DbKind::DuckDb => rows(
            conn,
            &format!(
                "SELECT view_name, sql FROM duckdb_views() WHERE schema_name = {s} AND NOT internal ORDER BY view_name"
            ),
        )?
        .iter()
        .map(|r| ViewDef {
            name: get(r, 0),
            materialized: false,
            query: sqlite_view_query(get(r, 1).trim().trim_end_matches(';')),
        })
        .collect(),
        DbKind::Redis => Vec::new(),
    };
    if let Some(only) = only {
        out.retain(|v| only.contains(&v.name));
    }
    Ok(out)
}

/// `CREATE VIEW v AS SELECT …` → `SELECT …`.
fn sqlite_view_query(sql: &str) -> String {
    // The first top-level AS after the view name starts the query.
    let lower = sql.to_ascii_lowercase();
    let mut search = 0;
    while let Some(pos) = lower.get(search..).and_then(|s| s.find("as")) {
        let at = search + pos;
        let before = lower.get(..at).and_then(|s| s.chars().last()).unwrap_or(' ');
        let after = lower.get(at + 2..).and_then(|s| s.chars().next()).unwrap_or(' ');
        let prefix = sql.get(..at).unwrap_or_default();
        let quotes = prefix.matches('"').count() + prefix.matches('`').count() + prefix.matches('\'').count();
        if before.is_whitespace() && (after.is_whitespace() || after == '(') && quotes % 2 == 0 {
            return sql.get(at + 2..).unwrap_or_default().trim().to_string();
        }
        search = at + 2;
    }
    sql.trim().to_string()
}

/// Tables and views of a schema (or only the named tables), with complete definitions.
pub fn schema_model(conn: &mut dyn Connection, schema: &str, only: Option<&[String]>) -> DbResult<SchemaModel> {
    let kind = conn.kind();
    let mut tables = Vec::new();
    for t in conn.tables(schema)? {
        if only.is_some_and(|o| !o.contains(&t.name)) || t.kind != TableKind::Table {
            continue;
        }
        tables.push(table_def(conn, schema, &t.name)?);
    }
    let views = views(conn, schema, only)?;
    Ok(SchemaModel { kind, schema: schema.to_string(), tables, views })
}

// ---------------------------------------------------------------- DDL of objects

fn view_ddl(kind: DbKind, schema: &str, v: &ViewDef) -> String {
    let what = if v.materialized { "MATERIALIZED VIEW" } else { "VIEW" };
    let or_replace = if kind == DbKind::Postgres && !v.materialized { "OR REPLACE " } else { "" };
    format!(
        "CREATE {or_replace}{what} {} AS\n{};\n",
        qualified(kind, schema, &v.name),
        v.query.trim().trim_end_matches(';')
    )
}

fn show_create(conn: &mut dyn Connection, what: &str, name: &str) -> DbResult<String> {
    let r = rows(conn, &format!("SHOW CREATE {what} {name}"))?;
    let ddl = r.first().map(|r| get(r, 1)).unwrap_or_default();
    if ddl.is_empty() {
        return Err(DbError::Query(format!("no {} {name}", what.to_ascii_lowercase())));
    }
    Ok(format!("{};\n", ddl.trim_end().trim_end_matches(';')))
}

fn sqlite_master_sql(conn: &mut dyn Connection, schema: &str, kind: &str, name: &str) -> DbResult<Option<String>> {
    let k = DbKind::Sqlite;
    Ok(rows(
        conn,
        &format!("SELECT sql FROM {} WHERE type = {} AND name = {}", master(k, schema), lit(k, kind)?, lit(k, name)?),
    )?
    .first()
    .map(|r| get(r, 0)))
}

/// The DDL of one object (DataGrip's Copy DDL / SQL Generator), statements ending with `;`.
pub fn object_ddl(conn: &mut dyn Connection, o: &ObjectRef) -> DbResult<String> {
    let kind = conn.kind();
    let my = matches!(kind, DbKind::Mysql | DbKind::Mariadb);
    match o.kind {
        ObjectKind::Schema => schema_ddl(conn, if o.schema.is_empty() { &o.name } else { &o.schema }),
        ObjectKind::Table if my => show_create(conn, "TABLE", &qualified(kind, &o.schema, &o.name)),
        ObjectKind::Table if kind == DbKind::Sqlite => {
            let sql = sqlite_master_sql(conn, &o.schema, "table", &o.name)?
                .ok_or_else(|| DbError::Query(format!("no table {}", o.display())))?;
            let mut out = vec![sql];
            let k = DbKind::Sqlite;
            for r in rows(
                conn,
                &format!(
                    "SELECT sql FROM {} WHERE type IN ('index', 'trigger') AND tbl_name = {} AND sql IS NOT NULL \
                     ORDER BY type, name",
                    master(k, &o.schema),
                    lit(k, &o.name)?
                ),
            )? {
                out.push(get(&r, 0));
            }
            Ok(join_statements(&out))
        }
        ObjectKind::Table if kind == DbKind::ClickHouse => {
            let r = rows(conn, &format!("SHOW CREATE TABLE {}", qualified(kind, &o.schema, &o.name)))?;
            let ddl = r.first().map(|r| get(r, 0)).unwrap_or_default();
            if ddl.is_empty() {
                return Err(DbError::Query(format!("no table {}", o.display())));
            }
            Ok(format!("{};\n", ddl.trim_end().trim_end_matches(';')))
        }
        ObjectKind::Table if kind == DbKind::DuckDb => {
            let r = rows(
                conn,
                &format!(
                    "SELECT sql FROM duckdb_tables() WHERE schema_name = {} AND table_name = {}",
                    lit(kind, &o.schema)?,
                    lit(kind, &o.name)?
                ),
            )?;
            let ddl =
                r.first().map(|r| get(r, 0)).ok_or_else(|| DbError::Query(format!("no table {}", o.display())))?;
            Ok(join_statements(&[ddl]))
        }
        ObjectKind::Table => {
            let t = table_def(conn, &o.schema, &o.name)?;
            table_ddl(kind, &t)
        }
        ObjectKind::View | ObjectKind::MaterializedView if my => {
            show_create(conn, "VIEW", &qualified(kind, &o.schema, &o.name))
        }
        ObjectKind::View | ObjectKind::MaterializedView if kind == DbKind::Sqlite => {
            let sql = sqlite_master_sql(conn, &o.schema, "view", &o.name)?
                .ok_or_else(|| DbError::Query(format!("no view {}", o.display())))?;
            Ok(join_statements(&[sql]))
        }
        ObjectKind::View | ObjectKind::MaterializedView => {
            let all = views(conn, &o.schema, Some(std::slice::from_ref(&o.name)))?;
            let v = all.first().ok_or_else(|| DbError::Query(format!("no view {}", o.display())))?;
            Ok(view_ddl(kind, &o.schema, v))
        }
        ObjectKind::Index => match kind {
            DbKind::Sqlite => match sqlite_master_sql(conn, &o.schema, "index", &o.name)? {
                Some(sql) if !sql.is_empty() => Ok(join_statements(&[sql])),
                Some(_) => Ok(format!(
                    "-- {} is created automatically for a PRIMARY KEY or UNIQUE constraint of {}\n",
                    o.name, o.table
                )),
                None => Err(DbError::Query(format!("no index {}", o.display()))),
            },
            DbKind::Postgres => {
                let k = DbKind::Postgres;
                let r = rows(
                    conn,
                    &format!(
                        "SELECT pg_get_indexdef(c.oid) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                         WHERE n.nspname = {} AND c.relname = {} AND c.relkind IN ('i', 'I')",
                        lit(k, &o.schema)?,
                        lit(k, &o.name)?
                    ),
                )?;
                let def =
                    r.first().map(|r| get(r, 0)).ok_or_else(|| DbError::Query(format!("no index {}", o.display())))?;
                Ok(join_statements(&[def]))
            }
            _ => {
                let t = table_def(conn, &o.schema, &o.table)?;
                let i = t
                    .indexes
                    .iter()
                    .find(|i| i.name == o.name)
                    .ok_or_else(|| DbError::Query(format!("no index {}", o.display())))?;
                Ok(join_statements(&[create_index(kind, &t, i)?]))
            }
        },
        ObjectKind::Column => {
            let t = table_def(conn, &o.schema, &o.table)?;
            let c = t.column(&o.name).ok_or_else(|| DbError::Query(format!("no column {}", o.display())))?;
            Ok(format!("ALTER TABLE {} ADD COLUMN {};\n", qualified(kind, &o.schema, &o.table), column_sql(kind, c)?))
        }
        ObjectKind::ForeignKey => {
            let t = table_def(conn, &o.schema, &o.table)?;
            let f = t
                .foreign_keys
                .iter()
                .find(|f| f.name == o.name)
                .ok_or_else(|| DbError::Query(format!("no foreign key {}", o.display())))?;
            let line = foreign_key_clause(kind, &t, f)?;
            Ok(format!("ALTER TABLE {} ADD {line};\n", qualified(kind, &o.schema, &o.table)))
        }
    }
}

/// The DDL of a whole schema: CREATE SCHEMA, its tables (referenced tables first) and views.
pub fn schema_ddl(conn: &mut dyn Connection, schema: &str) -> DbResult<String> {
    let kind = conn.kind();
    let mut out = String::new();
    match kind {
        DbKind::Postgres => out.push_str(&format!("CREATE SCHEMA {};\n\n", quote_ident(kind, schema))),
        DbKind::Mysql | DbKind::Mariadb => {
            out.push_str(&show_create(conn, "DATABASE", &quote_ident(kind, schema))?);
            out.push('\n');
        }
        DbKind::Sqlite => {}
        DbKind::SqlServer | DbKind::DuckDb => {
            out.push_str(&format!("CREATE SCHEMA {};\n\n", quote_ident(kind, schema)))
        }
        DbKind::ClickHouse => out.push_str(&format!("CREATE DATABASE {};\n\n", quote_ident(kind, schema))),
        DbKind::Redis => return Err(DbError::Unsupported("Redis has no schemas".into())),
    }
    let model = schema_model(conn, schema, None)?;
    for i in dependency_order(&model.tables) {
        let Some(t) = model.tables.get(i) else { continue };
        let o =
            ObjectRef { kind: ObjectKind::Table, schema: schema.into(), name: t.name.clone(), table: String::new() };
        out.push_str(&object_ddl(conn, &o)?);
        out.push('\n');
    }
    for v in &model.views {
        let o = ObjectRef {
            kind: if v.materialized { ObjectKind::MaterializedView } else { ObjectKind::View },
            schema: schema.into(),
            name: v.name.clone(),
            table: String::new(),
        };
        out.push_str(&object_ddl(conn, &o)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_expressions_and_lists() {
        assert_eq!(check_expression("CHECK ((price > (0)::numeric))"), "(price > (0)::numeric)");
        assert_eq!(check_expression("CHECK (a > 0) NOT VALID"), "a > 0");
        assert_eq!(check_expression("(`a` > 0)"), "`a` > 0");
        assert_eq!(split_quoted_list("a,\"b c\",\"d\"\"e\""), vec!["a", "b c", "d\"e"]);
        assert!(split_quoted_list("").is_empty());
    }

    #[test]
    fn sqlite_checks_are_parsed() {
        let sql = "CREATE TABLE \"t(x\" (a int CHECK (a > 0), \"b\" text CONSTRAINT b_len CHECK (length(b) < 10), \
                   -- check (no)\n c int, CONSTRAINT \"pair\" CHECK (a < c), CHECK (c <> ')'))";
        let checks = sqlite_checks(sql);
        assert_eq!(
            checks,
            vec![
                CheckDef { name: String::new(), expression: "a > 0".into() },
                CheckDef { name: "b_len".into(), expression: "length(b) < 10".into() },
                CheckDef { name: "pair".into(), expression: "a < c".into() },
                CheckDef { name: String::new(), expression: "c <> ')'".into() },
            ]
        );
        let _ = sqlite_checks("garbage (((");
        assert!(sqlite_checks("").is_empty());
        assert!(contains_word("create table t (id integer primary key autoincrement)", "AUTOINCREMENT"));
        assert!(!contains_word("create table t (x text default 'autoincrement')", "AUTOINCREMENT"));
    }

    #[test]
    fn mysql_defaults() {
        assert_eq!(my_default(None, "", "int", false), "");
        assert_eq!(my_default(Some("0".into()), "", "int", false), "0");
        assert_eq!(my_default(Some("abc".into()), "", "varchar(10)", false), "'abc'");
        assert_eq!(my_default(Some("it's".into()), "", "text", false), "'it''s'");
        assert_eq!(
            my_default(Some("CURRENT_TIMESTAMP".into()), "DEFAULT_GENERATED", "timestamp", false),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(my_default(Some("'abc'".into()), "", "varchar(10)", true), "'abc'");
        assert_eq!(my_default(Some("NULL".into()), "", "varchar(10)", true), "");
    }

    #[test]
    fn view_queries() {
        assert_eq!(sqlite_view_query("CREATE VIEW v AS SELECT 1"), "SELECT 1");
        assert_eq!(sqlite_view_query("create view \"has as\" as\nselect a as b from t"), "select a as b from t");
        assert_eq!(sqlite_view_query("CREATE VIEW v(a) AS(SELECT 1)"), "(SELECT 1)");
    }
}
