//! Catalog queries for what the schema tree does not load (routines, comments, view and
//! routine definitions, SQLite's own DDL), and DDL / Quick Documentation text built from it.

use std::collections::BTreeMap;

use serde::Serialize;

use codecraft_db::{Cell, ColumnInfo, DbKind, TableDetails, TableKind};

/// Kinds of objects the catalog is asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum InfoKind {
    Table,
    View,
    Routine,
}

/// A stored function or procedure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Routine {
    pub name: String,
    /// `function`, `procedure`, …
    pub kind: String,
    /// The result type (empty for procedures).
    pub returns: String,
}

/// Comments and definitions of one object, from the catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ObjectInfo {
    pub loading: bool,
    pub error: Option<String>,
    pub comment: Option<String>,
    pub column_comments: BTreeMap<String, String>,
    /// View or routine definitions (several for overloaded routines); SQLite's `CREATE`.
    pub definition: Vec<String>,
    /// SQLite: the `CREATE INDEX` / `CREATE TRIGGER` statements of a table.
    pub extra: Vec<String>,
}

/// An SQL string literal (backslashes doubled for MySQL, where they escape).
pub(crate) fn lit(kind: DbKind, s: &str) -> String {
    let s = s.replace('\'', "''");
    match kind {
        DbKind::Mysql | DbKind::Mariadb => format!("'{}'", s.replace('\\', "\\\\")),
        _ => format!("'{s}'"),
    }
}

/// `name` as an identifier of `kind`: bare when it is plain lower case, else quoted.
pub(crate) fn ident(kind: DbKind, name: &str) -> String {
    super::resolve::identifier(name, matches!(kind, DbKind::Mysql | DbKind::Mariadb))
}

fn full_name(kind: DbKind, schema: &str, name: &str) -> String {
    if schema.is_empty() { ident(kind, name) } else { format!("{}.{}", ident(kind, schema), ident(kind, name)) }
}

/// Routines of a schema: rows of (name, kind, result type). `None` where there are none
/// (SQLite).
pub fn routines_query(kind: DbKind, schema: &str) -> Option<String> {
    match kind {
        DbKind::Sqlite => None,
        _ => Some(format!(
            "SELECT DISTINCT routine_name, lower(routine_type), coalesce(data_type, '') FROM information_schema.routines \
             WHERE routine_schema = {} ORDER BY 1",
            lit(kind, schema)
        )),
    }
}

/// Comments and definitions of an object: rows of (what, column, text).
pub fn info_query(kind: DbKind, schema: &str, name: &str, what: InfoKind) -> Option<String> {
    let (s, n) = (lit(kind, schema), lit(kind, name));
    Some(match (kind, what) {
        (DbKind::Postgres, InfoKind::Routine) => format!(
            "SELECT 'definition', NULL, pg_get_functiondef(p.oid) FROM pg_proc p JOIN pg_namespace ns ON ns.oid = p.pronamespace \
             WHERE ns.nspname = {s} AND p.proname = {n} AND p.prokind IN ('f', 'p') \
             UNION ALL SELECT 'comment', NULL, obj_description(p.oid, 'pg_proc') FROM pg_proc p \
             JOIN pg_namespace ns ON ns.oid = p.pronamespace WHERE ns.nspname = {s} AND p.proname = {n}"
        ),
        (DbKind::Postgres, _) => format!(
            "SELECT 'comment', NULL, obj_description(c.oid, 'pg_class') FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace \
             WHERE ns.nspname = {s} AND c.relname = {n} \
             UNION ALL SELECT 'column', a.attname::text, col_description(c.oid, a.attnum) FROM pg_attribute a \
             JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace ns ON ns.oid = c.relnamespace \
             WHERE ns.nspname = {s} AND c.relname = {n} AND a.attnum > 0 AND NOT a.attisdropped \
             UNION ALL SELECT 'definition', NULL, pg_get_viewdef(c.oid, true) FROM pg_class c \
             JOIN pg_namespace ns ON ns.oid = c.relnamespace WHERE ns.nspname = {s} AND c.relname = {n} AND c.relkind IN ('v', 'm')"
        ),
        (DbKind::Mysql | DbKind::Mariadb, InfoKind::Routine) => format!(
            "SELECT 'definition', NULL, routine_definition FROM information_schema.routines WHERE routine_schema = {s} AND routine_name = {n} \
             UNION ALL SELECT 'comment', NULL, routine_comment FROM information_schema.routines WHERE routine_schema = {s} AND routine_name = {n}"
        ),
        (DbKind::Mysql | DbKind::Mariadb, _) => format!(
            "SELECT 'comment', NULL, table_comment FROM information_schema.tables WHERE table_schema = {s} AND table_name = {n} \
             UNION ALL SELECT 'column', column_name, column_comment FROM information_schema.columns WHERE table_schema = {s} AND table_name = {n} \
             UNION ALL SELECT 'definition', NULL, view_definition FROM information_schema.views WHERE table_schema = {s} AND table_name = {n}"
        ),
        (DbKind::Sqlite, InfoKind::Routine) => return None,
        (DbKind::Sqlite, _) => {
            let master = format!("{}.sqlite_master", codecraft_db::sql::quote_ident(DbKind::Sqlite, schema));
            format!(
                "SELECT 'definition', NULL, sql FROM {master} WHERE name = {n} AND type IN ('table', 'view') AND sql IS NOT NULL \
                 UNION ALL SELECT 'extra', name, sql FROM {master} WHERE tbl_name = {n} AND type IN ('index', 'trigger') AND sql IS NOT NULL"
            )
        }
    })
}

fn text(c: Option<&Cell>) -> Option<String> {
    match c? {
        Cell::Null => None,
        other => Some(other.to_plain()),
    }
}

/// Rows of [`routines_query`].
pub fn parse_routines(rows: &[Vec<Cell>]) -> Vec<Routine> {
    rows.iter()
        .filter_map(|r| {
            Some(Routine {
                name: text(r.first())?,
                kind: text(r.get(1)).unwrap_or_else(|| "function".into()),
                returns: text(r.get(2)).unwrap_or_default(),
            })
        })
        .take(20_000)
        .collect()
}

/// Rows of [`info_query`].
pub fn parse_info(rows: &[Vec<Cell>]) -> ObjectInfo {
    let mut info = ObjectInfo::default();
    for r in rows.iter().take(10_000) {
        let (Some(what), value) = (text(r.first()), text(r.get(2))) else { continue };
        let Some(value) = value.filter(|v| !v.trim().is_empty()) else { continue };
        match what.as_str() {
            "comment" => info.comment = Some(value),
            "column" => {
                if let Some(c) = text(r.get(1)) {
                    info.column_comments.insert(c, value);
                }
            }
            "definition" => info.definition.push(value),
            "extra" => info.extra.push(value),
            _ => {}
        }
    }
    info
}

fn statement(text: &str) -> String {
    let t = text.trim_end();
    if t.ends_with(';') { t.to_string() } else { format!("{t};") }
}

fn column_line(kind: DbKind, c: &ColumnInfo, single_pk: bool, comment: Option<&String>) -> String {
    let mut line =
        format!("    {} {}", ident(kind, &c.name), if c.data_type.is_empty() { "text" } else { &c.data_type });
    if let Some(d) = &c.default {
        line.push_str(&format!(" DEFAULT {d}"));
    }
    if !c.nullable && !(single_pk && c.primary_key) {
        line.push_str(" NOT NULL");
    }
    if single_pk && c.primary_key {
        line.push_str(" PRIMARY KEY");
    }
    if let (DbKind::Mysql | DbKind::Mariadb, Some(cm)) = (kind, comment) {
        line.push_str(&format!(" COMMENT {}", lit(kind, cm)));
    }
    line
}

/// The DDL of a table or view: the server's own text where the catalog has it (SQLite, view
/// definitions), else generated from the columns, keys and indexes.
pub fn table_ddl(
    kind: DbKind,
    schema: &str,
    name: &str,
    tkind: TableKind,
    details: Option<&TableDetails>,
    info: Option<&ObjectInfo>,
) -> String {
    let full = full_name(kind, schema, name);
    let mut out = String::new();
    let def = info.and_then(|i| i.definition.first());
    let is_view = matches!(tkind, TableKind::View | TableKind::MaterializedView);
    if let Some(d) = def.filter(|d| d.trim_start().get(..6).is_some_and(|p| p.eq_ignore_ascii_case("create"))) {
        // SQLite keeps the statement as written.
        out.push_str(&statement(d));
        out.push('\n');
        for x in info.map(|i| i.extra.as_slice()).unwrap_or(&[]) {
            out.push('\n');
            out.push_str(&statement(x));
            out.push('\n');
        }
        return out;
    }
    if is_view {
        let mat = if tkind == TableKind::MaterializedView { "MATERIALIZED " } else { "" };
        match def {
            Some(d) => out.push_str(&format!("CREATE {mat}VIEW {full} AS\n{}\n", statement(d.trim()))),
            None => {
                out.push_str("-- The view's definition is not available.\n");
                let cols: Vec<String> =
                    details.map(|d| d.columns.iter().map(|c| ident(kind, &c.name)).collect()).unwrap_or_default();
                out.push_str(&format!("CREATE {mat}VIEW {full} ({}) AS ...;\n", cols.join(", ")));
            }
        }
        return out;
    }
    let Some(d) = details else {
        return format!("-- The columns of {full} are not loaded.\nCREATE TABLE {full} ();\n");
    };
    let comments = info.map(|i| &i.column_comments);
    let pk: Vec<&ColumnInfo> = d.columns.iter().filter(|c| c.primary_key).collect();
    let single_pk = pk.len() == 1;
    let mut lines: Vec<String> =
        d.columns.iter().map(|c| column_line(kind, c, single_pk, comments.and_then(|m| m.get(&c.name)))).collect();
    if pk.len() > 1 {
        lines.push(format!(
            "    PRIMARY KEY ({})",
            pk.iter().map(|c| ident(kind, &c.name)).collect::<Vec<_>>().join(", ")
        ));
    }
    for fk in &d.foreign_keys {
        let target = full_name(kind, fk.ref_schema.as_deref().unwrap_or(""), &fk.ref_table);
        let cols = fk.columns.iter().map(|c| ident(kind, c)).collect::<Vec<_>>().join(", ");
        let refs = fk.ref_columns.iter().map(|c| ident(kind, c)).collect::<Vec<_>>().join(", ");
        let constraint =
            if fk.name.is_empty() { String::new() } else { format!("CONSTRAINT {} ", ident(kind, &fk.name)) };
        lines.push(format!("    {constraint}FOREIGN KEY ({cols}) REFERENCES {target} ({refs})"));
    }
    out.push_str(&format!("CREATE TABLE {full}\n(\n{}\n)", lines.join(",\n")));
    let table_comment = info.and_then(|i| i.comment.as_ref());
    match (kind, table_comment) {
        (DbKind::Mysql | DbKind::Mariadb, Some(c)) => out.push_str(&format!(" COMMENT = {};\n", lit(kind, c))),
        _ => out.push_str(";\n"),
    }
    if kind == DbKind::Postgres {
        if let Some(c) = table_comment {
            out.push_str(&format!("\nCOMMENT ON TABLE {full} IS {};\n", lit(kind, c)));
        }
        for c in &d.columns {
            if let Some(cm) = comments.and_then(|m| m.get(&c.name)) {
                out.push_str(&format!("COMMENT ON COLUMN {full}.{} IS {};\n", ident(kind, &c.name), lit(kind, cm)));
            }
        }
    }
    let indexes: Vec<_> = d.indexes.iter().filter(|i| !i.primary).collect();
    if !indexes.is_empty() {
        out.push('\n');
    }
    for i in indexes {
        let unique = if i.unique { "UNIQUE " } else { "" };
        let cols = i.columns.iter().map(|c| ident(kind, c)).collect::<Vec<_>>().join(", ");
        out.push_str(&format!("CREATE {unique}INDEX {} ON {full} ({cols});\n", ident(kind, &i.name)));
    }
    out
}

/// The DDL of a routine (its definitions, or a note when the server has none to show).
pub fn routine_ddl(kind: DbKind, schema: &str, name: &str, info: Option<&ObjectInfo>) -> String {
    let defs = info.map(|i| i.definition.as_slice()).unwrap_or(&[]);
    if defs.is_empty() {
        return format!("-- The definition of {} is not available.\n", full_name(kind, schema, name));
    }
    let body: Vec<String> = defs
        .iter()
        .map(|d| match kind {
            // MySQL's routine_definition is just the body.
            DbKind::Mysql | DbKind::Mariadb => format!("-- {}\n{}", full_name(kind, schema, name), d.trim()),
            _ => statement(d),
        })
        .collect();
    format!("{}\n", body.join("\n\n"))
}

/// The line (0-based) of `column`'s definition in a DDL text.
pub fn column_line_of(ddl: &str, column: &str) -> Option<usize> {
    let plain = column.to_ascii_lowercase();
    ddl.lines().position(|l| {
        let t = l.trim_start().trim_start_matches(['"', '`', '[']);
        let lower = t.to_ascii_lowercase();
        lower.starts_with(&plain) && lower.get(plain.len()..).is_some_and(|r| r.starts_with([' ', '"', '`', ']', '\t']))
    })
}

fn kind_label(t: TableKind) -> &'static str {
    match t {
        TableKind::Table => "table",
        TableKind::View => "view",
        TableKind::MaterializedView => "materialized view",
        TableKind::Foreign => "foreign table",
    }
}

fn column_summary(c: &ColumnInfo) -> String {
    let mut s = c.data_type.clone();
    if c.primary_key {
        s.push_str(", primary key");
    }
    if !c.nullable {
        s.push_str(", not null");
    }
    if let Some(d) = &c.default {
        s.push_str(&format!(", default {d}"));
    }
    s
}

/// Lines of a DDL kept in a documentation popup.
const DOC_DDL_LINES: usize = 60;

fn ddl_block(ddl: &str) -> String {
    let mut lines: Vec<&str> = ddl.lines().take(DOC_DDL_LINES).collect();
    if ddl.lines().count() > DOC_DDL_LINES {
        lines.push("…");
    }
    format!("```sql\n{}\n```", lines.join("\n"))
}

/// Quick Documentation of a table or view.
#[allow(clippy::too_many_arguments)]
pub fn table_doc(
    source: &str,
    kind: DbKind,
    schema: &str,
    name: &str,
    tkind: TableKind,
    details: Option<&TableDetails>,
    info: Option<&ObjectInfo>,
    rows: Option<u64>,
) -> String {
    let mut out = format!("**{name}** · {} in `{schema}` · {source} ({})", kind_label(tkind), kind.label());
    if let Some(n) = rows {
        out.push_str(&format!(" · {n} rows"));
    }
    out.push_str("\n\n");
    if let Some(c) = info.and_then(|i| i.comment.as_ref()) {
        out.push_str(&format!("{c}\n\n"));
    }
    if let Some(d) = details {
        out.push_str(&format!("**Columns** ({})\n\n", d.columns.len()));
        for c in d.columns.iter().take(200) {
            let comment =
                info.and_then(|i| i.column_comments.get(&c.name)).map(|c| format!(" — {c}")).unwrap_or_default();
            out.push_str(&format!("- `{}` {}{comment}\n", c.name, column_summary(c)));
        }
        if d.columns.len() > 200 {
            out.push_str("- …\n");
        }
        out.push('\n');
    } else {
        out.push_str("Loading columns…\n\n");
    }
    out.push_str(&ddl_block(&table_ddl(kind, schema, name, tkind, details, info)));
    out
}

/// Quick Documentation of a column.
pub fn column_doc(table: &str, schema: &str, c: &ColumnInfo, comment: Option<&String>) -> String {
    let mut out = format!("**{table}.{}** · column in `{schema}`\n\n`{}`\n", c.name, column_summary(c));
    if let Some(cm) = comment {
        out.push_str(&format!("\n{cm}\n"));
    }
    out
}

/// Quick Documentation of a routine.
pub fn routine_doc(kind: DbKind, schema: &str, r: &Routine, info: Option<&ObjectInfo>) -> String {
    let returns = if r.returns.is_empty() { String::new() } else { format!(" returns `{}`", r.returns) };
    let mut out = format!("**{}** · {} in `{schema}`{returns}\n\n", r.name, r.kind);
    if let Some(c) = info.and_then(|i| i.comment.as_ref()) {
        out.push_str(&format!("{c}\n\n"));
    }
    out.push_str(&ddl_block(&routine_ddl(kind, schema, &r.name, info)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_db::{ForeignKeyInfo, IndexInfo};

    fn details() -> TableDetails {
        let col = |name: &str, ty: &str, nullable: bool, pk: bool| ColumnInfo {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: None,
            primary_key: pk,
        };
        TableDetails {
            columns: vec![
                col("id", "integer", false, true),
                col("Name", "text", true, false),
                col("author_id", "integer", true, false),
            ],
            indexes: vec![
                IndexInfo { name: "books_pkey".into(), columns: vec!["id".into()], unique: true, primary: true },
                IndexInfo {
                    name: "by_author".into(),
                    columns: vec!["author_id".into()],
                    unique: false,
                    primary: false,
                },
            ],
            foreign_keys: vec![ForeignKeyInfo {
                name: "fk_author".into(),
                columns: vec!["author_id".into()],
                ref_schema: Some("public".into()),
                ref_table: "authors".into(),
                ref_columns: vec!["id".into()],
            }],
        }
    }

    #[test]
    fn generated_table_ddl() {
        let info = ObjectInfo {
            comment: Some("All the books".into()),
            column_comments: [("Name".to_string(), "it's the title".to_string())].into_iter().collect(),
            ..ObjectInfo::default()
        };
        let ddl = table_ddl(DbKind::Postgres, "public", "books", TableKind::Table, Some(&details()), Some(&info));
        assert!(
            ddl.starts_with("CREATE TABLE public.books\n(\n    id integer PRIMARY KEY,\n    \"Name\" text,"),
            "{ddl}"
        );
        assert!(ddl.contains("CONSTRAINT fk_author FOREIGN KEY (author_id) REFERENCES public.authors (id)"));
        assert!(ddl.contains("COMMENT ON TABLE public.books IS 'All the books';"));
        assert!(ddl.contains("COMMENT ON COLUMN public.books.\"Name\" IS 'it''s the title';"));
        assert!(ddl.contains("CREATE INDEX by_author ON public.books (author_id);"));
        assert!(!ddl.contains("books_pkey"));
        assert_eq!(column_line_of(&ddl, "Name"), Some(3));
        assert_eq!(column_line_of(&ddl, "author_id"), Some(4));
        let my = table_ddl(DbKind::Mysql, "app", "books", TableKind::Table, Some(&details()), Some(&info));
        assert!(
            my.contains("`Name` text COMMENT 'it''s the title'") && my.contains("COMMENT = 'All the books';"),
            "{my}"
        );
    }

    #[test]
    fn server_definitions_win() {
        let rows = vec![
            vec![Cell::Text("definition".into()), Cell::Null, Cell::Text("CREATE TABLE t (a int)".into())],
            vec![Cell::Text("extra".into()), Cell::Text("i".into()), Cell::Text("CREATE INDEX i ON t(a)".into())],
            vec![Cell::Text("comment".into()), Cell::Null, Cell::Null],
            vec![Cell::Int(1)],
        ];
        let info = parse_info(&rows);
        assert_eq!(info.comment, None);
        let ddl = table_ddl(DbKind::Sqlite, "main", "t", TableKind::Table, None, Some(&info));
        assert_eq!(ddl, "CREATE TABLE t (a int);\n\nCREATE INDEX i ON t(a);\n");
        let view = ObjectInfo { definition: vec![" SELECT 1;".into()], ..Default::default() };
        assert_eq!(
            table_ddl(DbKind::Postgres, "s", "v", TableKind::View, None, Some(&view)),
            "CREATE VIEW s.v AS\nSELECT 1;\n"
        );
        assert!(table_ddl(DbKind::Postgres, "s", "t", TableKind::Table, None, None).contains("not loaded"));
        let r = parse_routines(&[
            vec![Cell::Text("f".into()), Cell::Text("function".into()), Cell::Text("integer".into())],
            vec![],
        ]);
        assert_eq!(r, [Routine { name: "f".into(), kind: "function".into(), returns: "integer".into() }]);
        assert!(routine_ddl(DbKind::Postgres, "s", "f", None).contains("not available"));
    }

    #[test]
    fn queries_quote_names() {
        let q = info_query(DbKind::Postgres, "o'k", "t", InfoKind::Table).unwrap_or_default();
        assert!(q.contains("'o''k'"));
        let q = info_query(DbKind::Mysql, "a\\b", "t", InfoKind::Routine).unwrap_or_default();
        assert!(q.contains("'a\\\\b'"));
        assert!(routines_query(DbKind::Sqlite, "main").is_none());
        assert!(info_query(DbKind::Sqlite, "main", "t", InfoKind::Routine).is_none());
        assert!(
            info_query(DbKind::Sqlite, "ma\"in", "t", InfoKind::Table)
                .unwrap_or_default()
                .contains("\"ma\"\"in\".sqlite_master")
        );
        let doc = table_doc("Lib", DbKind::Sqlite, "main", "books", TableKind::Table, Some(&details()), None, Some(3));
        assert!(doc.starts_with("**books** · table in `main` · Lib (SQLite) · 3 rows"));
        assert!(doc.contains("- `id` integer, primary key, not null"));
    }
}
