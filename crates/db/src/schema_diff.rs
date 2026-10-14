//! Compare two schemas (pure): a diff tree of tables, columns, keys, indexes, foreign keys, checks
//! and views that were added, removed or changed, and the migration script that makes the target
//! schema like the source (DataGrip's Compare).
//!
//! Both sides may be different dialects: types are compared by family (`int4` = `integer` =
//! `INT(11)`), PostgreSQL casts in defaults are ignored, and indexes and foreign keys whose names
//! differ but whose definitions match are the same. The migration is written in the target's
//! dialect, built from the generators of [`crate::ddl`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ddl::{
    ColumnDef, DdlScript, ForeignKeyDef, IndexDef, ObjectKind, ObjectRef, TableDef, alter_table, create_table,
    drop_object, transactional_ddl,
};
use crate::sql::qualified;
use crate::{DbKind, DbResult};

/// A view (or materialized view) and its query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ViewDef {
    pub name: String,
    pub materialized: bool,
    /// The SELECT, as the database reports it.
    pub query: String,
}

/// Everything compared and drawn of one schema.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SchemaModel {
    pub kind: DbKind,
    pub schema: String,
    pub tables: Vec<TableDef>,
    pub views: Vec<ViewDef>,
}

impl SchemaModel {
    pub fn table(&self, name: &str) -> Option<&TableDef> {
        self.tables.iter().find(|t| t.name == name)
    }
}

/// How an object differs. `Added`: only in the source (the migration creates it in the target).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Change {
    Added,
    Removed,
    Changed,
}

impl Change {
    pub fn symbol(self) -> &'static str {
        match self {
            Change::Added => "+",
            Change::Removed => "−",
            Change::Changed => "~",
        }
    }
}

/// One node of the diff tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffItem {
    /// `table`, `view`, `column`, `primary key`, `index`, `foreign key`, `check`, `comment`.
    pub object: String,
    pub name: String,
    pub change: Change,
    /// What changed (`integer → bigint`, `nullable → not null`).
    pub detail: String,
    pub children: Vec<DiffItem>,
}

impl DiffItem {
    fn new(object: &str, name: &str, change: Change, detail: impl Into<String>) -> Self {
        Self { object: object.into(), name: name.into(), change, detail: detail.into(), children: Vec::new() }
    }

    /// Nodes in this subtree.
    pub fn count(&self) -> usize {
        1 + self.children.iter().map(DiffItem::count).sum::<usize>()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaDiff {
    pub items: Vec<DiffItem>,
    /// Caveats of the comparison (dialects differ, view definitions not compared).
    pub notes: Vec<String>,
}

impl SchemaDiff {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Top-level objects (tables and views) added, removed and changed.
    pub fn counts(&self) -> (usize, usize, usize) {
        let n = |c: Change| self.items.iter().filter(|i| i.change == c).count();
        (n(Change::Added), n(Change::Removed), n(Change::Changed))
    }
}

// ---------------------------------------------------------------- normalization

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A type reduced to its family for comparing across dialects (`int4`, `INT(11)` → `integer`).
pub fn type_family(kind: DbKind, ty: &str) -> String {
    let t = squash(ty).to_ascii_lowercase().replace(" (", "(").replace(", ", ",");
    let (base, args) = match t.find('(') {
        Some(i) => (t.get(..i).unwrap_or_default().trim().to_string(), t.get(i..).unwrap_or_default().to_string()),
        None => (t.clone(), String::new()),
    };
    let base = base.trim_end_matches(" unsigned").trim().to_string();
    let family = match base.as_str() {
        "tinyint" if args == "(1)" => "boolean",
        "bool" | "boolean" => "boolean",
        "smallint" | "int2" | "smallserial" | "tinyint" => "smallint",
        "int" | "integer" | "int4" | "mediumint" | "serial" | "serial4" => "integer",
        "bigint" | "int8" | "bigserial" | "serial8" => "bigint",
        "real" | "float4" => "real",
        "float" if kind != DbKind::Postgres => "real",
        "double" | "double precision" | "float8" | "float" => "double",
        "numeric" | "decimal" => return format!("numeric{args}"),
        "varchar" | "character varying" | "nvarchar" => return format!("varchar{args}"),
        "char" | "character" | "bpchar" | "nchar" => return format!("char{args}"),
        "text" | "tinytext" | "mediumtext" | "longtext" | "clob" => "text",
        "blob" | "bytea" | "tinyblob" | "mediumblob" | "longblob" | "binary" | "varbinary" => "blob",
        "datetime" | "timestamp" | "timestamp without time zone" => "timestamp",
        "timestamptz" | "timestamp with time zone" => "timestamptz",
        "time" | "time without time zone" => "time",
        "json" | "jsonb" => "json",
        _ => return t,
    };
    family.to_string()
}

/// The type to use in `to` for a type of `from`.
pub fn convert_type(from: DbKind, to: DbKind, ty: &str) -> String {
    let same = |a: DbKind, b: DbKind| {
        a == b || matches!((a, b), (DbKind::Mysql | DbKind::Mariadb, DbKind::Mysql | DbKind::Mariadb))
    };
    if same(from, to) {
        return ty.trim().to_string();
    }
    let fam = type_family(from, ty);
    let (base, args) = match fam.find('(') {
        Some(i) => (fam.get(..i).unwrap_or_default().to_string(), fam.get(i..).unwrap_or_default().to_string()),
        None => (fam.clone(), String::new()),
    };
    let my = matches!(to, DbKind::Mysql | DbKind::Mariadb);
    let pg = to == DbKind::Postgres;
    match base.as_str() {
        "boolean" if my => "tinyint(1)".to_string(),
        "boolean" if pg => "boolean".into(),
        "boolean" => "BOOLEAN".into(),
        "smallint" if to == DbKind::Sqlite => "INTEGER".into(),
        "integer" if to == DbKind::Sqlite => "INTEGER".into(),
        "bigint" if to == DbKind::Sqlite => "INTEGER".into(),
        "smallint" | "bigint" => base.clone(),
        "integer" if my => "int".into(),
        "integer" => "integer".into(),
        "real" if to == DbKind::Sqlite => "REAL".into(),
        "real" if my => "float".into(),
        "real" => "real".into(),
        "double" if to == DbKind::Sqlite => "REAL".into(),
        "double" if my => "double".into(),
        "double" => "double precision".into(),
        "numeric" if my => format!("decimal{args}"),
        "numeric" if to == DbKind::Sqlite => format!("NUMERIC{args}"),
        "numeric" => format!("numeric{args}"),
        "varchar" if args.is_empty() && my => "varchar(255)".into(),
        "varchar" if to == DbKind::Sqlite => format!("VARCHAR{args}"),
        "varchar" => format!("varchar{args}"),
        "char" if to == DbKind::Sqlite => format!("CHAR{args}"),
        "char" => format!("char{args}"),
        "text" if to == DbKind::Sqlite => "TEXT".into(),
        "text" if my => "longtext".into(),
        "text" => "text".into(),
        "blob" if pg => "bytea".into(),
        "blob" if my => "longblob".into(),
        "blob" => "BLOB".into(),
        "timestamp" if my => "datetime".into(),
        "timestamp" if to == DbKind::Sqlite => "DATETIME".into(),
        "timestamp" => "timestamp".into(),
        "timestamptz" if my => "timestamp".into(),
        "timestamptz" if to == DbKind::Sqlite => "DATETIME".into(),
        "timestamptz" => "timestamp with time zone".into(),
        "json" if to == DbKind::Sqlite => "TEXT".into(),
        "json" => "json".into(),
        "uuid" if my => "char(36)".into(),
        "uuid" if to == DbKind::Sqlite => "TEXT".into(),
        _ => ty.trim().to_string(),
    }
}

/// A default without PostgreSQL casts (`'a'::character varying` → `'a'`), whitespace squashed.
fn norm_default(d: &str) -> String {
    let mut out = String::new();
    let mut quote = false;
    let chars: Vec<char> = d.trim().chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        if c == '\'' {
            quote = !quote;
            out.push(c);
            i += 1;
        } else if !quote && c == ':' && chars.get(i + 1) == Some(&':') {
            // Skip the cast's type name (letters, spaces of multi-word types, brackets, digits).
            i += 2;
            while chars.get(i).is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | ' ' | '[' | ']' | '"' | '.')) {
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    let s = squash(&out);
    let s = s.trim().to_string();
    // `(0)` and `0` are the same default.
    if s.starts_with('(') && s.ends_with(')') && s.matches('(').count() == 1 {
        return s.get(1..s.len().saturating_sub(1)).unwrap_or_default().trim().to_string();
    }
    s
}

fn is_sequence_default(d: &str) -> bool {
    d.trim_start().to_ascii_lowercase().starts_with("nextval(")
}

/// A column with dialect-specific auto-increment spellings made uniform.
fn norm_column(kind: DbKind, c: &ColumnDef) -> (String, String, bool, bool) {
    let mut auto = c.auto_increment;
    let mut default = c.default.clone();
    if is_sequence_default(&default) {
        auto = true;
        default.clear();
    }
    let ty = type_family(kind, &c.data_type);
    if matches!(c.data_type.trim().to_ascii_lowercase().as_str(), "serial" | "bigserial" | "smallserial") {
        auto = true;
    }
    (ty, norm_default(&default), c.nullable, auto)
}

fn norm_check(e: &str) -> String {
    let mut s: String = e.chars().filter(|c| !c.is_whitespace() && !matches!(c, '(' | ')' | '"' | '`')).collect();
    s.make_ascii_lowercase();
    norm_default(&s)
}

/// Indexes that only back a MySQL foreign key (MySQL creates them by itself).
fn real_indexes(kind: DbKind, t: &TableDef) -> Vec<&IndexDef> {
    t.indexes
        .iter()
        .filter(|i| {
            !(matches!(kind, DbKind::Mysql | DbKind::Mariadb)
                && t.foreign_keys.iter().any(|f| f.name == i.name && f.columns == i.columns))
        })
        .collect()
}

fn index_sig(i: &IndexDef) -> (Vec<String>, bool) {
    (i.columns.iter().map(|c| c.to_ascii_lowercase()).collect(), i.unique)
}

fn fk_sig(f: &ForeignKeyDef) -> (Vec<String>, String, Vec<String>) {
    (
        f.columns.iter().map(|c| c.to_ascii_lowercase()).collect(),
        f.ref_table.to_ascii_lowercase(),
        f.ref_columns.iter().map(|c| c.to_ascii_lowercase()).collect(),
    )
}

/// Pair items of two lists: first by equal name, then by equal signature. Returns
/// `(source index, target index)` pairs plus the unmatched ones of each side.
fn pair<T, K: PartialEq>(
    src: &[T],
    dst: &[T],
    name: impl Fn(&T) -> &str,
    sig: impl Fn(&T) -> K,
) -> (Vec<(usize, usize)>, Vec<usize>, Vec<usize>) {
    let mut pairs = Vec::new();
    let mut used_d = BTreeSet::new();
    let mut used_s = BTreeSet::new();
    for (i, a) in src.iter().enumerate() {
        if let Some(j) =
            dst.iter().enumerate().position(|(j, b)| !used_d.contains(&j) && !name(a).is_empty() && name(a) == name(b))
        {
            pairs.push((i, j));
            used_d.insert(j);
            used_s.insert(i);
        }
    }
    for (i, a) in src.iter().enumerate().filter(|(i, _)| !used_s.contains(i)).collect::<Vec<_>>() {
        if let Some(j) = dst.iter().enumerate().position(|(j, b)| !used_d.contains(&j) && sig(a) == sig(b)) {
            pairs.push((i, j));
            used_d.insert(j);
            used_s.insert(i);
        }
    }
    let only_s = (0..src.len()).filter(|i| !used_s.contains(i)).collect();
    let only_d = (0..dst.len()).filter(|j| !used_d.contains(j)).collect();
    (pairs, only_s, only_d)
}

/// Match tables by name (case-insensitively when nothing matches exactly).
fn pair_by_name<'a, T>(
    src: &'a [T],
    dst: &'a [T],
    name: impl Fn(&T) -> &str,
) -> (Vec<(usize, usize)>, Vec<usize>, Vec<usize>) {
    pair(src, dst, &name, |t| name(t).to_lowercase())
}

// ---------------------------------------------------------------- diff

fn column_detail(sk: DbKind, a: &ColumnDef, tk: DbKind, b: &ColumnDef, cross: bool) -> Vec<String> {
    let (ta, da, na, aa) = norm_column(sk, a);
    let (tb, db, nb, ab) = norm_column(tk, b);
    let mut d = Vec::new();
    let types_differ = if cross {
        ta != tb
    } else {
        squash(&a.data_type).to_lowercase() != squash(&b.data_type).to_lowercase() && ta != tb
    };
    if types_differ {
        d.push(format!("type {} → {}", b.data_type.trim(), a.data_type.trim()));
    }
    if na != nb {
        d.push(if na { "not null → nullable".into() } else { "nullable → not null".into() });
    }
    if da != db {
        let show = |s: &str| if s.is_empty() { "none".to_string() } else { s.to_string() };
        d.push(format!("default {} → {}", show(&db), show(&da)));
    }
    if aa != ab {
        d.push(if aa { "auto-increment added".into() } else { "auto-increment removed".into() });
    }
    let comments = sk != DbKind::Sqlite && tk != DbKind::Sqlite;
    if comments && a.comment != b.comment {
        d.push("comment".into());
    }
    d
}

fn diff_table(sk: DbKind, s: &TableDef, tk: DbKind, t: &TableDef) -> Vec<DiffItem> {
    let cross = sk != tk;
    let mut out = Vec::new();
    let (pairs, only_s, only_t) = pair_by_name(&s.columns, &t.columns, |c| c.name.as_str());
    for i in only_s {
        if let Some(c) = s.columns.get(i) {
            out.push(DiffItem::new("column", &c.name, Change::Added, c.data_type.clone()));
        }
    }
    for j in only_t {
        if let Some(c) = t.columns.get(j) {
            out.push(DiffItem::new("column", &c.name, Change::Removed, c.data_type.clone()));
        }
    }
    for (i, j) in pairs {
        let (Some(a), Some(b)) = (s.columns.get(i), t.columns.get(j)) else { continue };
        let d = column_detail(sk, a, tk, b, cross);
        if !d.is_empty() {
            out.push(DiffItem::new("column", &a.name, Change::Changed, d.join("; ")));
        }
    }
    let lower = |v: &[String]| v.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>();
    if lower(&s.primary_key) != lower(&t.primary_key) {
        let show = |v: &[String]| if v.is_empty() { "none".to_string() } else { format!("({})", v.join(", ")) };
        let change = match (s.primary_key.is_empty(), t.primary_key.is_empty()) {
            (false, true) => Change::Added,
            (true, false) => Change::Removed,
            _ => Change::Changed,
        };
        out.push(DiffItem::new(
            "primary key",
            "",
            change,
            format!("{} → {}", show(&t.primary_key), show(&s.primary_key)),
        ));
    }
    let si = real_indexes(sk, s);
    let ti = real_indexes(tk, t);
    let (pairs, only_s, only_t) = pair(&si, &ti, |i| i.name.as_str(), |i| index_sig(i));
    for i in only_s {
        if let Some(x) = si.get(i) {
            out.push(DiffItem::new("index", &x.name, Change::Added, format!("({})", x.columns.join(", "))));
        }
    }
    for j in only_t {
        if let Some(x) = ti.get(j) {
            out.push(DiffItem::new("index", &x.name, Change::Removed, format!("({})", x.columns.join(", "))));
        }
    }
    for (i, j) in pairs {
        let (Some(a), Some(b)) = (si.get(i), ti.get(j)) else { continue };
        if index_sig(a) != index_sig(b) {
            let u = |x: &IndexDef| if x.unique { " unique" } else { "" };
            out.push(DiffItem::new(
                "index",
                &a.name,
                Change::Changed,
                format!("({}){} → ({}){}", b.columns.join(", "), u(b), a.columns.join(", "), u(a)),
            ));
        }
    }
    let (pairs, only_s, only_t) = pair(&s.foreign_keys, &t.foreign_keys, |f| f.name.as_str(), fk_sig);
    let fk_text =
        |f: &ForeignKeyDef| format!("({}) → {}({})", f.columns.join(", "), f.ref_table, f.ref_columns.join(", "));
    for i in only_s {
        if let Some(f) = s.foreign_keys.get(i) {
            out.push(DiffItem::new("foreign key", &f.name, Change::Added, fk_text(f)));
        }
    }
    for j in only_t {
        if let Some(f) = t.foreign_keys.get(j) {
            out.push(DiffItem::new("foreign key", &f.name, Change::Removed, fk_text(f)));
        }
    }
    for (i, j) in pairs {
        let (Some(a), Some(b)) = (s.foreign_keys.get(i), t.foreign_keys.get(j)) else { continue };
        let mut d = Vec::new();
        if fk_sig(a) != fk_sig(b) {
            d.push(format!("{} → {}", fk_text(b), fk_text(a)));
        }
        if a.on_delete != b.on_delete {
            d.push(format!("ON DELETE {} → {}", b.on_delete.sql(), a.on_delete.sql()));
        }
        if a.on_update != b.on_update {
            d.push(format!("ON UPDATE {} → {}", b.on_update.sql(), a.on_update.sql()));
        }
        if !d.is_empty() {
            out.push(DiffItem::new("foreign key", &a.name, Change::Changed, d.join("; ")));
        }
    }
    let (_, only_s, only_t) = pair(&s.checks, &t.checks, |_| "", |c| norm_check(&c.expression));
    for i in only_s {
        if let Some(c) = s.checks.get(i) {
            out.push(DiffItem::new("check", &c.name, Change::Added, c.expression.clone()));
        }
    }
    for j in only_t {
        if let Some(c) = t.checks.get(j) {
            out.push(DiffItem::new("check", &c.name, Change::Removed, c.expression.clone()));
        }
    }
    if sk != DbKind::Sqlite && tk != DbKind::Sqlite && s.comment != t.comment {
        out.push(DiffItem::new("comment", "", Change::Changed, format!("{:?} → {:?}", t.comment, s.comment)));
    }
    out
}

/// What differs between `source` and `target` (`Added`: only in the source).
pub fn diff(source: &SchemaModel, target: &SchemaModel) -> SchemaDiff {
    let mut d = SchemaDiff::default();
    let cross = source.kind != target.kind;
    if cross {
        d.notes.push(format!(
            "Comparing {} with {}: types are compared by family; view definitions are not compared",
            source.kind.label(),
            target.kind.label()
        ));
    }
    let (pairs, only_s, only_t) = pair_by_name(&source.tables, &target.tables, |t| t.name.as_str());
    for i in only_s {
        if let Some(t) = source.tables.get(i) {
            d.items.push(DiffItem::new("table", &t.name, Change::Added, format!("{} columns", t.columns.len())));
        }
    }
    for j in only_t {
        if let Some(t) = target.tables.get(j) {
            d.items.push(DiffItem::new("table", &t.name, Change::Removed, format!("{} columns", t.columns.len())));
        }
    }
    for (i, j) in pairs {
        let (Some(a), Some(b)) = (source.tables.get(i), target.tables.get(j)) else { continue };
        let children = diff_table(source.kind, a, target.kind, b);
        if !children.is_empty() {
            let mut item = DiffItem::new("table", &a.name, Change::Changed, format!("{} change(s)", children.len()));
            item.children = children;
            d.items.push(item);
        }
    }
    let (pairs, only_s, only_t) = pair_by_name(&source.views, &target.views, |v| v.name.as_str());
    for i in only_s {
        if let Some(v) = source.views.get(i) {
            d.items.push(DiffItem::new("view", &v.name, Change::Added, String::new()));
        }
    }
    for j in only_t {
        if let Some(v) = target.views.get(j) {
            d.items.push(DiffItem::new("view", &v.name, Change::Removed, String::new()));
        }
    }
    if !cross {
        for (i, j) in pairs {
            let (Some(a), Some(b)) = (source.views.get(i), target.views.get(j)) else { continue };
            if squash(&a.query).to_lowercase() != squash(&b.query).to_lowercase() || a.materialized != b.materialized {
                d.items.push(DiffItem::new("view", &a.name, Change::Changed, "definition".to_string()));
            }
        }
    }
    d.items.sort_by(|a, b| {
        let rank = |i: &DiffItem| (i.object != "table", i.change as u8);
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
    });
    d
}

// ---------------------------------------------------------------- migration

/// Table indexes in dependency order: tables referenced by foreign keys come before the tables
/// that reference them (cycles keep their original order).
pub fn dependency_order(tables: &[TableDef]) -> Vec<usize> {
    let index: BTreeMap<&str, usize> = tables.iter().enumerate().map(|(i, t)| (t.name.as_str(), i)).collect();
    let deps: Vec<BTreeSet<usize>> = tables
        .iter()
        .enumerate()
        .map(|(i, t)| {
            t.foreign_keys.iter().filter_map(|f| index.get(f.ref_table.as_str()).copied()).filter(|j| *j != i).collect()
        })
        .collect();
    let mut done = vec![false; tables.len()];
    let mut out = Vec::with_capacity(tables.len());
    loop {
        let mut progressed = false;
        for i in 0..tables.len() {
            if done.get(i).copied().unwrap_or(true) {
                continue;
            }
            if deps.get(i).is_some_and(|d| d.iter().all(|j| done.get(*j).copied().unwrap_or(true))) {
                if let Some(slot) = done.get_mut(i) {
                    *slot = true;
                }
                out.push(i);
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    // Cycles: the rest in their original order.
    for (i, d) in done.iter().enumerate() {
        if !d {
            out.push(i);
        }
    }
    out
}

/// A source table rewritten for the target: target schema, target dialect types and defaults.
fn adapt(t: &TableDef, from: &SchemaModel, to: &SchemaModel) -> TableDef {
    let mut out = t.clone();
    out.schema = to.schema.clone();
    let cross = from.kind != to.kind;
    for c in &mut out.columns {
        c.original.clear();
        if cross {
            if is_sequence_default(&c.default) {
                c.default.clear();
                c.auto_increment = true;
            }
            if matches!(c.data_type.trim().to_ascii_lowercase().as_str(), "serial" | "bigserial" | "smallserial") {
                c.auto_increment = true;
            }
            c.data_type = convert_type(from.kind, to.kind, &c.data_type);
            c.default = norm_default(&c.default);
            if to.kind == DbKind::Sqlite && c.auto_increment {
                // Only an INTEGER PRIMARY KEY can auto-increment in SQLite.
                c.auto_increment = t.primary_key.as_slice() == std::slice::from_ref(&c.name);
            }
        }
    }
    for f in &mut out.foreign_keys {
        if f.ref_schema.is_empty() || f.ref_schema == from.schema {
            f.ref_schema.clear();
        }
    }
    if cross {
        out.indexes.retain(|i| !i.columns.is_empty());
        for i in &mut out.indexes {
            i.definition.clear();
        }
        out.triggers.clear();
        if matches!(from.kind, DbKind::Mysql | DbKind::Mariadb) {
            let fks = out.foreign_keys.clone();
            out.indexes.retain(|i| !fks.iter().any(|f| f.name == i.name && f.columns == i.columns));
        }
    }
    out
}

/// The source table as the target should become: matched columns keep the target's spelling of
/// things that compare equal (so nothing is altered needlessly) and remember their target name.
fn desired(s: &TableDef, t: &TableDef, from: &SchemaModel, to: &SchemaModel) -> TableDef {
    let mut want = adapt(s, from, to);
    want.name = t.name.clone();
    let cross = from.kind != to.kind;
    let (pairs, _, _) = pair_by_name(&s.columns, &t.columns, |c| c.name.as_str());
    for (i, j) in pairs {
        let (Some(sc), Some(tc)) = (s.columns.get(i), t.columns.get(j)) else { continue };
        let Some(w) = want.columns.get_mut(i) else { continue };
        w.original = tc.name.clone();
        let (ta, da, _, aa) = norm_column(from.kind, sc);
        let (tb, db, _, ab) = norm_column(to.kind, tc);
        let same_type = if cross { ta == tb } else { norm_type_eq(&sc.data_type, &tc.data_type) || ta == tb };
        if same_type {
            w.data_type = tc.data_type.clone();
        }
        if da == db {
            w.default = tc.default.clone();
        }
        if aa == ab {
            w.auto_increment = tc.auto_increment;
            if is_sequence_default(&tc.default) {
                w.default = tc.default.clone();
            }
        }
        if to.kind == DbKind::Sqlite || from.kind == DbKind::Sqlite {
            w.comment = tc.comment.clone();
        }
        // Keep the target's name when only the case differs (a cross-dialect artefact).
        if w.name.to_lowercase() == tc.name.to_lowercase() {
            w.name = tc.name.clone();
        }
    }
    // Keys, indexes and foreign keys that match the target keep the target's names.
    let rename_cols = |cols: &mut Vec<String>, want_cols: &[ColumnDef]| {
        for c in cols.iter_mut() {
            if let Some(w) = want_cols.iter().find(|w| w.name.to_lowercase() == c.to_lowercase()) {
                *c = w.name.clone();
            }
        }
    };
    let cols = want.columns.clone();
    rename_cols(&mut want.primary_key, &cols);
    if want.primary_key.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>()
        == t.primary_key.iter().map(|c| c.to_lowercase()).collect::<Vec<_>>()
    {
        want.primary_key_name = t.primary_key_name.clone();
    }
    for i in &mut want.indexes {
        rename_cols(&mut i.columns, &cols);
    }
    let ti = real_indexes(to.kind, t);
    let (pairs, _, _) = pair(&want.indexes.iter().collect::<Vec<_>>(), &ti, |i| i.name.as_str(), |i| index_sig(i));
    for (i, j) in pairs {
        if let (Some(w), Some(x)) = (want.indexes.get_mut(i), ti.get(j))
            && index_sig(w) == index_sig(x)
        {
            *w = (*x).clone();
        }
    }
    // MySQL keeps the indexes behind its foreign keys: keep them on a MySQL target.
    for x in t.indexes.iter().filter(|x| !ti.iter().any(|y| y.name == x.name)) {
        if !want.indexes.iter().any(|w| w.name == x.name) {
            want.indexes.push(x.clone());
        }
    }
    for f in &mut want.foreign_keys {
        rename_cols(&mut f.columns, &cols);
    }
    let (pairs, _, _) = pair(&want.foreign_keys, &t.foreign_keys, |f| f.name.as_str(), fk_sig);
    for (i, j) in pairs {
        if let (Some(w), Some(x)) = (want.foreign_keys.get_mut(i), t.foreign_keys.get(j))
            && fk_sig(w) == fk_sig(x)
            && w.on_delete == x.on_delete
            && w.on_update == x.on_update
        {
            *w = x.clone();
        }
    }
    let (pairs, _, _) = pair(&want.checks, &t.checks, |_| "", |c| norm_check(&c.expression));
    for (i, j) in pairs {
        if let (Some(w), Some(x)) = (want.checks.get_mut(i), t.checks.get(j)) {
            *w = x.clone();
        }
    }
    if to.kind == DbKind::Sqlite || from.kind == DbKind::Sqlite {
        want.comment = t.comment.clone();
    }
    want.triggers = t.triggers.clone();
    want
}

fn norm_type_eq(a: &str, b: &str) -> bool {
    squash(a).to_lowercase().replace(", ", ",") == squash(b).to_lowercase().replace(", ", ",")
}

fn view_script(kind: DbKind, schema: &str, v: &ViewDef) -> String {
    let what = if v.materialized && kind == DbKind::Postgres { "MATERIALIZED VIEW" } else { "VIEW" };
    format!("CREATE {what} {} AS {}", qualified(kind, schema, &v.name), v.query.trim().trim_end_matches(';').trim())
}

/// The script that makes `target` like `source`, in the target's dialect: drops what only the
/// target has, creates what only the source has and alters the tables that differ.
pub fn migration(source: &SchemaModel, target: &SchemaModel) -> DbResult<DdlScript> {
    let tk = target.kind;
    let mut script = DdlScript { kind: tk, transactional: transactional_ddl(tk), ..DdlScript::default() };
    let cross = source.kind != tk;
    let (pairs, only_s, only_t) = pair_by_name(&source.tables, &target.tables, |t| t.name.as_str());
    let obj = |kind: ObjectKind, name: &str, table: &str| ObjectRef {
        kind,
        schema: target.schema.clone(),
        name: name.to_string(),
        table: table.to_string(),
    };

    // 1. Changed tables: what each should become, and foreign keys to drop before anything else.
    let mut alters: Vec<(TableDef, TableDef)> = Vec::new();
    for (i, j) in &pairs {
        let (Some(s), Some(t)) = (source.tables.get(*i), target.tables.get(*j)) else { continue };
        let want = desired(s, t, source, target);
        let mut old = t.clone().for_modify();
        old.schema = target.schema.clone();
        for c in &mut old.columns {
            c.original.clear();
        }
        if tk != DbKind::Sqlite {
            let dropped: Vec<ForeignKeyDef> =
                t.foreign_keys.iter().filter(|f| !want.foreign_keys.iter().any(|w| w == *f)).cloned().collect();
            for f in &dropped {
                script.extend(drop_object(tk, &obj(ObjectKind::ForeignKey, &f.name, &t.name), false)?);
            }
            old.foreign_keys.retain(|f| !dropped.contains(f));
        }
        alters.push((old, want));
    }
    // 2. Views that go away or change.
    let (view_pairs, views_s, views_t) = pair_by_name(&source.views, &target.views, |v| v.name.as_str());
    let mut changed_views: Vec<usize> = Vec::new();
    for (i, j) in &view_pairs {
        let (Some(a), Some(b)) = (source.views.get(*i), target.views.get(*j)) else { continue };
        if !cross
            && (squash(&a.query).to_lowercase() != squash(&b.query).to_lowercase() || a.materialized != b.materialized)
        {
            let kind = if b.materialized { ObjectKind::MaterializedView } else { ObjectKind::View };
            script.extend(drop_object(tk, &obj(kind, &b.name, ""), false)?);
            changed_views.push(*i);
        }
    }
    for j in &views_t {
        if let Some(v) = target.views.get(*j) {
            let kind =
                if v.materialized && tk == DbKind::Postgres { ObjectKind::MaterializedView } else { ObjectKind::View };
            script.extend(drop_object(tk, &obj(kind, &v.name, ""), false)?);
        }
    }
    // 3. New tables, referenced ones first.
    let added: Vec<TableDef> =
        only_s.iter().filter_map(|i| source.tables.get(*i)).map(|t| adapt(t, source, target)).collect();
    for i in dependency_order(&added) {
        if let Some(t) = added.get(i) {
            script.extend(create_table(tk, t)?);
        }
    }
    // 4. Alter the changed tables.
    for (old, want) in &alters {
        script.extend(alter_table(tk, old, want)?);
    }
    // 5. Drop tables only the target has, referencing ones first.
    let removed: Vec<TableDef> = only_t.iter().filter_map(|j| target.tables.get(*j)).cloned().collect();
    for i in dependency_order(&removed).into_iter().rev() {
        if let Some(t) = removed.get(i) {
            script.extend(drop_object(tk, &obj(ObjectKind::Table, &t.name, ""), false)?);
        }
    }
    // 6. New and changed views.
    for i in views_s.iter().chain(changed_views.iter()) {
        if let Some(v) = source.views.get(*i) {
            script.statements.push(view_script(tk, &target.schema, v));
        }
    }
    if cross && (!views_s.is_empty()) {
        script.warnings.push(format!(
            "View queries are copied from {}; check them before running on {}",
            source.kind.label(),
            tk.label()
        ));
    }
    if cross {
        script.warnings.push(format!("Types converted from {} to {}", source.kind.label(), tk.label()));
    }
    // Keep each warning once (every SQLite rebuild adds its own).
    let mut seen = BTreeSet::new();
    script.warnings.retain(|w| seen.insert(w.clone()));
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl::{FkAction, IndexDef};

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), ..Default::default() }
    }

    fn model(kind: DbKind, schema: &str, tables: Vec<TableDef>) -> SchemaModel {
        SchemaModel { kind, schema: schema.into(), tables, views: Vec::new() }
    }

    fn authors() -> TableDef {
        TableDef {
            name: "authors".into(),
            columns: vec![ColumnDef { nullable: false, ..col("id", "integer") }, col("name", "text")],
            primary_key: vec!["id".into()],
            ..Default::default()
        }
    }

    fn books() -> TableDef {
        TableDef {
            name: "books".into(),
            columns: vec![
                ColumnDef { nullable: false, ..col("id", "integer") },
                col("author_id", "integer"),
                col("title", "varchar(200)"),
            ],
            primary_key: vec!["id".into()],
            indexes: vec![IndexDef { name: "books_title".into(), columns: vec!["title".into()], ..Default::default() }],
            foreign_keys: vec![ForeignKeyDef {
                name: "books_author".into(),
                columns: vec!["author_id".into()],
                ref_table: "authors".into(),
                ref_columns: vec!["id".into()],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn identical_schemas_have_no_diff_and_no_migration() {
        let a = model(DbKind::Postgres, "public", vec![authors(), books()]);
        let d = diff(&a, &a);
        assert!(d.is_empty(), "{d:?}");
        assert!(migration(&a, &a).unwrap().is_empty());
    }

    #[test]
    fn added_removed_changed() {
        let mut src_books = books();
        src_books.columns[2].data_type = "varchar(300)".into();
        src_books.columns.push(col("isbn", "text"));
        src_books.foreign_keys[0].on_delete = FkAction::Cascade;
        let src = model(DbKind::Postgres, "public", vec![authors(), src_books]);
        let mut tgt_authors = authors();
        tgt_authors.columns[1].nullable = false;
        let old = TableDef { name: "old".into(), columns: vec![col("x", "int")], ..Default::default() };
        let tgt = model(DbKind::Postgres, "public", vec![tgt_authors, books(), old]);
        let d = diff(&src, &tgt);
        assert_eq!(d.counts(), (0, 1, 2), "{d:#?}");
        let b = d.items.iter().find(|i| i.name == "books").unwrap();
        assert!(b.children.iter().any(|c| c.name == "isbn" && c.change == Change::Added));
        assert!(b.children.iter().any(|c| c.name == "title" && c.detail.contains("varchar(200) → varchar(300)")));
        assert!(
            b.children.iter().any(|c| c.object == "foreign key" && c.detail.contains("ON DELETE NO ACTION → CASCADE"))
        );
        let m = migration(&src, &tgt).unwrap();
        let sql = m.to_sql();
        let pos = |needle: &str| sql.find(needle).unwrap_or_else(|| panic!("{needle} missing in\n{sql}"));
        // The changed foreign key is dropped first, the old table last, then the key comes back.
        assert!(pos("DROP CONSTRAINT \"books_author\"") < pos("ALTER COLUMN \"title\" TYPE varchar(300)"));
        assert!(pos("ALTER TABLE \"public\".\"authors\" ALTER COLUMN \"name\" DROP NOT NULL") > 0);
        assert!(pos("ADD COLUMN \"isbn\" text") > 0);
        assert!(pos("DROP TABLE \"public\".\"old\"") > pos("ADD COLUMN \"isbn\""));
        assert!(
            pos(
                "ADD CONSTRAINT \"books_author\" FOREIGN KEY (\"author_id\") REFERENCES \"public\".\"authors\" (\"id\") ON DELETE CASCADE"
            ) > 0
        );
        assert!(sql.starts_with("BEGIN;") && sql.trim_end().ends_with("COMMIT;"));
    }

    #[test]
    fn new_tables_are_created_in_dependency_order() {
        let src = model(DbKind::Sqlite, "main", vec![books(), authors()]);
        let tgt = model(DbKind::Sqlite, "main", vec![]);
        let m = migration(&src, &tgt).unwrap();
        let a = m.statements.iter().position(|s| s.contains("CREATE TABLE \"main\".\"authors\"")).unwrap();
        let b = m.statements.iter().position(|s| s.contains("CREATE TABLE \"main\".\"books\"")).unwrap();
        assert!(a < b, "{:?}", m.statements);
        assert_eq!(dependency_order(&[books(), authors()]), vec![1, 0]);
        // A cycle keeps its order and does not hang.
        let mut a2 = authors();
        a2.foreign_keys.push(ForeignKeyDef {
            name: "x".into(),
            columns: vec!["id".into()],
            ref_table: "books".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        assert_eq!(dependency_order(&[books(), a2]), vec![0, 1]);
    }

    #[test]
    fn cross_dialect_compares_by_family() {
        let mut my_books = books();
        my_books.columns[0].data_type = "int(11)".into();
        my_books.columns[1].data_type = "int".into();
        my_books.columns[2].data_type = "varchar(200)".into();
        // MySQL's own index behind the foreign key is not a difference.
        my_books.indexes.push(IndexDef {
            name: "books_author".into(),
            columns: vec!["author_id".into()],
            ..Default::default()
        });
        my_books.indexes[0].name = "title_idx".into();
        let mut my_authors = authors();
        my_authors.columns[0].data_type = "int".into();
        my_authors.columns[1].data_type = "longtext".into();
        let my = model(DbKind::Mysql, "app", vec![my_authors, my_books]);
        let mut pg_books = books();
        pg_books.columns[2].data_type = "character varying(200)".into();
        let mut pg_authors = authors();
        pg_authors.columns[0].default = "nextval('authors_id_seq'::regclass)".into();
        pg_authors.columns[0].data_type = "integer".into();
        let mut my2 = my.clone();
        if let Some(c) = my2.tables[0].columns.get_mut(0) {
            c.auto_increment = true;
        }
        let pg = model(DbKind::Postgres, "public", vec![pg_authors, pg_books]);
        let d = diff(&my2, &pg);
        assert!(d.is_empty(), "{d:#?}");
        assert!(!d.notes.is_empty());
        assert!(migration(&my2, &pg).unwrap().is_empty(), "{}", migration(&my2, &pg).unwrap().to_sql());
        // Into an empty SQLite schema: types converted, references unqualified.
        let lite = model(DbKind::Sqlite, "main", vec![]);
        let m = migration(&pg, &lite).unwrap().to_sql();
        assert!(m.contains("\"title\" VARCHAR(200)"), "{m}");
        assert!(m.contains("REFERENCES \"authors\" (\"id\")"), "{m}");
        assert!(m.contains("Types converted from PostgreSQL to SQLite"));
    }

    #[test]
    fn type_helpers() {
        assert_eq!(type_family(DbKind::Mysql, "INT(11) unsigned"), "integer");
        assert_eq!(type_family(DbKind::Mysql, "tinyint(1)"), "boolean");
        assert_eq!(type_family(DbKind::Postgres, "numeric(10, 2)"), "numeric(10,2)");
        assert_eq!(type_family(DbKind::Postgres, "timestamp without time zone"), "timestamp");
        assert_eq!(convert_type(DbKind::Postgres, DbKind::Mysql, "boolean"), "tinyint(1)");
        assert_eq!(convert_type(DbKind::Mysql, DbKind::Postgres, "datetime"), "timestamp");
        assert_eq!(convert_type(DbKind::Postgres, DbKind::Sqlite, "bytea"), "BLOB");
        assert_eq!(convert_type(DbKind::Postgres, DbKind::Postgres, " citext "), "citext");
        assert_eq!(convert_type(DbKind::Postgres, DbKind::Mysql, "citext"), "citext");
        assert_eq!(norm_default("'a'::character varying"), "'a'");
        assert_eq!(norm_default("'x::y'"), "'x::y'");
        assert_eq!(norm_default("(0)"), "0");
        assert_eq!(norm_check("((price > (0)::numeric))"), norm_check("price > 0"));
    }

    #[test]
    fn views_are_dropped_and_created() {
        let mut src = model(DbKind::Postgres, "s", vec![authors()]);
        src.views.push(ViewDef { name: "v".into(), materialized: false, query: "SELECT id FROM authors".into() });
        src.views.push(ViewDef { name: "w".into(), materialized: false, query: "SELECT 2".into() });
        let mut tgt = model(DbKind::Postgres, "s", vec![authors()]);
        tgt.views.push(ViewDef { name: "v".into(), materialized: false, query: "SELECT 1".into() });
        tgt.views.push(ViewDef { name: "gone".into(), materialized: true, query: "SELECT 3".into() });
        let d = diff(&src, &tgt);
        assert_eq!(d.counts(), (1, 1, 1));
        let m = migration(&src, &tgt).unwrap();
        assert_eq!(
            m.statements,
            vec![
                "DROP VIEW \"s\".\"v\"",
                "DROP MATERIALIZED VIEW \"s\".\"gone\"",
                "CREATE VIEW \"s\".\"w\" AS SELECT 2",
                "CREATE VIEW \"s\".\"v\" AS SELECT id FROM authors",
            ]
        );
    }
}
