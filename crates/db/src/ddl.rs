//! DDL generation per dialect (pure) and the runner that applies a generated script.
//!
//! A [`TableDef`] describes a table the way the Create/Modify Table dialogs edit it: columns
//! (type, default, nullability, auto-increment, comment), the primary key, indexes, foreign keys
//! (with ON DELETE/UPDATE actions) and checks. [`create_table`] turns it into `CREATE TABLE`
//! (+ indexes and comments); [`alter_table`] turns an old and a new definition into the statements
//! that change one into the other. SQLite cannot alter much in place, so most changes there become
//! the table rebuild sequence of the SQLite documentation (create the new table, copy the rows,
//! drop the old one, rename) inside a transaction with foreign keys off.
//!
//! Every identifier is quoted for its dialect ([`quote_ident`]) and every comment is a string
//! literal ([`crate::edit::literal`]). Types, defaults and check expressions are SQL fragments the
//! user types: they are checked to be one balanced expression (no `;`, no comments, quotes and
//! parentheses closed) so they can never end the statement they are part of.
//!
//! [`run_script`] executes a [`DdlScript`]: in one transaction where the dialect has
//! transactional DDL (PostgreSQL, SQLite) and rolled back on the first error; MySQL commits every
//! DDL statement, so there it runs statement by statement and reports exactly which statement
//! failed and which ones were applied before it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::edit::{begin_statement, literal};
use crate::sql::{qualified, quote_ident};
use crate::{Cell, Connection, DbError, DbKind, DbResult, query_all};

// ---------------------------------------------------------------- model

/// What happens to referencing rows when the referenced row is deleted or updated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FkAction {
    #[default]
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl FkAction {
    pub const ALL: [FkAction; 5] =
        [FkAction::NoAction, FkAction::Restrict, FkAction::Cascade, FkAction::SetNull, FkAction::SetDefault];

    pub fn sql(self) -> &'static str {
        match self {
            FkAction::NoAction => "NO ACTION",
            FkAction::Restrict => "RESTRICT",
            FkAction::Cascade => "CASCADE",
            FkAction::SetNull => "SET NULL",
            FkAction::SetDefault => "SET DEFAULT",
        }
    }

    /// `CASCADE`, `set null`, `setNull`, PostgreSQL's one-letter codes (`a`, `r`, `c`, `n`, `d`).
    pub fn parse(s: &str) -> Option<FkAction> {
        let k: String = s.trim().chars().filter(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
        Some(match k.as_str() {
            "noaction" | "a" | "" => FkAction::NoAction,
            "restrict" | "r" => FkAction::Restrict,
            "cascade" | "c" => FkAction::Cascade,
            "setnull" | "n" => FkAction::SetNull,
            "setdefault" | "d" => FkAction::SetDefault,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ColumnDef {
    pub name: String,
    /// The type as typed (`varchar(20)`, `numeric(10, 2)`, `integer[]`).
    pub data_type: String,
    pub nullable: bool,
    /// The default as an SQL expression (`0`, `'text'`, `now()`); empty: none.
    pub default: String,
    /// Identity (PostgreSQL), `AUTO_INCREMENT` (MySQL), `INTEGER PRIMARY KEY AUTOINCREMENT` (SQLite).
    pub auto_increment: bool,
    pub comment: String,
    /// The column's current name in the database (Modify Table); empty for a new column.
    /// Renames are detected through it.
    pub original: String,
}

impl Default for ColumnDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            data_type: String::new(),
            nullable: true,
            default: String::new(),
            auto_increment: false,
            comment: String::new(),
            original: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexDef {
    /// Empty: a name is generated (`<table>_<columns>_index`).
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    /// A UNIQUE constraint (dropped with DROP CONSTRAINT) rather than a plain index.
    pub constraint: bool,
    /// The catalog's own `CREATE INDEX` (expression and partial indexes), used as is while the
    /// index is unchanged.
    pub definition: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ForeignKeyDef {
    /// Empty: a name is generated (`<table>_<columns>_fk`).
    pub name: String,
    pub columns: Vec<String>,
    /// Empty: the table's own schema.
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: FkAction,
    pub on_update: FkAction,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CheckDef {
    /// Empty: unnamed (the database names it).
    pub name: String,
    /// The condition, e.g. `price > 0`.
    pub expression: String,
}

/// A table as the Create/Modify Table dialogs edit it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TableDef {
    pub schema: String,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// Primary key columns, in key order (empty: no primary key).
    pub primary_key: Vec<String>,
    /// The primary key constraint's name (empty: the database's default).
    pub primary_key_name: String,
    pub indexes: Vec<IndexDef>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    pub checks: Vec<CheckDef>,
    pub comment: String,
    /// SQLite triggers on the table (`CREATE TRIGGER …` as stored), recreated by a rebuild.
    pub triggers: Vec<String>,
    /// PostgreSQL views that use the table: they block column type changes, so an alter drops
    /// and recreates them.
    pub dependent_views: Vec<DependentView>,
}

/// A view that depends on a table, with the statements to drop and recreate it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DependentView {
    pub name: String,
    pub drop: String,
    pub create: String,
}

impl TableDef {
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// The definition as a Modify Table dialog starts it: every column remembers its name.
    pub fn for_modify(mut self) -> Self {
        for c in &mut self.columns {
            c.original = c.name.clone();
        }
        self
    }
}

/// A database object the Drop/Rename/SQL Generator actions address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectKind {
    Schema,
    Table,
    View,
    MaterializedView,
    Column,
    Index,
    ForeignKey,
}

impl ObjectKind {
    pub fn parse(s: &str) -> Option<ObjectKind> {
        let k: String = s.chars().filter(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
        Some(match k.as_str() {
            "schema" | "database" => ObjectKind::Schema,
            "table" => ObjectKind::Table,
            "view" => ObjectKind::View,
            "materializedview" | "matview" => ObjectKind::MaterializedView,
            "column" => ObjectKind::Column,
            "index" => ObjectKind::Index,
            "foreignkey" | "fk" => ObjectKind::ForeignKey,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Schema => "schema",
            ObjectKind::Table => "table",
            ObjectKind::View => "view",
            ObjectKind::MaterializedView => "materialized view",
            ObjectKind::Column => "column",
            ObjectKind::Index => "index",
            ObjectKind::ForeignKey => "foreign key",
        }
    }

    /// Objects that belong to a table (and need [`ObjectRef::table`]).
    pub fn in_table(self) -> bool {
        matches!(self, ObjectKind::Column | ObjectKind::Index | ObjectKind::ForeignKey)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRef {
    pub kind: ObjectKind,
    /// The schema (for a schema itself: its name again, or empty).
    #[serde(default)]
    pub schema: String,
    pub name: String,
    /// The table of a column, index or foreign key.
    #[serde(default)]
    pub table: String,
}

impl ObjectRef {
    /// `schema.table.name` for messages.
    pub fn display(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if !self.schema.is_empty() && self.kind != ObjectKind::Schema {
            parts.push(&self.schema);
        }
        if self.kind.in_table() && !self.table.is_empty() {
            parts.push(&self.table);
        }
        parts.push(&self.name);
        parts.join(".")
    }
}

/// Statements that change a schema, as previewed and as run by [`run_script`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DdlScript {
    pub kind: DbKind,
    /// Run before the transaction (SQLite: `PRAGMA foreign_keys = OFF`, which a transaction ignores).
    pub setup: Vec<String>,
    /// The changes, in order.
    pub statements: Vec<String>,
    /// Run after the transaction, whatever happened.
    pub cleanup: Vec<String>,
    /// `statements` run in one transaction (PostgreSQL, SQLite). MySQL commits each DDL statement.
    pub transactional: bool,
    /// Things the user should know (shown above the SQL as comments).
    pub warnings: Vec<String>,
}

impl DdlScript {
    fn new(kind: DbKind) -> Self {
        Self { kind, transactional: transactional_ddl(kind), ..Self::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// The script as it runs, for the preview: warnings as comments, then setup, BEGIN, the
    /// statements, COMMIT and cleanup, each ending with `;`.
    pub fn to_sql(&self) -> String {
        let mut out = String::new();
        for w in &self.warnings {
            for line in w.lines() {
                out.push_str("-- ");
                out.push_str(line);
                out.push('\n');
            }
        }
        let mut push = |s: &str| {
            out.push_str(s.trim_end());
            out.push_str(";\n");
        };
        for s in &self.setup {
            push(s);
        }
        let wrap = self.transactional && !self.statements.is_empty();
        if wrap {
            push(begin_statement(self.kind));
        }
        for s in &self.statements {
            push(s);
        }
        if wrap {
            push("COMMIT");
        }
        for s in &self.cleanup {
            push(s);
        }
        out
    }

    /// Append another script's statements (same dialect).
    pub fn extend(&mut self, other: DdlScript) {
        for s in other.setup {
            if !self.setup.contains(&s) {
                self.setup.push(s);
            }
        }
        self.statements.extend(other.statements);
        for s in other.cleanup {
            if !self.cleanup.contains(&s) {
                self.cleanup.push(s);
            }
        }
        self.warnings.extend(other.warnings);
    }
}

/// Whether DDL statements can be rolled back.
pub fn transactional_ddl(kind: DbKind) -> bool {
    matches!(kind, DbKind::Postgres | DbKind::Sqlite)
}

/// Common types for the dialect, for the type field's suggestions.
pub fn type_suggestions(kind: DbKind) -> &'static [&'static str] {
    match kind {
        DbKind::Postgres => &[
            "integer",
            "bigint",
            "smallint",
            "serial",
            "bigserial",
            "numeric(10, 2)",
            "real",
            "double precision",
            "boolean",
            "text",
            "varchar(255)",
            "char(1)",
            "date",
            "time",
            "timestamp",
            "timestamp with time zone",
            "interval",
            "uuid",
            "json",
            "jsonb",
            "bytea",
            "inet",
            "integer[]",
            "text[]",
        ],
        DbKind::Mysql | DbKind::Mariadb => &[
            "int",
            "bigint",
            "smallint",
            "tinyint",
            "tinyint(1)",
            "decimal(10, 2)",
            "float",
            "double",
            "boolean",
            "varchar(255)",
            "char(1)",
            "text",
            "mediumtext",
            "longtext",
            "date",
            "datetime",
            "timestamp",
            "time",
            "year",
            "json",
            "blob",
            "longblob",
            "bit(1)",
            "enum('a', 'b')",
        ],
        DbKind::Sqlite => {
            &["INTEGER", "TEXT", "REAL", "NUMERIC", "BLOB", "BOOLEAN", "DATE", "DATETIME", "VARCHAR(255)"]
        }
    }
}

// ---------------------------------------------------------------- validation

/// Check that `s` is one SQL fragment that cannot end its statement: quotes and parentheses
/// balanced, no `;` and no comment outside quotes.
pub fn check_fragment(what: &str, s: &str) -> DbResult<()> {
    let bad = |why: &str| Err(DbError::Config(format!("{what} `{}`: {why}", s.trim())));
    if s.contains('\0') {
        return bad("contains a NUL character");
    }
    let mut depth: i32 = 0;
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                if chars.peek() == Some(&q) {
                    chars.next();
                } else {
                    quote = None;
                }
            } else if c == '\\' && q == '\'' {
                // A backslash escape (MySQL, E'' strings): skip the escaped character.
                chars.next();
            }
            continue;
        }
        match c {
            '\'' | '"' | '`' => quote = Some(c),
            '(' | '[' => depth += 1,
            ')' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return bad("unbalanced parentheses");
                }
            }
            ';' => return bad("must not contain `;`"),
            '-' if chars.peek() == Some(&'-') => return bad("must not contain a comment"),
            '/' if chars.peek() == Some(&'*') => return bad("must not contain a comment"),
            '#' => return bad("must not contain `#`"),
            _ => {}
        }
    }
    if quote.is_some() {
        return bad("unterminated quote");
    }
    if depth != 0 {
        return bad("unbalanced parentheses");
    }
    Ok(())
}

fn check_name(what: &str, name: &str) -> DbResult<()> {
    if name.trim().is_empty() {
        return Err(DbError::Config(format!("{what} needs a name")));
    }
    if name.contains('\0') {
        return Err(DbError::Config(format!("{what} name contains a NUL character")));
    }
    Ok(())
}

fn check_type(col: &ColumnDef) -> DbResult<()> {
    if col.data_type.trim().is_empty() {
        return Err(DbError::Config(format!("column `{}` needs a type", col.name)));
    }
    check_fragment(&format!("type of `{}`", col.name), &col.data_type)
}

/// Validate a definition: names, types, defaults, checks, and that keys name existing columns.
pub fn validate(kind: DbKind, t: &TableDef) -> DbResult<()> {
    check_name("the table", &t.name)?;
    if t.columns.is_empty() {
        return Err(DbError::Config("a table needs at least one column".into()));
    }
    let mut seen = BTreeSet::new();
    for c in &t.columns {
        check_name("a column", &c.name)?;
        if !seen.insert(c.name.as_str()) {
            return Err(DbError::Config(format!("duplicate column `{}`", c.name)));
        }
        check_type(c)?;
        if !c.default.trim().is_empty() {
            check_fragment(&format!("default of `{}`", c.name), &c.default)?;
        }
        if c.auto_increment && !c.default.trim().is_empty() {
            return Err(DbError::Config(format!("column `{}` cannot have both a default and auto-increment", c.name)));
        }
    }
    let known = |cols: &[String], what: &str| -> DbResult<()> {
        if cols.is_empty() {
            return Err(DbError::Config(format!("{what} needs at least one column")));
        }
        for c in cols {
            if !seen.contains(c.as_str()) {
                return Err(DbError::Config(format!("{what}: no column `{c}`")));
            }
        }
        Ok(())
    };
    if !t.primary_key.is_empty() {
        known(&t.primary_key, "the primary key")?;
    }
    for i in &t.indexes {
        if i.definition.trim().is_empty() || !i.columns.is_empty() {
            known(&i.columns, &format!("index `{}`", i.name))?;
        }
    }
    for f in &t.foreign_keys {
        known(&f.columns, &format!("foreign key `{}`", f.name))?;
        check_name("the referenced table", &f.ref_table)?;
        if f.ref_columns.len() != f.columns.len() {
            return Err(DbError::Config(format!(
                "foreign key `{}`: {} column(s) reference {} column(s)",
                f.name,
                f.columns.len(),
                f.ref_columns.len()
            )));
        }
    }
    for ch in &t.checks {
        if ch.expression.trim().is_empty() {
            return Err(DbError::Config(format!("check `{}` needs a condition", ch.name)));
        }
        check_fragment("check", &ch.expression)?;
    }
    if kind == DbKind::Sqlite {
        for c in t.columns.iter().filter(|c| c.auto_increment) {
            let integer = c.data_type.trim().eq_ignore_ascii_case("integer");
            if !integer || t.primary_key.as_slice() != std::slice::from_ref(&c.name) {
                return Err(DbError::Config(format!(
                    "SQLite auto-increment needs `{}` to be the only primary key column, of type INTEGER",
                    c.name
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- pieces

fn q(kind: DbKind, name: &str) -> String {
    quote_ident(kind, name)
}

fn q_list(kind: DbKind, cols: &[String]) -> String {
    cols.iter().map(|c| q(kind, c)).collect::<Vec<_>>().join(", ")
}

fn text(kind: DbKind, s: &str) -> DbResult<String> {
    literal(kind, &Cell::Text(s.to_string()))
}

fn table_name(kind: DbKind, t: &TableDef) -> String {
    qualified(kind, &t.schema, &t.name)
}

/// `<table>_<cols>_<suffix>` for unnamed indexes and foreign keys.
fn generated_name(table: &str, cols: &[String], suffix: &str) -> String {
    let mut parts = vec![table.to_string()];
    parts.extend(cols.iter().cloned());
    parts.push(suffix.to_string());
    let mut name = parts.join("_");
    // PostgreSQL truncates names at 63 bytes; keep generated ones well inside every limit.
    while name.len() > 60 {
        name.pop();
    }
    name
}

fn index_name(t: &TableDef, i: &IndexDef) -> String {
    if i.name.trim().is_empty() {
        generated_name(&t.name, &i.columns, if i.unique { "uindex" } else { "index" })
    } else {
        i.name.clone()
    }
}

fn fk_name(t: &TableDef, f: &ForeignKeyDef) -> String {
    if f.name.trim().is_empty() { generated_name(&t.name, &f.columns, "fk") } else { f.name.clone() }
}

/// A SQLite default must be a literal or a parenthesized expression.
fn sqlite_default(d: &str) -> String {
    let d = d.trim();
    let upper = d.to_ascii_uppercase();
    let simple = d.starts_with('(')
        || d.starts_with('\'')
        || d.starts_with('"')
        || d.parse::<f64>().is_ok()
        || matches!(upper.as_str(), "NULL" | "TRUE" | "FALSE" | "CURRENT_TIME" | "CURRENT_DATE" | "CURRENT_TIMESTAMP")
        || (upper.starts_with("X'") && d.ends_with('\''));
    if simple { d.to_string() } else { format!("({d})") }
}

fn is_inline_sqlite_autoinc(kind: DbKind, t: &TableDef) -> Option<&str> {
    if kind != DbKind::Sqlite {
        return None;
    }
    let c = t.columns.iter().find(|c| c.auto_increment)?;
    (t.primary_key.as_slice() == std::slice::from_ref(&c.name)).then_some(c.name.as_str())
}

/// One column's definition inside CREATE TABLE / ADD COLUMN.
pub fn column_sql(kind: DbKind, c: &ColumnDef) -> DbResult<String> {
    check_name("a column", &c.name)?;
    check_type(c)?;
    let mut s = format!("{} {}", q(kind, &c.name), c.data_type.trim());
    let default = c.default.trim();
    if !default.is_empty() {
        check_fragment(&format!("default of `{}`", c.name), default)?;
    }
    match kind {
        DbKind::Postgres => {
            if c.auto_increment {
                s.push_str(" GENERATED BY DEFAULT AS IDENTITY");
            }
            if !default.is_empty() {
                s.push_str(&format!(" DEFAULT {default}"));
            }
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
        }
        DbKind::Mysql | DbKind::Mariadb => {
            s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            if !default.is_empty() {
                s.push_str(&format!(" DEFAULT {default}"));
            }
            if c.auto_increment {
                s.push_str(" AUTO_INCREMENT");
            }
            if !c.comment.is_empty() {
                s.push_str(&format!(" COMMENT {}", text(kind, &c.comment)?));
            }
        }
        DbKind::Sqlite => {
            if c.auto_increment {
                s.push_str(" PRIMARY KEY AUTOINCREMENT");
            }
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
            if !default.is_empty() {
                s.push_str(&format!(" DEFAULT {}", sqlite_default(default)));
            }
        }
    }
    Ok(s)
}

fn pk_clause(kind: DbKind, t: &TableDef) -> String {
    let mut s = String::new();
    if !t.primary_key_name.trim().is_empty() && kind != DbKind::Mysql && kind != DbKind::Mariadb {
        s.push_str(&format!("CONSTRAINT {} ", q(kind, &t.primary_key_name)));
    }
    s.push_str(&format!("PRIMARY KEY ({})", q_list(kind, &t.primary_key)));
    s
}

fn fk_clause(kind: DbKind, t: &TableDef, f: &ForeignKeyDef) -> DbResult<String> {
    check_name("the referenced table", &f.ref_table)?;
    let ref_schema = if f.ref_schema.is_empty() { t.schema.as_str() } else { f.ref_schema.as_str() };
    // SQLite references are unqualified: the referenced table is in the same database.
    let target = match kind {
        DbKind::Sqlite => q(kind, &f.ref_table),
        _ => qualified(kind, ref_schema, &f.ref_table),
    };
    let mut s = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {target} ({})",
        q(kind, &fk_name(t, f)),
        q_list(kind, &f.columns),
        q_list(kind, &f.ref_columns)
    );
    if f.on_delete != FkAction::NoAction {
        s.push_str(&format!(" ON DELETE {}", f.on_delete.sql()));
    }
    if f.on_update != FkAction::NoAction {
        s.push_str(&format!(" ON UPDATE {}", f.on_update.sql()));
    }
    Ok(s)
}

/// `CONSTRAINT name FOREIGN KEY (…) REFERENCES …` for a foreign key of `t`.
pub fn foreign_key_clause(kind: DbKind, t: &TableDef, f: &ForeignKeyDef) -> DbResult<String> {
    fk_clause(kind, t, f)
}

fn check_clause(kind: DbKind, c: &CheckDef) -> DbResult<String> {
    check_fragment("check", &c.expression)?;
    let named = if c.name.trim().is_empty() { String::new() } else { format!("CONSTRAINT {} ", q(kind, &c.name)) };
    Ok(format!("{named}CHECK ({})", c.expression.trim()))
}

fn unique_clause(kind: DbKind, t: &TableDef, i: &IndexDef) -> String {
    format!("CONSTRAINT {} UNIQUE ({})", q(kind, &index_name(t, i)), q_list(kind, &i.columns))
}

/// `CREATE [UNIQUE] INDEX` for an index of `t`.
pub fn create_index(kind: DbKind, t: &TableDef, i: &IndexDef) -> DbResult<String> {
    if i.columns.is_empty() {
        let def = i.definition.trim().trim_end_matches(';').trim_end();
        if def.is_empty() {
            return Err(DbError::Config(format!("index `{}` needs at least one column", i.name)));
        }
        // Expression or partial index read from the catalog: keep its own definition.
        return Ok(def.to_string());
    }
    let name = index_name(t, i);
    let unique = if i.unique { "UNIQUE " } else { "" };
    Ok(match kind {
        DbKind::Sqlite => format!(
            "CREATE {unique}INDEX {} ON {} ({})",
            qualified(kind, &t.schema, &name),
            q(kind, &t.name),
            q_list(kind, &i.columns)
        ),
        _ => {
            format!("CREATE {unique}INDEX {} ON {} ({})", q(kind, &name), table_name(kind, t), q_list(kind, &i.columns))
        }
    })
}

fn drop_index_sql(kind: DbKind, t: &TableDef, i: &IndexDef) -> String {
    let name = index_name(t, i);
    match kind {
        DbKind::Mysql | DbKind::Mariadb => format!("DROP INDEX {} ON {}", q(kind, &name), table_name(kind, t)),
        DbKind::Postgres if i.constraint => {
            format!("ALTER TABLE {} DROP CONSTRAINT {}", table_name(kind, t), q(kind, &name))
        }
        _ => format!("DROP INDEX {}", qualified(kind, &t.schema, &name)),
    }
}

fn comment_statements(kind: DbKind, t: &TableDef, out: &mut Vec<String>) -> DbResult<()> {
    if kind != DbKind::Postgres {
        return Ok(());
    }
    let name = table_name(kind, t);
    if !t.comment.is_empty() {
        out.push(format!("COMMENT ON TABLE {name} IS {}", text(kind, &t.comment)?));
    }
    for c in t.columns.iter().filter(|c| !c.comment.is_empty()) {
        out.push(format!("COMMENT ON COLUMN {name}.{} IS {}", q(kind, &c.name), text(kind, &c.comment)?));
    }
    Ok(())
}

fn pg_name_warnings(kind: DbKind, t: &TableDef, warnings: &mut Vec<String>) {
    if kind != DbKind::Postgres {
        return;
    }
    let names = std::iter::once(&t.name).chain(t.columns.iter().map(|c| &c.name));
    for n in names {
        if n.len() > 63 {
            warnings.push(format!("PostgreSQL truncates `{n}` to 63 bytes"));
        }
    }
}

/// The body of `CREATE TABLE name (…)`: columns, primary key, unique constraints, foreign keys
/// and checks, one per line.
fn create_table_sql(kind: DbKind, t: &TableDef) -> DbResult<String> {
    let mut lines = Vec::new();
    for c in &t.columns {
        lines.push(column_sql(kind, c)?);
    }
    if !t.primary_key.is_empty() && is_inline_sqlite_autoinc(kind, t).is_none() {
        lines.push(pk_clause(kind, t));
    }
    for i in t.indexes.iter().filter(|i| i.constraint && !i.columns.is_empty()) {
        lines.push(unique_clause(kind, t, i));
    }
    for f in &t.foreign_keys {
        lines.push(fk_clause(kind, t, f)?);
    }
    for c in &t.checks {
        lines.push(check_clause(kind, c)?);
    }
    let mut s = format!("CREATE TABLE {} (\n    {}\n)", table_name(kind, t), lines.join(",\n    "));
    if matches!(kind, DbKind::Mysql | DbKind::Mariadb) && !t.comment.is_empty() {
        s.push_str(&format!(" COMMENT = {}", text(kind, &t.comment)?));
    }
    Ok(s)
}

/// `CREATE TABLE` with its indexes and comments.
pub fn create_table(kind: DbKind, t: &TableDef) -> DbResult<DdlScript> {
    validate(kind, t)?;
    let mut script = DdlScript::new(kind);
    script.statements.push(create_table_sql(kind, t)?);
    for i in t.indexes.iter().filter(|i| !i.constraint) {
        script.statements.push(create_index(kind, t, i)?);
    }
    comment_statements(kind, t, &mut script.statements)?;
    if kind == DbKind::Sqlite && (!t.comment.is_empty() || t.columns.iter().any(|c| !c.comment.is_empty())) {
        script.warnings.push("SQLite has no comments; they are not saved".into());
    }
    pg_name_warnings(kind, t, &mut script.warnings);
    Ok(script)
}

// ---------------------------------------------------------------- alter

/// Rename map old name → new name for columns that keep their identity.
fn renames(old: &TableDef, new: &TableDef) -> DbResult<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for c in &new.columns {
        if c.original.is_empty() {
            continue;
        }
        if old.column(&c.original).is_none() {
            return Err(DbError::Config(format!("the table has no column `{}` any more; reload it", c.original)));
        }
        if map.insert(c.original.clone(), c.name.clone()).is_some() {
            return Err(DbError::Config(format!("column `{}` appears twice", c.original)));
        }
    }
    Ok(map)
}

/// The new definition with key and index columns that still use a renamed column's old name
/// pointing at its new name.
fn follow_renames(new: &TableDef, map: &BTreeMap<String, String>) -> TableDef {
    let mut t = new.clone();
    let names: BTreeSet<String> = t.columns.iter().map(|c| c.name.clone()).collect();
    let fix = |cols: &mut Vec<String>| {
        for c in cols.iter_mut() {
            if !names.contains(c.as_str())
                && let Some(n) = map.get(c.as_str())
            {
                *c = n.clone();
            }
        }
    };
    fix(&mut t.primary_key);
    for i in &mut t.indexes {
        fix(&mut i.columns);
    }
    for f in &mut t.foreign_keys {
        fix(&mut f.columns);
    }
    t
}

fn mapped(cols: &[String], map: &BTreeMap<String, String>) -> Vec<String> {
    cols.iter().map(|c| map.get(c).cloned().unwrap_or_else(|| c.clone())).collect()
}

fn same_index(a: &IndexDef, b: &IndexDef, map: &BTreeMap<String, String>) -> bool {
    a.unique == b.unique && a.constraint == b.constraint && mapped(&a.columns, map) == b.columns
}

fn same_fk(a: &ForeignKeyDef, b: &ForeignKeyDef, map: &BTreeMap<String, String>, schema: &str) -> bool {
    let rs = |s: &str| if s.is_empty() { schema.to_string() } else { s.to_string() };
    mapped(&a.columns, map) == b.columns
        && rs(&a.ref_schema) == rs(&b.ref_schema)
        && a.ref_table == b.ref_table
        && a.ref_columns == b.ref_columns
        && a.on_delete == b.on_delete
        && a.on_update == b.on_update
}

fn norm_type(t: &str) -> String {
    t.split_whitespace().collect::<Vec<_>>().join(" ").to_ascii_lowercase().replace(", ", ",")
}

/// What changed between two definitions (shared by every dialect).
struct Changes<'a> {
    map: BTreeMap<String, String>,
    new: TableDef,
    dropped_columns: Vec<&'a ColumnDef>,
    dropped_indexes: Vec<&'a IndexDef>,
    added_indexes: Vec<IndexDef>,
    dropped_fks: Vec<&'a ForeignKeyDef>,
    added_fks: Vec<ForeignKeyDef>,
    dropped_checks: Vec<&'a CheckDef>,
    added_checks: Vec<CheckDef>,
    pk_changed: bool,
}

impl<'a> Changes<'a> {
    fn of(old: &'a TableDef, new: &TableDef) -> DbResult<Self> {
        let map = renames(old, new)?;
        let new = follow_renames(new, &map);
        let dropped_columns: Vec<&ColumnDef> = old.columns.iter().filter(|c| !map.contains_key(&c.name)).collect();
        let named = |n: &str| !n.trim().is_empty();
        let dropped_indexes: Vec<&IndexDef> = old
            .indexes
            .iter()
            .filter(|o| !new.indexes.iter().any(|n| named(&n.name) && n.name == o.name && same_index(o, n, &map)))
            .collect();
        let added_indexes: Vec<IndexDef> = new
            .indexes
            .iter()
            .filter(|n| !old.indexes.iter().any(|o| named(&n.name) && n.name == o.name && same_index(o, n, &map)))
            .cloned()
            .collect();
        let dropped_fks: Vec<&ForeignKeyDef> = old
            .foreign_keys
            .iter()
            .filter(|o| {
                !new.foreign_keys.iter().any(|n| named(&n.name) && n.name == o.name && same_fk(o, n, &map, &old.schema))
            })
            .collect();
        let added_fks: Vec<ForeignKeyDef> = new
            .foreign_keys
            .iter()
            .filter(|n| {
                !old.foreign_keys.iter().any(|o| named(&n.name) && n.name == o.name && same_fk(o, n, &map, &old.schema))
            })
            .cloned()
            .collect();
        let same_check =
            |a: &CheckDef, b: &CheckDef| a.name == b.name && norm_type(&a.expression) == norm_type(&b.expression);
        let dropped_checks: Vec<&CheckDef> =
            old.checks.iter().filter(|o| !new.checks.iter().any(|n| same_check(o, n))).collect();
        let added_checks: Vec<CheckDef> =
            new.checks.iter().filter(|n| !old.checks.iter().any(|o| same_check(o, n))).cloned().collect();
        let pk_changed = mapped(&old.primary_key, &map) != new.primary_key;
        Ok(Self {
            map,
            new,
            dropped_columns,
            dropped_indexes,
            added_indexes,
            dropped_fks,
            added_fks,
            dropped_checks,
            added_checks,
            pk_changed,
        })
    }
}

/// The statements that turn table `old` (as introspected) into `new` (as edited). Columns of
/// `new` with an `original` keep their data (renamed when the name changed); old columns no new
/// column claims are dropped.
pub fn alter_table(kind: DbKind, old: &TableDef, new: &TableDef) -> DbResult<DdlScript> {
    let ch = Changes::of(old, new)?;
    validate(kind, &ch.new)?;
    let mut script = match kind {
        DbKind::Postgres => alter_pg(old, &ch)?,
        DbKind::Mysql | DbKind::Mariadb => alter_my(kind, old, &ch)?,
        DbKind::Sqlite => alter_sqlite(old, &ch)?,
    };
    pg_name_warnings(kind, &ch.new, &mut script.warnings);
    Ok(script)
}

fn alter_pg(old: &TableDef, ch: &Changes<'_>) -> DbResult<DdlScript> {
    let kind = DbKind::Postgres;
    let new = &ch.new;
    let mut s = DdlScript::new(kind);
    let old_name = table_name(kind, old);
    let mut out: Vec<String> = Vec::new();
    for f in &ch.dropped_fks {
        out.push(format!("ALTER TABLE {old_name} DROP CONSTRAINT {}", q(kind, &fk_name(old, f))));
    }
    for c in &ch.dropped_checks {
        if c.name.is_empty() {
            s.warnings.push(format!("an unnamed check ({}) cannot be dropped by name", c.expression));
            continue;
        }
        out.push(format!("ALTER TABLE {old_name} DROP CONSTRAINT {}", q(kind, &c.name)));
    }
    for i in &ch.dropped_indexes {
        out.push(drop_index_sql(kind, old, i));
    }
    if ch.pk_changed && !old.primary_key.is_empty() {
        let name =
            if old.primary_key_name.is_empty() { format!("{}_pkey", old.name) } else { old.primary_key_name.clone() };
        out.push(format!("ALTER TABLE {old_name} DROP CONSTRAINT {}", q(kind, &name)));
    }
    if old.name != new.name {
        out.push(format!("ALTER TABLE {old_name} RENAME TO {}", q(kind, &new.name)));
    }
    let name = qualified(kind, &old.schema, &new.name);
    for (from, to) in ch.map.iter().filter(|(a, b)| a != b) {
        out.push(format!("ALTER TABLE {name} RENAME COLUMN {} TO {}", q(kind, from), q(kind, to)));
    }
    for c in &ch.dropped_columns {
        out.push(format!("ALTER TABLE {name} DROP COLUMN {}", q(kind, &c.name)));
    }
    for c in &new.columns {
        let col = q(kind, &c.name);
        let Some(o) = (!c.original.is_empty()).then(|| old.column(&c.original)).flatten() else {
            out.push(format!("ALTER TABLE {name} ADD COLUMN {}", column_sql(kind, c)?));
            if !c.comment.is_empty() {
                out.push(format!("COMMENT ON COLUMN {name}.{col} IS {}", text(kind, &c.comment)?));
            }
            continue;
        };
        check_type(c)?;
        if norm_type(&o.data_type) != norm_type(&c.data_type) {
            let ty = c.data_type.trim();
            out.push(format!("ALTER TABLE {name} ALTER COLUMN {col} TYPE {ty} USING {col}::{ty}"));
        }
        if o.auto_increment && !c.auto_increment {
            out.push(format!("ALTER TABLE {name} ALTER COLUMN {col} DROP IDENTITY IF EXISTS"));
        }
        if o.default.trim() != c.default.trim() {
            if c.default.trim().is_empty() {
                out.push(format!("ALTER TABLE {name} ALTER COLUMN {col} DROP DEFAULT"));
            } else {
                check_fragment(&format!("default of `{}`", c.name), &c.default)?;
                out.push(format!("ALTER TABLE {name} ALTER COLUMN {col} SET DEFAULT {}", c.default.trim()));
            }
        }
        if !o.auto_increment && c.auto_increment {
            out.push(format!("ALTER TABLE {name} ALTER COLUMN {col} ADD GENERATED BY DEFAULT AS IDENTITY"));
        }
        if o.nullable != c.nullable {
            out.push(format!(
                "ALTER TABLE {name} ALTER COLUMN {col} {} NOT NULL",
                if c.nullable { "DROP" } else { "SET" }
            ));
        }
        if o.comment != c.comment {
            let v = if c.comment.is_empty() { "NULL".to_string() } else { text(kind, &c.comment)? };
            out.push(format!("COMMENT ON COLUMN {name}.{col} IS {v}"));
        }
    }
    if ch.pk_changed && !new.primary_key.is_empty() {
        out.push(format!("ALTER TABLE {name} ADD {}", pk_clause(kind, new)));
    }
    for c in &ch.added_checks {
        out.push(format!("ALTER TABLE {name} ADD {}", check_clause(kind, c)?));
    }
    let t_new = TableDef { schema: old.schema.clone(), ..new.clone() };
    for i in &ch.added_indexes {
        if i.constraint {
            out.push(format!("ALTER TABLE {name} ADD {}", unique_clause(kind, &t_new, i)));
        } else {
            out.push(create_index(kind, &t_new, i)?);
        }
    }
    for f in &ch.added_fks {
        out.push(format!("ALTER TABLE {name} ADD {}", fk_clause(kind, &t_new, f)?));
    }
    if old.comment != new.comment {
        let v = if new.comment.is_empty() { "NULL".to_string() } else { text(kind, &new.comment)? };
        out.push(format!("COMMENT ON TABLE {name} IS {v}"));
    }
    // Views block type changes and drops of the columns they use: recreate them around it.
    let retyped = new
        .columns
        .iter()
        .any(|c| old.column(&c.original).is_some_and(|o| norm_type(&o.data_type) != norm_type(&c.data_type)));
    if (retyped || !ch.dropped_columns.is_empty()) && !old.dependent_views.is_empty() {
        let renamed: BTreeMap<String, String> =
            ch.map.iter().filter(|(a, b)| a != b).map(|(a, b)| (a.clone(), b.clone())).collect();
        let drops = old.dependent_views.iter().rev().map(|v| v.drop.clone());
        let creates = old.dependent_views.iter().map(|v| rename_identifiers(kind, &v.create, &renamed));
        out = drops.chain(out).chain(creates).collect();
        let names: Vec<&str> = old.dependent_views.iter().map(|v| v.name.as_str()).collect();
        s.warnings.push(format!("Views using the table are dropped and recreated: {}", names.join(", ")));
    }
    s.statements = out;
    Ok(s)
}

fn alter_my(kind: DbKind, old: &TableDef, ch: &Changes<'_>) -> DbResult<DdlScript> {
    let new = &ch.new;
    let mut s = DdlScript::new(kind);
    let old_name = table_name(kind, old);
    let mut out: Vec<String> = Vec::new();
    for f in &ch.dropped_fks {
        out.push(format!("ALTER TABLE {old_name} DROP FOREIGN KEY {}", q(kind, &fk_name(old, f))));
    }
    for c in &ch.dropped_checks {
        if c.name.is_empty() {
            s.warnings.push(format!("an unnamed check ({}) cannot be dropped by name", c.expression));
            continue;
        }
        let how = if kind == DbKind::Mariadb { "CONSTRAINT" } else { "CHECK" };
        out.push(format!("ALTER TABLE {old_name} DROP {how} {}", q(kind, &c.name)));
    }
    for i in &ch.dropped_indexes {
        out.push(drop_index_sql(kind, old, i));
    }
    if old.name != new.name {
        out.push(format!("RENAME TABLE {old_name} TO {}", qualified(kind, &old.schema, &new.name)));
    }
    let name = qualified(kind, &old.schema, &new.name);
    if ch.pk_changed && !old.primary_key.is_empty() {
        // An AUTO_INCREMENT column must stay a key: drop the attribute first when the key moves.
        for c in old.columns.iter().filter(|c| c.auto_increment) {
            if let Some(n) = new.columns.iter().find(|n| n.original == c.name) {
                let plain = ColumnDef { auto_increment: false, ..n.clone() };
                out.push(format!(
                    "ALTER TABLE {name} CHANGE COLUMN {} {}",
                    q(kind, &c.name),
                    column_sql(kind, &ColumnDef { name: c.name.clone(), ..plain })?
                ));
            }
        }
        out.push(format!("ALTER TABLE {name} DROP PRIMARY KEY"));
    }
    for c in &ch.dropped_columns {
        out.push(format!("ALTER TABLE {name} DROP COLUMN {}", q(kind, &c.name)));
    }
    let mut prev: Option<&str> = None;
    for c in &new.columns {
        let position = match prev {
            None => " FIRST".to_string(),
            Some(p) => format!(" AFTER {}", q(kind, p)),
        };
        prev = Some(&c.name);
        let Some(o) = (!c.original.is_empty()).then(|| old.column(&c.original)).flatten() else {
            out.push(format!("ALTER TABLE {name} ADD COLUMN {}{position}", column_sql(kind, c)?));
            continue;
        };
        let changed = o.name != c.name
            || norm_type(&o.data_type) != norm_type(&c.data_type)
            || o.nullable != c.nullable
            || o.default.trim() != c.default.trim()
            || o.auto_increment != c.auto_increment
            || o.comment != c.comment;
        if changed {
            let def = if ch.pk_changed && c.auto_increment && new.primary_key.contains(&c.name) {
                // AUTO_INCREMENT is restored once the new key exists.
                column_sql(kind, &ColumnDef { auto_increment: false, ..c.clone() })?
            } else {
                column_sql(kind, c)?
            };
            out.push(format!("ALTER TABLE {name} CHANGE COLUMN {} {def}", q(kind, &o.name)));
        }
    }
    if ch.pk_changed && !new.primary_key.is_empty() {
        out.push(format!("ALTER TABLE {name} ADD {}", pk_clause(kind, new)));
        for c in new.columns.iter().filter(|c| c.auto_increment && new.primary_key.contains(&c.name)) {
            out.push(format!("ALTER TABLE {name} MODIFY COLUMN {}", column_sql(kind, c)?));
        }
    }
    for c in &ch.added_checks {
        out.push(format!("ALTER TABLE {name} ADD {}", check_clause(kind, c)?));
    }
    let t_new = TableDef { schema: old.schema.clone(), ..new.clone() };
    for i in &ch.added_indexes {
        out.push(create_index(kind, &t_new, i)?);
    }
    for f in &ch.added_fks {
        out.push(format!("ALTER TABLE {name} ADD {}", fk_clause(kind, &t_new, f)?));
    }
    if old.comment != new.comment {
        out.push(format!("ALTER TABLE {name} COMMENT = {}", text(kind, &new.comment)?));
    }
    s.statements = out;
    Ok(s)
}

/// SQLite alters in place when it can (rename table/columns, add plain columns, drop unindexed
/// columns, indexes); anything else rebuilds the table.
fn alter_sqlite(old: &TableDef, ch: &Changes<'_>) -> DbResult<DdlScript> {
    let kind = DbKind::Sqlite;
    let new = &ch.new;
    let mut s = DdlScript::new(kind);
    if (!new.comment.is_empty() && new.comment != old.comment)
        || new
            .columns
            .iter()
            .any(|c| !c.comment.is_empty() && old.column(&c.original).is_none_or(|o| o.comment != c.comment))
    {
        s.warnings.push("SQLite has no comments; they are not saved".into());
    }
    let in_keys = |name: &str| {
        old.primary_key.iter().any(|c| c == name)
            || old.indexes.iter().any(|i| i.columns.iter().any(|c| c == name) || i.columns.is_empty())
            || old.foreign_keys.iter().any(|f| f.columns.iter().any(|c| c == name))
            || !old.checks.is_empty()
    };
    let column_changed = new.columns.iter().any(|c| match old.column(&c.original) {
        Some(o) => {
            norm_type(&o.data_type) != norm_type(&c.data_type)
                || o.nullable != c.nullable
                || o.default.trim() != c.default.trim()
                || o.auto_increment != c.auto_increment
        }
        None => {
            c.auto_increment
                || new.primary_key.contains(&c.name)
                || (!c.nullable && c.default.trim().is_empty())
                || new.indexes.iter().any(|i| i.constraint && i.columns.contains(&c.name))
                || new.foreign_keys.iter().any(|f| f.columns.contains(&c.name))
        }
    });
    // Added columns go last; any other order needs a rebuild.
    let kept: Vec<&str> = new.columns.iter().filter(|c| !c.original.is_empty()).map(|c| c.original.as_str()).collect();
    let old_order: Vec<&str> =
        old.columns.iter().filter(|c| ch.map.contains_key(&c.name)).map(|c| c.name.as_str()).collect();
    let first_added = new.columns.iter().position(|c| c.original.is_empty()).unwrap_or(new.columns.len());
    let reordered = kept != old_order || new.columns.iter().skip(first_added).any(|c| !c.original.is_empty());
    let constraint_index_changed =
        ch.dropped_indexes.iter().any(|i| i.constraint || (i.columns.is_empty() && i.definition.is_empty()))
            || ch.added_indexes.iter().any(|i| i.constraint);
    let rebuild = column_changed
        || reordered
        || ch.pk_changed
        || !ch.dropped_fks.is_empty()
        || !ch.added_fks.is_empty()
        || !ch.dropped_checks.is_empty()
        || !ch.added_checks.is_empty()
        || constraint_index_changed
        || ch.dropped_columns.iter().any(|c| in_keys(&c.name));
    if rebuild {
        return sqlite_rebuild(old, ch, s);
    }
    let old_name = table_name(kind, old);
    let mut out = Vec::new();
    for i in &ch.dropped_indexes {
        out.push(drop_index_sql(kind, old, i));
    }
    if old.name != new.name {
        out.push(format!("ALTER TABLE {old_name} RENAME TO {}", q(kind, &new.name)));
    }
    let name = qualified(kind, &old.schema, &new.name);
    for (from, to) in ch.map.iter().filter(|(a, b)| a != b) {
        out.push(format!("ALTER TABLE {name} RENAME COLUMN {} TO {}", q(kind, from), q(kind, to)));
    }
    for c in &ch.dropped_columns {
        out.push(format!("ALTER TABLE {name} DROP COLUMN {}", q(kind, &c.name)));
    }
    for c in new.columns.iter().filter(|c| c.original.is_empty()) {
        out.push(format!("ALTER TABLE {name} ADD COLUMN {}", column_sql(kind, c)?));
    }
    let t_new = TableDef { schema: old.schema.clone(), ..new.clone() };
    for i in &ch.added_indexes {
        out.push(create_index(kind, &t_new, i)?);
    }
    s.statements = out;
    Ok(s)
}

/// The SQLite rebuild: create the new table under a temporary name, copy the rows, drop the old
/// table, rename, recreate indexes and triggers, check foreign keys; foreign keys off around it.
fn sqlite_rebuild(old: &TableDef, ch: &Changes<'_>, mut s: DdlScript) -> DbResult<DdlScript> {
    let kind = DbKind::Sqlite;
    let new = &ch.new;
    let mut tmp_name = format!("codecraft_new_{}", new.name);
    while tmp_name == old.name || tmp_name == new.name {
        tmp_name.push('_');
    }
    let tmp = TableDef { schema: old.schema.clone(), name: tmp_name.clone(), ..new.clone() };
    s.setup.push("PRAGMA foreign_keys = OFF".into());
    // Renames below must update views and triggers (the modern behaviour).
    s.setup.push("PRAGMA legacy_alter_table = OFF".into());
    // Renamed columns are renamed in place first, so SQLite updates the views and triggers that
    // use them; then the rebuild copies by the new names. A rename onto a name that still exists
    // (swaps) cannot, and copies across instead.
    let renamed: BTreeMap<String, String> =
        ch.map.iter().filter(|(a, b)| a != b).map(|(a, b)| (a.clone(), b.clone())).collect();
    let collides = renamed.iter().any(|(from, to)| old.columns.iter().any(|c| &c.name == to && &c.name != from));
    let in_place = !renamed.is_empty() && !collides;
    let source = table_name(kind, old);
    let mut tmp = tmp;
    for c in &mut tmp.checks {
        c.expression = rename_identifiers(kind, &c.expression, &renamed);
    }
    if in_place {
        for (from, to) in &renamed {
            s.statements.push(format!("ALTER TABLE {source} RENAME COLUMN {} TO {}", q(kind, from), q(kind, to)));
        }
    }
    s.statements.push(create_table_sql(kind, &tmp)?);
    let copied: Vec<(&str, &str)> = new
        .columns
        .iter()
        .filter(|c| !c.original.is_empty())
        .map(|c| (c.name.as_str(), if in_place { c.name.as_str() } else { c.original.as_str() }))
        .collect();
    if !copied.is_empty() {
        s.statements.push(format!(
            "INSERT INTO {} ({}) SELECT {} FROM {source}",
            table_name(kind, &tmp),
            copied.iter().map(|(n, _)| q(kind, n)).collect::<Vec<_>>().join(", "),
            copied.iter().map(|(_, o)| q(kind, o)).collect::<Vec<_>>().join(", "),
        ));
    }
    s.statements.push(format!("DROP TABLE {source}"));
    // The views that use the table point nowhere until the rename: the legacy rename does not
    // check them.
    s.statements.push("PRAGMA legacy_alter_table = ON".into());
    s.statements.push(format!("ALTER TABLE {} RENAME TO {}", table_name(kind, &tmp), q(kind, &new.name)));
    s.statements.push("PRAGMA legacy_alter_table = OFF".into());
    let t_new = TableDef { schema: old.schema.clone(), ..new.clone() };
    for i in t_new.indexes.iter().filter(|i| !i.constraint) {
        let mut i = i.clone();
        i.definition = rename_identifiers(kind, &i.definition, &renamed);
        s.statements.push(create_index(kind, &t_new, &i)?);
    }
    for trig in &old.triggers {
        let t = rename_identifiers(kind, trig.trim().trim_end_matches(';').trim_end(), &renamed);
        if !t.is_empty() {
            s.statements.push(t);
        }
    }
    let check = if old.schema.is_empty() {
        format!("PRAGMA foreign_key_check({})", q(kind, &new.name))
    } else {
        format!("PRAGMA {}.foreign_key_check({})", q(kind, &old.schema), q(kind, &new.name))
    };
    s.statements.push(check);
    s.cleanup.push("PRAGMA foreign_keys = ON".into());
    s.cleanup.push("PRAGMA legacy_alter_table = OFF".into());
    let lost: Vec<&str> = ch.dropped_columns.iter().map(|c| c.name.as_str()).collect();
    let mut note =
        String::from("SQLite cannot make these changes in place: the table is rebuilt (rows copied into a new table)");
    if !lost.is_empty() {
        note.push_str(&format!("; the data of {} is dropped", lost.join(", ")));
    }
    s.warnings.insert(0, note);
    if !old.triggers.is_empty() && !renamed.is_empty() {
        s.warnings.push("Triggers are recreated with renamed columns replaced: check them".into());
    }
    Ok(s)
}

/// Replace identifiers (bare or quoted, outside strings and comments) named in `map` by the new
/// names, quoted for `kind` when needed. For recreating triggers, views and expression indexes
/// after column renames.
pub fn rename_identifiers(kind: DbKind, sql: &str, map: &BTreeMap<String, String>) -> String {
    if map.is_empty() {
        return sql.to_string();
    }
    let lookup = |name: &str| -> Option<&String> {
        map.get(name).or_else(|| map.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v))
    };
    let bare_ok = |n: &str| {
        let mut chars = n.chars();
        chars.next().is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let next = chars.get(i + 1).copied();
        if c == '\'' {
            // A string literal: copied as is.
            out.push(c);
            i += 1;
            while let Some(&d) = chars.get(i) {
                out.push(d);
                i += 1;
                if d == '\'' {
                    if chars.get(i) == Some(&'\'') {
                        out.push('\'');
                        i += 1;
                    } else {
                        break;
                    }
                }
            }
        } else if c == '-' && next == Some('-') {
            while let Some(&d) = chars.get(i) {
                out.push(d);
                i += 1;
                if d == '\n' {
                    break;
                }
            }
        } else if c == '/' && next == Some('*') {
            out.push_str("/*");
            i += 2;
            while let Some(&d) = chars.get(i) {
                if d == '*' && chars.get(i + 1) == Some(&'/') {
                    out.push_str("*/");
                    i += 2;
                    break;
                }
                out.push(d);
                i += 1;
            }
        } else if c == '"' || c == '`' {
            let start = i;
            let mut name = String::new();
            i += 1;
            while let Some(&d) = chars.get(i) {
                i += 1;
                if d == c {
                    if chars.get(i) == Some(&c) {
                        name.push(c);
                        i += 1;
                        continue;
                    }
                    break;
                }
                name.push(d);
            }
            match map.get(&name) {
                Some(n) => out.push_str(&quote_ident(kind, n)),
                None => out.extend(chars.get(start..i).unwrap_or_default()),
            }
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while chars.get(i).is_some_and(|d| d.is_alphanumeric() || *d == '_' || *d == '$') {
                i += 1;
            }
            let word: String = chars.get(start..i).unwrap_or_default().iter().collect();
            match lookup(&word) {
                Some(n) if bare_ok(n) => out.push_str(n),
                Some(n) => out.push_str(&quote_ident(kind, n)),
                None => out.push_str(&word),
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

// ---------------------------------------------------------------- drop / rename / truncate

fn single(kind: DbKind, sql: String) -> DdlScript {
    let mut s = DdlScript::new(kind);
    s.statements.push(sql);
    s
}

fn unsupported<T>(kind: DbKind, what: &str) -> DbResult<T> {
    Err(DbError::Unsupported(format!("{} cannot {what}", kind.label())))
}

fn need_table(o: &ObjectRef) -> DbResult<()> {
    if o.kind.in_table() && o.table.trim().is_empty() {
        return Err(DbError::Config(format!("a {} needs its table", o.kind.label())));
    }
    Ok(())
}

/// `DROP …` for an object. `cascade` drops dependent objects too (PostgreSQL).
pub fn drop_object(kind: DbKind, o: &ObjectRef, cascade: bool) -> DbResult<DdlScript> {
    check_name(&format!("the {}", o.kind.label()), &o.name)?;
    need_table(o)?;
    let pg_cascade = if cascade && kind == DbKind::Postgres { " CASCADE" } else { "" };
    let table = qualified(kind, &o.schema, &o.table);
    let sql = match o.kind {
        ObjectKind::Schema => match kind {
            DbKind::Postgres => format!("DROP SCHEMA {}{pg_cascade}", q(kind, &o.name)),
            DbKind::Mysql | DbKind::Mariadb => format!("DROP DATABASE {}", q(kind, &o.name)),
            DbKind::Sqlite => return unsupported(kind, "drop a schema (detach the database instead)"),
        },
        ObjectKind::Table => format!("DROP TABLE {}{pg_cascade}", qualified(kind, &o.schema, &o.name)),
        ObjectKind::View => format!("DROP VIEW {}{pg_cascade}", qualified(kind, &o.schema, &o.name)),
        ObjectKind::MaterializedView => match kind {
            DbKind::Postgres => format!("DROP MATERIALIZED VIEW {}{pg_cascade}", qualified(kind, &o.schema, &o.name)),
            _ => return unsupported(kind, "have materialized views"),
        },
        ObjectKind::Column => format!("ALTER TABLE {table} DROP COLUMN {}{pg_cascade}", q(kind, &o.name)),
        ObjectKind::Index => match kind {
            DbKind::Mysql | DbKind::Mariadb => format!("DROP INDEX {} ON {table}", q(kind, &o.name)),
            _ => format!("DROP INDEX {}{pg_cascade}", qualified(kind, &o.schema, &o.name)),
        },
        ObjectKind::ForeignKey => match kind {
            DbKind::Postgres => format!("ALTER TABLE {table} DROP CONSTRAINT {}", q(kind, &o.name)),
            DbKind::Mysql | DbKind::Mariadb => format!("ALTER TABLE {table} DROP FOREIGN KEY {}", q(kind, &o.name)),
            DbKind::Sqlite => {
                return Err(DbError::Unsupported(
                    "SQLite cannot drop a foreign key in place: remove it in Modify Table (the table is rebuilt)"
                        .into(),
                ));
            }
        },
    };
    Ok(single(kind, sql))
}

/// Rename an object to `new_name`.
pub fn rename_object(kind: DbKind, o: &ObjectRef, new_name: &str) -> DbResult<DdlScript> {
    check_name(&format!("the {}", o.kind.label()), &o.name)?;
    check_name("the new name", new_name)?;
    need_table(o)?;
    let to = q(kind, new_name);
    let table = qualified(kind, &o.schema, &o.table);
    let my = matches!(kind, DbKind::Mysql | DbKind::Mariadb);
    let sql = match o.kind {
        ObjectKind::Schema => match kind {
            DbKind::Postgres => format!("ALTER SCHEMA {} RENAME TO {to}", q(kind, &o.name)),
            _ => return unsupported(kind, "rename a schema"),
        },
        ObjectKind::Table | ObjectKind::View if my => {
            format!("RENAME TABLE {} TO {}", qualified(kind, &o.schema, &o.name), qualified(kind, &o.schema, new_name))
        }
        ObjectKind::Table => format!("ALTER TABLE {} RENAME TO {to}", qualified(kind, &o.schema, &o.name)),
        ObjectKind::View => match kind {
            DbKind::Postgres => format!("ALTER VIEW {} RENAME TO {to}", qualified(kind, &o.schema, &o.name)),
            _ => return unsupported(kind, "rename a view (drop it and create it again)"),
        },
        ObjectKind::MaterializedView => match kind {
            DbKind::Postgres => {
                format!("ALTER MATERIALIZED VIEW {} RENAME TO {to}", qualified(kind, &o.schema, &o.name))
            }
            _ => return unsupported(kind, "have materialized views"),
        },
        ObjectKind::Column => format!("ALTER TABLE {table} RENAME COLUMN {} TO {to}", q(kind, &o.name)),
        ObjectKind::Index => match kind {
            DbKind::Postgres => format!("ALTER INDEX {} RENAME TO {to}", qualified(kind, &o.schema, &o.name)),
            DbKind::Mysql | DbKind::Mariadb => format!("ALTER TABLE {table} RENAME INDEX {} TO {to}", q(kind, &o.name)),
            DbKind::Sqlite => return unsupported(kind, "rename an index (drop it and create it again)"),
        },
        ObjectKind::ForeignKey => match kind {
            DbKind::Postgres => format!("ALTER TABLE {table} RENAME CONSTRAINT {} TO {to}", q(kind, &o.name)),
            _ => return unsupported(kind, "rename a foreign key (drop it and add it again)"),
        },
    };
    Ok(single(kind, sql))
}

/// Delete every row of a table (`TRUNCATE`; SQLite has `DELETE FROM`).
pub fn truncate_table(kind: DbKind, schema: &str, table: &str, cascade: bool) -> DbResult<DdlScript> {
    check_name("the table", table)?;
    let name = qualified(kind, schema, table);
    let sql = match kind {
        DbKind::Sqlite => format!("DELETE FROM {name}"),
        DbKind::Postgres if cascade => format!("TRUNCATE TABLE {name} CASCADE"),
        _ => format!("TRUNCATE TABLE {name}"),
    };
    let mut s = single(kind, sql);
    if matches!(kind, DbKind::Mysql | DbKind::Mariadb) {
        s.warnings.push("MySQL commits TRUNCATE at once: it cannot be rolled back".into());
    }
    Ok(s)
}

// ---------------------------------------------------------------- SQL generator

/// The DDL of a table as text (CREATE TABLE, indexes, comments), each statement ending with `;`.
pub fn table_ddl(kind: DbKind, t: &TableDef) -> DbResult<String> {
    let mut script = DdlScript::new(kind);
    script.statements.push(create_table_sql(kind, t)?);
    for i in t.indexes.iter().filter(|i| !i.constraint) {
        script.statements.push(create_index(kind, t, i)?);
    }
    comment_statements(kind, t, &mut script.statements)?;
    for trig in &t.triggers {
        let tr = trig.trim().trim_end_matches(';').trim_end();
        if !tr.is_empty() {
            script.statements.push(tr.to_string());
        }
    }
    Ok(join_statements(&script.statements))
}

/// Statements as a script: each ends with `;` and a blank line separates them.
pub fn join_statements(statements: &[String]) -> String {
    statements
        .iter()
        .map(|s| format!("{};\n", s.trim().trim_end_matches(';').trim_end()))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- runner

/// Why [`run_script`] stopped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptFailure {
    /// The failing statement (0-based index into [`DdlScript::statements`]; `None`: setup,
    /// BEGIN or COMMIT).
    pub index: Option<usize>,
    pub sql: String,
    pub error: String,
    /// Statements that stay applied (MySQL commits DDL statement by statement).
    pub applied: Vec<String>,
    /// The transaction was rolled back: nothing changed.
    pub rolled_back: bool,
}

impl ScriptFailure {
    /// The message shown to the user: the error, the statement, and what stays applied.
    pub fn message(&self) -> String {
        let mut m = match self.index {
            Some(i) => format!("Statement {} failed: {}\n  {}", i + 1, self.error, self.sql),
            None => format!("{}\n  {}", self.error, self.sql),
        };
        if self.rolled_back {
            m.push_str("\nThe transaction was rolled back; nothing was changed.");
        } else if self.applied.is_empty() {
            m.push_str("\nNothing was changed.");
        } else {
            m.push_str(&format!(
                "\n{} statement(s) before it were applied and stay applied (DDL commits at once):",
                self.applied.len()
            ));
            for a in &self.applied {
                m.push_str(&format!("\n  {}", a.lines().next().unwrap_or_default()));
            }
        }
        m
    }
}

impl std::fmt::Display for ScriptFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

fn exec(conn: &mut dyn Connection, sql: &str) -> DbResult<Vec<Vec<Cell>>> {
    query_all(conn, sql).map(|(_, rows, _)| rows)
}

/// `PRAGMA name = value` → `name` (plain names only).
fn pragma_setting(sql: &str) -> Option<String> {
    let lower = sql.trim().to_ascii_lowercase();
    let rest = lower.strip_prefix("pragma")?;
    let (name, _) = rest.split_once('=')?;
    let name = name.trim();
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| name.to_string())
}

fn is_fk_check(sql: &str) -> bool {
    let s = sql.trim_start().to_ascii_lowercase();
    s.starts_with("pragma") && s.contains("foreign_key_check")
}

/// Run a script: setup, then the statements (in one transaction when the dialect allows; the
/// first error rolls everything back), then cleanup. Returns the statements run.
pub fn run_script(conn: &mut dyn Connection, script: &DdlScript) -> Result<usize, ScriptFailure> {
    let fail = |index: Option<usize>, sql: &str, error: String, applied: Vec<String>, rolled_back: bool| {
        ScriptFailure { index, sql: sql.to_string(), error, applied, rolled_back }
    };
    // SQLite settings the setup changes go back to what they were, not to a fixed value.
    let mut cleanup = script.cleanup.clone();
    if conn.kind() == DbKind::Sqlite {
        for s in &script.setup {
            let Some(name) = pragma_setting(s) else { continue };
            let current =
                exec(conn, &format!("PRAGMA {name}")).ok().and_then(|r| r.first().and_then(|row| row.first().cloned()));
            if let Some(Cell::Int(v)) = current {
                for c in cleanup.iter_mut().filter(|c| pragma_setting(c).as_deref() == Some(name.as_str())) {
                    *c = format!("PRAGMA {name} = {v}");
                }
            }
        }
    }
    let run_cleanup = |conn: &mut dyn Connection| {
        for c in &cleanup {
            let _ = exec(conn, c);
        }
    };
    for s in &script.setup {
        if let Err(e) = exec(conn, s) {
            run_cleanup(conn);
            return Err(fail(None, s, e.to_string(), Vec::new(), false));
        }
    }
    let tx = script.transactional && !script.statements.is_empty();
    let begin = begin_statement(conn.kind());
    if tx && let Err(e) = exec(conn, begin) {
        run_cleanup(conn);
        return Err(fail(None, begin, format!("cannot start a transaction: {e}"), Vec::new(), false));
    }
    let mut applied: Vec<String> = Vec::new();
    for (i, sql) in script.statements.iter().enumerate() {
        let result = exec(conn, sql).and_then(|rows| {
            if is_fk_check(sql) && !rows.is_empty() {
                let shown: Vec<String> =
                    rows.iter().take(5).map(|r| r.iter().map(Cell::display).collect::<Vec<_>>().join(" ")).collect();
                return Err(DbError::Query(format!(
                    "{} row(s) violate foreign keys (table rowid parent fkid): {}",
                    rows.len(),
                    shown.join("; ")
                )));
            }
            Ok(())
        });
        if let Err(e) = result {
            let rolled_back = tx && exec(conn, "ROLLBACK").is_ok();
            run_cleanup(conn);
            let kept = if tx { Vec::new() } else { applied };
            return Err(fail(Some(i), sql, e.to_string(), kept, rolled_back));
        }
        applied.push(sql.clone());
    }
    if tx && let Err(e) = exec(conn, "COMMIT") {
        let rolled_back = exec(conn, "ROLLBACK").is_ok();
        run_cleanup(conn);
        return Err(fail(None, "COMMIT", e.to_string(), Vec::new(), rolled_back));
    }
    run_cleanup(conn);
    Ok(script.statements.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), ..Default::default() }
    }

    fn users(schema: &str) -> TableDef {
        TableDef {
            schema: schema.into(),
            name: "users".into(),
            columns: vec![
                ColumnDef { nullable: false, auto_increment: true, ..col("id", "integer") },
                ColumnDef { nullable: false, comment: "the user's name".into(), ..col("name", "varchar(100)") },
                ColumnDef { default: "0".into(), ..col("score", "integer") },
            ],
            primary_key: vec!["id".into()],
            indexes: vec![IndexDef {
                name: "users_name".into(),
                columns: vec!["name".into()],
                unique: true,
                ..Default::default()
            }],
            checks: vec![CheckDef { name: "score_positive".into(), expression: "score >= 0".into() }],
            comment: "People".into(),
            ..Default::default()
        }
    }

    #[test]
    fn fragments_cannot_end_the_statement() {
        for ok in ["varchar(20)", "numeric(10, 2)", "'a;b'", "now()", "enum('a', 'b''s')", "integer[]", "'x\\'y'"] {
            assert!(check_fragment("t", ok).is_ok(), "{ok}");
        }
        for bad in ["int; drop table x", "int -- x", "int /* x */", "(1", "1)", "'open", "\"open", "a # b", "x\0"] {
            assert!(check_fragment("t", bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_table_per_dialect() {
        let pg = create_table(DbKind::Postgres, &users("public")).unwrap();
        assert!(pg.transactional);
        assert_eq!(
            pg.statements[0],
            "CREATE TABLE \"public\".\"users\" (\n    \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n    \
             \"name\" varchar(100) NOT NULL,\n    \"score\" integer DEFAULT 0,\n    PRIMARY KEY (\"id\"),\n    \
             CONSTRAINT \"score_positive\" CHECK (score >= 0)\n)"
        );
        assert_eq!(pg.statements[1], "CREATE UNIQUE INDEX \"users_name\" ON \"public\".\"users\" (\"name\")");
        assert_eq!(pg.statements[2], "COMMENT ON TABLE \"public\".\"users\" IS 'People'");
        assert_eq!(pg.statements[3], "COMMENT ON COLUMN \"public\".\"users\".\"name\" IS 'the user''s name'");

        let my = create_table(DbKind::Mysql, &users("app")).unwrap();
        assert!(!my.transactional);
        assert!(my.statements[0].contains("`id` integer NOT NULL AUTO_INCREMENT"), "{}", my.statements[0]);
        assert!(my.statements[0].contains("`name` varchar(100) NOT NULL COMMENT 'the user''s name'"));
        assert!(my.statements[0].ends_with(") COMMENT = 'People'"));
        assert_eq!(my.statements[1], "CREATE UNIQUE INDEX `users_name` ON `app`.`users` (`name`)");

        let lite = create_table(DbKind::Sqlite, &users("main")).unwrap();
        assert!(
            lite.statements[0].contains("\"id\" integer PRIMARY KEY AUTOINCREMENT NOT NULL"),
            "{}",
            lite.statements[0]
        );
        assert!(!lite.statements[0].contains("PRIMARY KEY (\"id\")"));
        assert_eq!(lite.statements[1], "CREATE UNIQUE INDEX \"main\".\"users_name\" ON \"users\" (\"name\")");
        assert!(lite.warnings.iter().any(|w| w.contains("no comments")));
        assert!(lite.to_sql().starts_with("-- SQLite has no comments"));
        assert!(lite.to_sql().contains("BEGIN;\nCREATE TABLE"));
    }

    #[test]
    fn hostile_names_are_quoted() {
        let t = TableDef {
            schema: "we\"ird".into(),
            name: "ta`ble\"; drop".into(),
            columns: vec![col("sel\"ect", "text"), col("ünï code", "text"), col("order", "int")],
            foreign_keys: vec![ForeignKeyDef {
                name: "f\"k".into(),
                columns: vec!["order".into()],
                ref_table: "o`ther".into(),
                ref_columns: vec!["i\"d".into()],
                on_delete: FkAction::Cascade,
                ..Default::default()
            }],
            comment: "it's \\ here".into(),
            ..Default::default()
        };
        let pg = create_table(DbKind::Postgres, &t).unwrap().to_sql();
        assert!(pg.contains("CREATE TABLE \"we\"\"ird\".\"ta`ble\"\"; drop\""), "{pg}");
        assert!(pg.contains("\"sel\"\"ect\" text"));
        assert!(pg.contains("\"ünï code\" text"));
        assert!(pg.contains("\"order\" int"));
        assert!(pg.contains(
            "CONSTRAINT \"f\"\"k\" FOREIGN KEY (\"order\") REFERENCES \"we\"\"ird\".\"o`ther\" (\"i\"\"d\") ON DELETE CASCADE"
        ));
        assert!(pg.contains("IS E'it''s \\\\ here'"), "{pg}");
        let my = create_table(DbKind::Mysql, &t).unwrap().to_sql();
        assert!(my.contains("CREATE TABLE `we\"ird`.`ta``ble\"; drop`"), "{my}");
        assert!(my.contains("REFERENCES `we\"ird`.`o``ther` (`i\"d`)"));
        assert!(my.contains("COMMENT = CONVERT(X'"), "{my}");
        let lite = create_table(DbKind::Sqlite, &t).unwrap().to_sql();
        assert!(lite.contains("REFERENCES \"o`ther\" (\"i\"\"d\")"), "{lite}");
        // Nothing user-typed can smuggle a statement.
        let mut evil = t.clone();
        evil.columns[0].default = "1); drop table x; --".into();
        assert!(create_table(DbKind::Postgres, &evil).is_err());
        let mut evil = t.clone();
        evil.columns[0].data_type = "text, x int); drop table y; (".into();
        assert!(create_table(DbKind::Sqlite, &evil).is_err());
        let mut evil = t;
        evil.checks.push(CheckDef { name: String::new(), expression: "1=1) ; drop table z; (".into() });
        assert!(create_table(DbKind::Mysql, &evil).is_err());
    }

    #[test]
    fn validation_errors() {
        let mut t = users("s");
        t.columns.push(col("id", "int"));
        assert!(validate(DbKind::Postgres, &t).unwrap_err().to_string().contains("duplicate column"));
        let mut t = users("s");
        t.primary_key = vec!["nope".into()];
        assert!(validate(DbKind::Postgres, &t).is_err());
        let mut t = users("s");
        t.columns[1].auto_increment = true;
        assert!(validate(DbKind::Sqlite, &t).is_err());
        let mut t = users("s");
        t.columns[2].data_type = " ".into();
        assert!(validate(DbKind::Mysql, &t).is_err());
        let t = TableDef { name: "x".into(), ..Default::default() };
        assert!(validate(DbKind::Mysql, &t).is_err());
        let mut t = users("s");
        t.foreign_keys.push(ForeignKeyDef {
            columns: vec!["id".into()],
            ref_table: "p".into(),
            ref_columns: vec![],
            ..Default::default()
        });
        assert!(validate(DbKind::Postgres, &t).is_err());
    }

    fn modified(old: &TableDef) -> TableDef {
        let mut new = old.clone().for_modify();
        new.columns[1].name = "full_name".into();
        new.columns[2].data_type = "bigint".into();
        new.columns[2].default = String::new();
        new.columns[2].nullable = false;
        new.columns.push(ColumnDef { comment: "added".into(), ..col("email", "text") });
        new.indexes.push(IndexDef { columns: vec!["email".into()], ..Default::default() });
        new
    }

    #[test]
    fn alter_postgres() {
        let old = users("public");
        let s = alter_table(DbKind::Postgres, &old, &modified(&old)).unwrap();
        let sql = s.to_sql();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"public\".\"users\" RENAME COLUMN \"name\" TO \"full_name\"",
                "ALTER TABLE \"public\".\"users\" ALTER COLUMN \"score\" TYPE bigint USING \"score\"::bigint",
                "ALTER TABLE \"public\".\"users\" ALTER COLUMN \"score\" DROP DEFAULT",
                "ALTER TABLE \"public\".\"users\" ALTER COLUMN \"score\" SET NOT NULL",
                "ALTER TABLE \"public\".\"users\" ADD COLUMN \"email\" text",
                "COMMENT ON COLUMN \"public\".\"users\".\"email\" IS 'added'",
                "CREATE INDEX \"users_email_index\" ON \"public\".\"users\" (\"email\")",
            ],
            "{sql}"
        );
        assert!(sql.starts_with("BEGIN;\n") && sql.ends_with("COMMIT;\n"));
        // The renamed column's index follows it without being recreated.
        let mut new = modified(&old);
        new.indexes[0].columns = vec!["name".into()];
        let s2 = alter_table(DbKind::Postgres, &old, &new).unwrap();
        assert!(!s2.statements.iter().any(|s| s.contains("users_name")), "{:?}", s2.statements);
        // Unchanged: nothing to do.
        assert!(alter_table(DbKind::Postgres, &old, &old.clone().for_modify()).unwrap().is_empty());
        // Table rename, dropped column, primary key and identity changes.
        let mut new = old.clone().for_modify();
        new.name = "people".into();
        new.columns.remove(2);
        new.columns[0].auto_increment = false;
        new.primary_key = vec!["id".into(), "name".into()];
        new.checks.clear();
        let s = alter_table(DbKind::Postgres, &old, &new).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"public\".\"users\" DROP CONSTRAINT \"score_positive\"",
                "ALTER TABLE \"public\".\"users\" DROP CONSTRAINT \"users_pkey\"",
                "ALTER TABLE \"public\".\"users\" RENAME TO \"people\"",
                "ALTER TABLE \"public\".\"people\" DROP COLUMN \"score\"",
                "ALTER TABLE \"public\".\"people\" ALTER COLUMN \"id\" DROP IDENTITY IF EXISTS",
                "ALTER TABLE \"public\".\"people\" ADD PRIMARY KEY (\"id\", \"name\")",
                "COMMENT ON TABLE \"public\".\"people\" IS 'People'",
            ]
            .into_iter()
            .filter(|s| !s.starts_with("COMMENT"))
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn alter_mysql_statement_per_change() {
        let old = users("app");
        let s = alter_table(DbKind::Mysql, &old, &modified(&old)).unwrap();
        assert!(!s.transactional);
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `app`.`users` CHANGE COLUMN `name` `full_name` varchar(100) NOT NULL COMMENT 'the user''s name'",
                "ALTER TABLE `app`.`users` CHANGE COLUMN `score` `score` bigint NOT NULL",
                "ALTER TABLE `app`.`users` ADD COLUMN `email` text NULL COMMENT 'added' AFTER `score`",
                "CREATE INDEX `users_email_index` ON `app`.`users` (`email`)",
            ]
        );
        let mut new = old.clone().for_modify();
        new.foreign_keys.push(ForeignKeyDef {
            name: "fk".into(),
            columns: vec!["score".into()],
            ref_table: "scores".into(),
            ref_columns: vec!["id".into()],
            on_update: FkAction::SetNull,
            ..Default::default()
        });
        new.checks.clear();
        let s = alter_table(DbKind::Mariadb, &old, &new).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `app`.`users` DROP CONSTRAINT `score_positive`",
                "ALTER TABLE `app`.`users` ADD CONSTRAINT `fk` FOREIGN KEY (`score`) REFERENCES `app`.`scores` (`id`) ON UPDATE SET NULL",
            ]
        );
    }

    #[test]
    fn alter_sqlite_in_place_or_rebuild() {
        let mut old = users("main");
        old.comment.clear();
        old.columns[1].comment.clear();
        // Renames and a plain added column alter in place.
        let mut new = old.clone().for_modify();
        new.name = "people".into();
        new.columns[2].name = "points".into();
        new.columns.push(col("email", "text"));
        let s = alter_table(DbKind::Sqlite, &old, &new).unwrap();
        assert!(s.setup.is_empty());
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"main\".\"users\" RENAME TO \"people\"",
                "ALTER TABLE \"main\".\"people\" RENAME COLUMN \"score\" TO \"points\"",
                "ALTER TABLE \"main\".\"people\" ADD COLUMN \"email\" text",
            ]
        );
        // A type change rebuilds.
        let mut new = old.clone().for_modify();
        new.columns[2].data_type = "real".into();
        new.columns[2].name = "points".into();
        let s = alter_table(DbKind::Sqlite, &old, &new).unwrap();
        assert_eq!(s.setup, vec!["PRAGMA foreign_keys = OFF", "PRAGMA legacy_alter_table = OFF"]);
        assert_eq!(s.cleanup, vec!["PRAGMA foreign_keys = ON", "PRAGMA legacy_alter_table = OFF"]);
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"main\".\"users\" RENAME COLUMN \"score\" TO \"points\"".to_string(),
                "CREATE TABLE \"main\".\"codecraft_new_users\" (\n    \"id\" integer PRIMARY KEY AUTOINCREMENT NOT NULL,\n    \
                 \"name\" varchar(100) NOT NULL,\n    \"points\" real DEFAULT 0,\n    CONSTRAINT \"score_positive\" CHECK (points >= 0)\n)"
                    .to_string(),
                "INSERT INTO \"main\".\"codecraft_new_users\" (\"id\", \"name\", \"points\") SELECT \"id\", \"name\", \"points\" FROM \"main\".\"users\"".to_string(),
                "DROP TABLE \"main\".\"users\"".to_string(),
                "PRAGMA legacy_alter_table = ON".to_string(),
                "ALTER TABLE \"main\".\"codecraft_new_users\" RENAME TO \"users\"".to_string(),
                "PRAGMA legacy_alter_table = OFF".to_string(),
                "CREATE UNIQUE INDEX \"main\".\"users_name\" ON \"users\" (\"name\")".to_string(),
                "PRAGMA \"main\".foreign_key_check(\"users\")".to_string(),
            ]
        );
        assert!(s.warnings[0].contains("rebuilt"));
        let sql = s.to_sql();
        assert!(sql.find("PRAGMA foreign_keys = OFF").unwrap() < sql.find("BEGIN").unwrap());
        assert!(sql.find("COMMIT").unwrap() < sql.find("PRAGMA foreign_keys = ON").unwrap());
    }

    #[test]
    fn drop_rename_truncate() {
        let t = |kind, name: &str, table: &str| ObjectRef {
            kind,
            schema: "s".into(),
            name: name.into(),
            table: table.into(),
        };
        let pg = |o: &ObjectRef| drop_object(DbKind::Postgres, o, false).unwrap().statements[0].clone();
        assert_eq!(pg(&t(ObjectKind::Table, "a\"b", "")), "DROP TABLE \"s\".\"a\"\"b\"");
        assert_eq!(
            drop_object(DbKind::Postgres, &t(ObjectKind::Schema, "s", ""), true).unwrap().statements[0],
            "DROP SCHEMA \"s\" CASCADE"
        );
        assert_eq!(pg(&t(ObjectKind::Index, "i", "t")), "DROP INDEX \"s\".\"i\"");
        assert_eq!(pg(&t(ObjectKind::Column, "c", "t")), "ALTER TABLE \"s\".\"t\" DROP COLUMN \"c\"");
        assert_eq!(
            drop_object(DbKind::Mysql, &t(ObjectKind::Index, "i", "t"), false).unwrap().statements[0],
            "DROP INDEX `i` ON `s`.`t`"
        );
        assert_eq!(
            drop_object(DbKind::Mysql, &t(ObjectKind::Schema, "s", ""), false).unwrap().statements[0],
            "DROP DATABASE `s`"
        );
        assert!(drop_object(DbKind::Sqlite, &t(ObjectKind::Schema, "main", ""), false).is_err());
        assert!(drop_object(DbKind::Sqlite, &t(ObjectKind::ForeignKey, "fk", "t"), false).is_err());
        assert!(drop_object(DbKind::Postgres, &t(ObjectKind::Column, "c", ""), false).is_err());
        assert!(drop_object(DbKind::Postgres, &t(ObjectKind::Table, "", ""), false).is_err());

        let r = |kind, o: &ObjectRef, n: &str| rename_object(kind, o, n).map(|s| s.statements[0].clone());
        assert_eq!(
            r(DbKind::Postgres, &t(ObjectKind::Table, "a", ""), "b").unwrap(),
            "ALTER TABLE \"s\".\"a\" RENAME TO \"b\""
        );
        assert_eq!(
            r(DbKind::Mysql, &t(ObjectKind::View, "a", ""), "b`c").unwrap(),
            "RENAME TABLE `s`.`a` TO `s`.`b``c`"
        );
        assert_eq!(
            r(DbKind::Sqlite, &t(ObjectKind::Column, "a", "t"), "b").unwrap(),
            "ALTER TABLE \"s\".\"t\" RENAME COLUMN \"a\" TO \"b\""
        );
        assert_eq!(
            r(DbKind::Mysql, &t(ObjectKind::Index, "a", "t"), "b").unwrap(),
            "ALTER TABLE `s`.`t` RENAME INDEX `a` TO `b`"
        );
        assert_eq!(
            r(DbKind::Postgres, &t(ObjectKind::Schema, "s", ""), "n").unwrap(),
            "ALTER SCHEMA \"s\" RENAME TO \"n\""
        );
        assert!(r(DbKind::Sqlite, &t(ObjectKind::Index, "a", "t"), "b").is_err());
        assert!(r(DbKind::Mysql, &t(ObjectKind::Schema, "a", ""), "b").is_err());
        assert!(r(DbKind::Postgres, &t(ObjectKind::Table, "a", ""), " ").is_err());

        assert_eq!(
            truncate_table(DbKind::Sqlite, "main", "t", false).unwrap().statements[0],
            "DELETE FROM \"main\".\"t\""
        );
        assert_eq!(
            truncate_table(DbKind::Postgres, "s", "t", true).unwrap().statements[0],
            "TRUNCATE TABLE \"s\".\"t\" CASCADE"
        );
        assert!(!truncate_table(DbKind::Mysql, "s", "t", false).unwrap().warnings.is_empty());
    }

    #[test]
    fn identifiers_are_renamed_outside_strings() {
        let map: BTreeMap<String, String> =
            [("title".to_string(), "book title".to_string()), ("price".to_string(), "cost".to_string())].into();
        let sql = "CREATE TRIGGER t AFTER UPDATE ON b BEGIN UPDATE log SET v = 'title' || NEW.title, \"price\" = Price; -- title\n END";
        assert_eq!(
            rename_identifiers(DbKind::Sqlite, sql, &map),
            "CREATE TRIGGER t AFTER UPDATE ON b BEGIN UPDATE log SET v = 'title' || NEW.\"book title\", \"cost\" = cost; -- title\n END"
        );
        assert_eq!(rename_identifiers(DbKind::Mysql, "`title` + titles", &map), "`book title` + titles");
        assert_eq!(rename_identifiers(DbKind::Postgres, "x", &BTreeMap::new()), "x");
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(FkAction::parse("SET NULL"), Some(FkAction::SetNull));
        assert_eq!(FkAction::parse("c"), Some(FkAction::Cascade));
        assert_eq!(FkAction::parse("explode"), None);
        assert_eq!(ObjectKind::parse("materialized view"), Some(ObjectKind::MaterializedView));
        assert_eq!(ObjectKind::parse("FK"), Some(ObjectKind::ForeignKey));
        assert!(!type_suggestions(DbKind::Sqlite).is_empty());
        assert_eq!(sqlite_default("datetime('now')"), "(datetime('now'))");
        assert_eq!(sqlite_default("'x'"), "'x'");
        assert_eq!(sqlite_default("-1.5"), "-1.5");
        assert_eq!(generated_name(&"t".repeat(80), &[], "fk").len(), 60);
    }
}
