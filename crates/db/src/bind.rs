//! Bind parameters: values travel next to the SQL instead of inside it.
//!
//! Every statement CodeCraft builds from values (grid edits, imports, Compare Data sync runs,
//! Data Tools searches) and every console or agent statement with parameters becomes a
//! [`Bound`]: SQL in the connection's native placeholder style plus the values, which
//! [`Connection::execute_bound`](crate::Connection::execute_bound) sends separately.
//!
//! | Database | Placeholders | How values travel |
//! |---|---|---|
//! | PostgreSQL | `$1`, `$2` | extended protocol; text format typed by the server's inference (`bytea` binary) |
//! | MySQL / MariaDB | `?` | binary protocol (prepared statement) |
//! | SQLite | `?1`, `?2` | `sqlite3_bind_*` (integer, real, text, blob) |
//! | SQL Server | `@P1`, `@P2` | `sp_executesql` RPC parameters (`nvarchar`, `bigint`, `float`, `bit`, `varbinary`) |
//! | ClickHouse | `{p1:String}` | HTTP query parameters `param_p1=…` (console and agent parameters) |
//! | DuckDB (CLI), Redis | — | no binding: escaped literals ([`crate::edit::literal`]) |
//!
//! NULL is always the `NULL` keyword (no typed NULL parameter is needed, and `= NULL`
//! semantics are the same either way). ClickHouse binds console and agent parameters; the DML
//! CodeCraft generates for it (imports) keeps escaped literals because ClickHouse parameters
//! travel in the URL, whose size the server limits. [`Bound::preview`] says which applies.
//!
//! [`prepare`] rewrites user SQL with `?`, `?NNN`, `$1`, `:name` or `${name}` placeholders
//! ([`crate::params::find_params`]) — and ClickHouse's own `{name:Type}` — into the native form.
//! A parameter's value never changes what kind of statement it is: only the placeholders are
//! replaced, so a statement classified as a read stays one. The exception is
//! [`ParamValue::Sql`], an expression the user typed into the parameter prompt on purpose,
//! which is inlined as typed (see [`expand_sql_values`] for classifying such a statement).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::params::find_params;
use crate::{Cell, DbError, DbKind, DbResult, Dialect};

/// A statement and the values of its placeholders, in order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Bound {
    pub sql: String,
    pub params: Vec<Cell>,
}

impl Bound {
    /// A statement without parameters.
    pub fn plain(sql: impl Into<String>) -> Self {
        Self { sql: sql.into(), params: Vec::new() }
    }

    pub fn has_params(&self) -> bool {
        !self.params.is_empty()
    }

    /// The statement as shown before running it: the SQL, then (when values are bound) a
    /// comment line with each placeholder's value as a literal (long values shortened, line
    /// breaks escaped).
    pub fn preview(&self, kind: DbKind) -> String {
        if self.params.is_empty() {
            return self.sql.clone();
        }
        let parts: Vec<String> = self
            .params
            .iter()
            .enumerate()
            .map(|(i, v)| format!("{} = {}", param_label(kind, i + 1), preview_value(kind, v)))
            .collect();
        format!("{}\n-- parameters: {}", self.sql, parts.join(", "))
    }

    /// The statement with every placeholder replaced by its value as an escaped literal (for
    /// scripts written to files and the clipboard). Fails when a value has no literal.
    pub fn inline(&self, kind: DbKind) -> DbResult<String> {
        if self.params.is_empty() {
            return Ok(self.sql.clone());
        }
        let chars: Vec<char> = self.sql.chars().collect();
        let spans = native_spans(kind, &chars);
        if spans.len() != self.params.len() {
            return Err(DbError::Query(format!(
                "the statement has {} placeholders for {} values",
                spans.len(),
                self.params.len()
            )));
        }
        let mut out = String::with_capacity(self.sql.len() + 16 * self.params.len());
        let mut last = 0usize;
        for ((start, end, n), _) in spans.iter().zip(&self.params) {
            out.extend(chars.get(last..*start).unwrap_or(&[]).iter());
            let v = n.checked_sub(1).and_then(|i| self.params.get(i)).unwrap_or(&Cell::Null);
            out.push_str(&crate::edit::literal(kind, v)?);
            last = *end;
        }
        out.extend(chars.get(last.min(chars.len())..).unwrap_or(&[]).iter());
        Ok(out)
    }
}

/// `$1`, `?1`, `@P1`, `{p1}` — how the preview names placeholder `n` (1-based).
pub fn param_label(kind: DbKind, n: usize) -> String {
    match kind {
        DbKind::Postgres => format!("${n}"),
        DbKind::SqlServer => format!("@P{n}"),
        DbKind::ClickHouse => format!("p{n}"),
        _ => format!("?{n}"),
    }
}

/// Longest value text shown in a preview.
const PREVIEW_VALUE_CHARS: usize = 120;

fn preview_value(kind: DbKind, v: &Cell) -> String {
    let lit = crate::edit::literal(kind, v).unwrap_or_else(|_| format!("'{}'", v.to_plain().replace('\'', "''")));
    let mut out = String::new();
    for (i, c) in lit.chars().enumerate() {
        if i >= PREVIEW_VALUE_CHARS {
            out.push_str(&format!("… ({} characters)", lit.chars().count()));
            break;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

/// The native placeholders of `chars` as `(start, end, number)` in order (what [`Binder`]
/// and [`prepare`] write; inside strings and comments nothing counts).
fn native_spans(kind: DbKind, chars: &[char]) -> Vec<(usize, usize, usize)> {
    let at = |k: usize| chars.get(k).copied();
    let mut out = Vec::new();
    let toks = crate::lex::tokens(chars, kind.dialect());
    let mut skip_until = 0usize;
    let mut positional = 0usize;
    for tok in toks {
        let crate::lex::Tok::Punct(i) = tok else {
            // SQL Server `@P1` is a word starting after `@` (punctuation): handled below.
            continue;
        };
        if i < skip_until {
            continue;
        }
        let digits_from = |from: usize| {
            let mut end = from;
            while at(end).is_some_and(|d| d.is_ascii_digit()) {
                end += 1;
            }
            end
        };
        let number = |from: usize, end: usize| -> Option<usize> {
            chars.get(from..end)?.iter().collect::<String>().parse::<usize>().ok()
        };
        match (kind, at(i)) {
            (DbKind::Postgres, Some('$')) => {
                let end = digits_from(i + 1);
                if let Some(n) = number(i + 1, end) {
                    out.push((i, end, n));
                    skip_until = end;
                }
            }
            (DbKind::Sqlite, Some('?')) => {
                let end = digits_from(i + 1);
                let n = number(i + 1, end).unwrap_or_else(|| positional + 1);
                positional = n;
                out.push((i, end, n));
                skip_until = end;
            }
            (DbKind::Mysql | DbKind::Mariadb, Some('?')) => {
                positional += 1;
                out.push((i, i + 1, positional));
            }
            (DbKind::SqlServer, Some('@')) if at(i + 1) == Some('P') => {
                let end = digits_from(i + 2);
                if let Some(n) = number(i + 2, end).filter(|_| !at(end).is_some_and(crate::split::is_ident_char)) {
                    out.push((i, end, n));
                    skip_until = end;
                }
            }
            (DbKind::ClickHouse, Some('{')) if at(i + 1) == Some('p') => {
                let end = digits_from(i + 2);
                if let Some(n) = number(i + 2, end).filter(|_| at(end) == Some(':')) {
                    let mut close = end;
                    while at(close).is_some_and(|c| c != '}' && c != '\n') && close < end + 200 {
                        close += 1;
                    }
                    if at(close) == Some('}') {
                        out.push((i, close + 1, n));
                        skip_until = close + 1;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether the driver sends values as bind parameters.
pub fn binds(kind: DbKind) -> bool {
    !matches!(kind, DbKind::DuckDb | DbKind::Redis)
}

/// Whether the statements CodeCraft generates (edits, imports, sync runs) bind their values.
/// ClickHouse parameters travel in the URL, so its bulk statements keep escaped literals.
pub fn binds_generated(kind: DbKind) -> bool {
    binds(kind) && kind != DbKind::ClickHouse
}

/// Why values are inlined as literals for `kind`, when they are.
pub fn literal_reason(kind: DbKind) -> Option<String> {
    match kind {
        DbKind::DuckDb => Some("the DuckDB command-line shell cannot bind parameters".into()),
        DbKind::Redis => Some("Redis commands have no bind parameters".into()),
        DbKind::ClickHouse => {
            Some("ClickHouse parameters travel in the request URL, so generated statements inline their values".into())
        }
        _ => None,
    }
}

/// Most parameters one statement may carry (the server's limit, with a margin).
pub fn max_params(kind: DbKind) -> usize {
    match kind {
        DbKind::Postgres | DbKind::Mysql | DbKind::Mariadb => 65_000,
        DbKind::Sqlite => 32_000,
        // sp_executesql takes at most 2100 parameters.
        DbKind::SqlServer => 2_000,
        DbKind::ClickHouse => 1_000,
        DbKind::DuckDb | DbKind::Redis => usize::MAX,
    }
}

/// Placeholder `n` (1-based) for `value` in `kind`'s native style.
pub fn placeholder(kind: DbKind, n: usize, value: &Cell) -> String {
    match kind {
        DbKind::Postgres => format!("${n}"),
        DbKind::Sqlite => format!("?{n}"),
        DbKind::SqlServer => format!("@P{n}"),
        DbKind::ClickHouse => format!("{{p{n}:{}}}", clickhouse_type(value)),
        _ => "?".into(),
    }
}

/// The ClickHouse type a value is declared with.
pub fn clickhouse_type(value: &Cell) -> &'static str {
    match value {
        Cell::Bool(_) => "Bool",
        Cell::Int(_) => "Int64",
        Cell::UInt(_) => "UInt64",
        Cell::Float(_) => "Float64",
        Cell::Null => "Nullable(String)",
        Cell::Text(_) | Cell::Other(_) | Cell::Bytes(_) => "String",
    }
}

/// Builds one statement's values: each value becomes a placeholder (and a parameter) or, where
/// the driver can't bind, an escaped literal.
#[derive(Debug)]
pub struct Binder {
    kind: DbKind,
    bind: bool,
    params: Vec<Cell>,
}

impl Binder {
    /// For statements CodeCraft generates: binds where [`binds_generated`].
    pub fn new(kind: DbKind) -> Self {
        Self { kind, bind: binds_generated(kind), params: Vec::new() }
    }

    /// For user parameters (consoles, agents): binds where [`binds`].
    pub fn for_user(kind: DbKind) -> Self {
        Self { kind, bind: binds(kind), params: Vec::new() }
    }

    /// Never binds: every value is a literal (scripts written to files).
    pub fn literals(kind: DbKind) -> Self {
        Self { kind, bind: false, params: Vec::new() }
    }

    pub fn binds(&self) -> bool {
        self.bind
    }

    /// The SQL text that stands for `value`.
    pub fn value(&mut self, value: &Cell) -> DbResult<String> {
        if self.kind == DbKind::Redis {
            // No binding: one quoted argument the console's parser reads back exactly.
            return match value {
                Cell::Null => Err(DbError::Unsupported("Redis has no NULL: pass a value".into())),
                Cell::Bytes(b) => Ok(crate::redis::quote_arg(b)),
                other => Ok(crate::redis::quote_arg(other.to_plain().as_bytes())),
            };
        }
        if value.is_null() || !self.bind {
            return crate::edit::literal(self.kind, value);
        }
        if let Cell::Float(f) = value
            && !f.is_finite()
            && !matches!(self.kind, DbKind::Postgres | DbKind::ClickHouse)
        {
            return Err(DbError::Unsupported(format!("{} cannot store {f}", self.kind.label())));
        }
        if self.params.len() >= max_params(self.kind) {
            return Err(DbError::Unsupported(format!(
                "a statement takes at most {} parameters on {}",
                max_params(self.kind),
                self.kind.label()
            )));
        }
        self.params.push(value.clone());
        Ok(placeholder(self.kind, self.params.len(), value))
    }

    /// Values bound so far.
    pub fn len(&self) -> usize {
        self.params.len()
    }

    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    pub fn finish(self, sql: String) -> Bound {
        Bound { sql, params: self.params }
    }
}

/// A parameter's value: data to bind, or SQL typed into the parameter prompt on purpose.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParamValue {
    Value(Cell),
    /// Inlined as typed (the prompt's "SQL expression" kind; never from agents' `params`).
    Sql(String),
}

/// Values by parameter name.
pub type ParamValues = BTreeMap<String, ParamValue>;

/// How the parameter prompt reads what was typed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ParamKind {
    /// `NULL` is NULL, `42` an integer, `true`/`false` booleans, `'it''s'` the text inside the
    /// quotes; anything else is text as typed.
    #[default]
    Auto,
    /// Exactly the text typed.
    Text,
    /// An SQL expression inserted as typed (`now()`, a subquery).
    Sql,
}

impl ParamKind {
    pub fn parse(s: &str) -> Option<ParamKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "value" => Some(ParamKind::Auto),
            "text" | "string" => Some(ParamKind::Text),
            "sql" | "expression" | "expr" => Some(ParamKind::Sql),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ParamKind::Auto => "Value",
            ParamKind::Text => "Text",
            ParamKind::Sql => "SQL expression",
        }
    }
}

/// What the prompt's text means for `kind`.
pub fn parse_param(text: &str, kind: ParamKind) -> ParamValue {
    match kind {
        ParamKind::Text => ParamValue::Value(Cell::Text(text.to_string())),
        ParamKind::Sql => ParamValue::Sql(text.to_string()),
        ParamKind::Auto => ParamValue::Value(auto_value(text)),
    }
}

fn auto_value(text: &str) -> Cell {
    let t = text.trim();
    if t.eq_ignore_ascii_case("null") {
        return Cell::Null;
    }
    if t.eq_ignore_ascii_case("true") {
        return Cell::Bool(true);
    }
    if t.eq_ignore_ascii_case("false") {
        return Cell::Bool(false);
    }
    // Integers in canonical form only: `007` and `+1` stay text (codes, phone numbers).
    let digits = t.strip_prefix('-').unwrap_or(t);
    let canonical = !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && (digits == "0" || !digits.starts_with('0'))
        && t != "-0";
    if canonical {
        if let Ok(i) = t.parse::<i64>() {
            return Cell::Int(i);
        }
        if let Ok(u) = t.parse::<u64>() {
            return Cell::UInt(u);
        }
    }
    if t.len() >= 2
        && t.starts_with('\'')
        && t.ends_with('\'')
        && let Some(inner) = t.get(1..t.len() - 1)
        && !inner.replace("''", "").contains('\'')
    {
        return Cell::Text(inner.replace("''", "'"));
    }
    Cell::Text(text.to_string())
}

/// A JSON value from an agent or a command (`params`): null, booleans, numbers and strings bind
/// as themselves; `{"hex": "00ff"}` binds bytes; `{"sql": "now()"}` is an expression, refused
/// when `allow_sql` is false.
pub fn param_from_json(v: &serde_json::Value, allow_sql: bool) -> Result<ParamValue, String> {
    use serde_json::Value as J;
    Ok(ParamValue::Value(match v {
        J::Null => Cell::Null,
        J::Bool(b) => Cell::Bool(*b),
        J::Number(n) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => Cell::Int(i),
            (_, Some(u), _) => Cell::UInt(u),
            (_, _, Some(f)) => Cell::Float(f),
            _ => Cell::Other(n.to_string()),
        },
        J::String(s) => Cell::Text(s.clone()),
        J::Object(m) => {
            if let Some(h) = m.get("hex").and_then(J::as_str) {
                return parse_hex(h).map(|b| ParamValue::Value(Cell::Bytes(b)));
            }
            if let Some(t) = m.get("text").and_then(J::as_str) {
                return Ok(ParamValue::Value(Cell::Text(t.to_string())));
            }
            if let Some(sql) = m.get("sql").and_then(J::as_str) {
                if !allow_sql {
                    return Err("SQL expressions are not accepted as parameter values here; pass the value itself".into());
                }
                return Ok(ParamValue::Sql(sql.to_string()));
            }
            return Err("a parameter object is {\"hex\": \"…\"}, {\"text\": \"…\"} or {\"sql\": \"…\"}".into());
        }
        J::Array(_) => return Err("a parameter value cannot be an array".into()),
    }))
}

fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let t = s.trim();
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("\\x")).unwrap_or(t);
    if !t.len().is_multiple_of(2) {
        return Err("hex bytes need an even number of digits".into());
    }
    let b = t.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    for pair in b.chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| "not hex".to_string())?;
        out.push(u8::from_str_radix(s, 16).map_err(|_| format!("`{s}` is not a hex byte"))?);
    }
    Ok(out)
}

/// `params` of an agent or command call: an object `{name: value}` (names with or without
/// their `:`/`$`/`?` sigil) or an array of positional values (`?`/`$n` by number, named
/// parameters in order of first appearance). See [`param_from_json`].
pub fn values_from_json(
    sql_names: &[String],
    v: &serde_json::Value,
    allow_sql: bool,
) -> Result<Option<ParamValues>, String> {
    use serde_json::Value as J;
    match v {
        J::Null => Ok(None),
        J::Object(m) => {
            let mut out = ParamValues::new();
            for (k, x) in m {
                out.insert(k.clone(), param_from_json(x, allow_sql).map_err(|e| format!("parameter `{k}`: {e}"))?);
            }
            Ok(Some(out))
        }
        J::Array(a) => {
            let numbered = sql_names.iter().filter(|n| positional_number(n).is_some()).count();
            if numbered > 0 && numbered < sql_names.len() {
                return Err("the SQL mixes positional and named parameters: pass `params` as an object".into());
            }
            let mut out = ParamValues::new();
            let mut named = 0usize;
            let mut used = vec![false; a.len()];
            for name in sql_names {
                let index = match positional_number(name) {
                    Some(n) => n.saturating_sub(1),
                    None => {
                        named += 1;
                        named - 1
                    }
                };
                let Some(x) = a.get(index) else {
                    return Err(format!("no value for parameter `{name}` ({} values given)", a.len()));
                };
                if let Some(u) = used.get_mut(index) {
                    *u = true;
                }
                out.insert(name.clone(), param_from_json(x, allow_sql).map_err(|e| format!("parameter `{name}`: {e}"))?);
            }
            if let Some(extra) = used.iter().position(|u| !u) {
                return Err(format!(
                    "{} values for {} parameters (value {} has no placeholder)",
                    a.len(),
                    sql_names.len(),
                    extra + 1
                ));
            }
            Ok(Some(out))
        }
        _ => Err("`params` must be an object {name: value} or an array of values".into()),
    }
}

/// `?3` → 3, `$2` → 2; named parameters → None.
fn positional_number(name: &str) -> Option<usize> {
    let digits = name.strip_prefix('?').or_else(|| name.strip_prefix('$'))?;
    digits.parse::<usize>().ok().filter(|n| *n > 0)
}

/// The value for parameter `name`: by its exact name, else by its bare name (`:id`, `${id}`,
/// `$id` and `id` are the same parameter).
fn lookup<'a>(values: &'a ParamValues, name: &str) -> Option<&'a ParamValue> {
    if let Some(v) = values.get(name) {
        return Some(v);
    }
    let bare = bare_name(name);
    values.iter().find(|(k, _)| bare_name(k) == bare).map(|(_, v)| v)
}

fn bare_name(name: &str) -> &str {
    let n = name.strip_prefix("${").and_then(|n| n.strip_suffix('}')).unwrap_or(name);
    n.trim_start_matches([':', '$', '?', '@'])
}

/// The parameters of user SQL, in order of first appearance: [`crate::params::param_names`],
/// plus ClickHouse's own `{name:Type}` placeholders.
pub fn user_param_names(kind: DbKind, sql: &str) -> Vec<String> {
    let mut names = crate::params::param_names(sql, kind.dialect());
    if kind == DbKind::ClickHouse {
        let chars: Vec<char> = sql.chars().collect();
        for (_, _, name, _) in clickhouse_native(&chars) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// ClickHouse `{name:Type}` placeholders: `(start, end, name, type)`.
fn clickhouse_native(chars: &[char]) -> Vec<(usize, usize, String, String)> {
    let at = |k: usize| chars.get(k).copied();
    let mut out = Vec::new();
    for tok in crate::lex::tokens(chars, Dialect::Mysql) {
        let crate::lex::Tok::Punct(i) = tok else { continue };
        if at(i) != Some('{') || !at(i + 1).is_some_and(crate::split::is_ident_start) {
            continue;
        }
        let mut colon = i + 1;
        while at(colon).is_some_and(crate::split::is_ident_char) {
            colon += 1;
        }
        if at(colon) != Some(':') {
            continue;
        }
        let mut close = colon + 1;
        while at(close).is_some_and(|c| c != '}' && c != '\n' && c != '{') && close < colon + 200 {
            close += 1;
        }
        if at(close) != Some('}') || close == colon + 1 {
            continue;
        }
        let name: String = chars.get(i + 1..colon).unwrap_or(&[]).iter().collect();
        let ty: String = chars.get(colon + 1..close).unwrap_or(&[]).iter().collect::<String>().trim().to_string();
        out.push((i, close + 1, name, ty));
    }
    out
}

/// Rewrite user SQL's placeholders into `kind`'s native ones with their values (or literals
/// where the driver can't bind). Fails with the first parameter that has no value.
pub fn prepare(kind: DbKind, sql: &str, values: &ParamValues) -> Result<Bound, String> {
    let chars: Vec<char> = sql.chars().collect();
    // (start, end, name, ClickHouse type when the user declared one)
    let mut spans: Vec<(usize, usize, String, Option<String>)> =
        find_params(sql, kind.dialect()).into_iter().map(|p| (p.start, p.end, p.name, None)).collect();
    if kind == DbKind::ClickHouse {
        for (s, e, name, ty) in clickhouse_native(&chars) {
            if lookup(values, &name).is_some() {
                spans.push((s, e, name, Some(ty)));
            }
        }
        spans.sort_by_key(|(s, ..)| *s);
    }
    let mut binder = Binder::for_user(kind);
    let mut out = String::with_capacity(sql.len());
    let mut last = 0usize;
    for (start, end, name, ch_type) in spans {
        if start < last {
            continue;
        }
        let value = lookup(values, &name).ok_or_else(|| format!("no value for parameter `{name}`"))?;
        out.extend(chars.get(last..start).unwrap_or(&[]).iter());
        match value {
            ParamValue::Sql(text) => out.push_str(text),
            ParamValue::Value(v) => match ch_type {
                Some(ty) if binder.binds() && !v.is_null() => {
                    // Keep the type the user declared (`{id:UInt32}`).
                    binder.value(v).map_err(|e| e.to_string())?;
                    out.push_str(&format!("{{p{}:{ty}}}", binder.len()));
                }
                _ => out.push_str(&binder.value(v).map_err(|e| e.to_string())?),
            },
        }
        last = end;
    }
    out.extend(chars.get(last.min(chars.len())..).unwrap_or(&[]).iter());
    Ok(binder.finish(out))
}

/// The statement with only its SQL-expression values inlined (bound values stay placeholders):
/// what decides whether it reads or writes.
pub fn expand_sql_values(sql: &str, dialect: Dialect, values: &ParamValues) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len());
    let mut last = 0usize;
    for p in find_params(sql, dialect) {
        let Some(ParamValue::Sql(text)) = lookup(values, &p.name) else { continue };
        out.extend(chars.get(last..p.start).unwrap_or(&[]).iter());
        out.push_str(text);
        last = p.end;
    }
    out.extend(chars.get(last.min(chars.len())..).unwrap_or(&[]).iter());
    out
}

/// A ClickHouse query parameter value in the `Escaped` text format the server parses it with.
pub fn clickhouse_param_text(value: &Cell) -> String {
    match value {
        Cell::Null => "\\N".into(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) if f.is_nan() => "nan".into(),
        Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "inf" } else { "-inf" }.into(),
        Cell::Float(f) => f.to_string(),
        Cell::Text(s) | Cell::Other(s) => {
            let mut out = String::with_capacity(s.len());
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\0' => out.push_str("\\0"),
                    c => out.push(c),
                }
            }
            out
        }
        Cell::Bytes(b) => {
            let mut out = String::with_capacity(b.len() * 4);
            for byte in b {
                out.push_str(&format!("\\x{byte:02X}"));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vals(pairs: &[(&str, Cell)]) -> ParamValues {
        pairs.iter().map(|(k, v)| ((*k).to_string(), ParamValue::Value(v.clone()))).collect()
    }

    #[test]
    fn binder_uses_native_placeholders_and_inlines_null() {
        let mut b = Binder::new(DbKind::Postgres);
        assert_eq!(b.value(&Cell::Text("x".into())).unwrap(), "$1");
        assert_eq!(b.value(&Cell::Null).unwrap(), "NULL");
        assert_eq!(b.value(&Cell::Int(3)).unwrap(), "$2");
        let bound = b.finish("select $1, NULL, $2".into());
        assert_eq!(bound.params, vec![Cell::Text("x".into()), Cell::Int(3)]);
        let mut b = Binder::new(DbKind::Mysql);
        assert_eq!(b.value(&Cell::Int(1)).unwrap(), "?");
        let mut b = Binder::new(DbKind::Sqlite);
        b.value(&Cell::Int(1)).unwrap();
        assert_eq!(b.value(&Cell::Int(1)).unwrap(), "?2");
        let mut b = Binder::new(DbKind::SqlServer);
        assert_eq!(b.value(&Cell::Int(1)).unwrap(), "@P1");
        // Generated ClickHouse and DuckDB statements inline literals.
        let mut b = Binder::new(DbKind::ClickHouse);
        assert_eq!(b.value(&Cell::Text("it's".into())).unwrap(), "'it\\'s'");
        assert!(b.is_empty());
        let mut b = Binder::new(DbKind::DuckDb);
        assert_eq!(b.value(&Cell::Text("a'b".into())).unwrap(), "'a''b'");
        let mut b = Binder::for_user(DbKind::ClickHouse);
        assert_eq!(b.value(&Cell::Text("x".into())).unwrap(), "{p1:String}");
        assert_eq!(b.value(&Cell::Int(-1)).unwrap(), "{p2:Int64}");
        // Infinity binds only where the type exists.
        assert!(Binder::new(DbKind::Sqlite).value(&Cell::Float(f64::INFINITY)).is_err());
        assert_eq!(Binder::new(DbKind::Postgres).value(&Cell::Float(f64::NAN)).unwrap(), "$1");
    }

    #[test]
    fn preview_lists_values_and_inline_restores_literals() {
        let b = Bound {
            sql: "UPDATE \"t\" SET \"a\" = $1 WHERE \"id\" = $2".into(),
            params: vec![Cell::Text("it's\nok".into()), Cell::Int(3)],
        };
        assert_eq!(
            b.preview(DbKind::Postgres),
            "UPDATE \"t\" SET \"a\" = $1 WHERE \"id\" = $2\n-- parameters: $1 = 'it''s\\nok', $2 = 3"
        );
        assert_eq!(b.inline(DbKind::Postgres).unwrap(), "UPDATE \"t\" SET \"a\" = 'it''s\nok' WHERE \"id\" = 3");
        let long = Bound { sql: "select ?1".into(), params: vec![Cell::Text("x".repeat(500))] };
        assert!(long.preview(DbKind::Sqlite).contains("(502 characters)"));
        assert_eq!(Bound::plain("select 1").preview(DbKind::Sqlite), "select 1");
        let my = Bound { sql: "select ?, '?', ?".into(), params: vec![Cell::Int(1), Cell::Text("b".into())] };
        assert_eq!(my.inline(DbKind::Mysql).unwrap(), "select 1, '?', 'b'");
        let ms = Bound { sql: "select @P1, @P2x".into(), params: vec![Cell::Int(1)] };
        assert_eq!(ms.inline(DbKind::SqlServer).unwrap(), "select 1, @P2x");
        let ch = Bound { sql: "select {p1:String}".into(), params: vec![Cell::Text("a".into())] };
        assert_eq!(ch.inline(DbKind::ClickHouse).unwrap(), "select 'a'");
        let bad = Bound { sql: "select 1".into(), params: vec![Cell::Int(1)] };
        assert!(bad.inline(DbKind::Postgres).is_err());
    }

    #[test]
    fn prepare_rewrites_every_style() {
        let v = vals(&[("?1", Cell::Text("Ann".into())), (":id", Cell::Int(42))]);
        let b = prepare(DbKind::Postgres, "select * from t where name = ? and id = :id or id = :id", &v).unwrap();
        assert_eq!(b.sql, "select * from t where name = $1 and id = $2 or id = $3");
        assert_eq!(b.params, vec![Cell::Text("Ann".into()), Cell::Int(42), Cell::Int(42)]);
        let b = prepare(DbKind::Mysql, "select ?, :id", &v).unwrap();
        assert_eq!(b.sql, "select ?, ?");
        let b = prepare(DbKind::SqlServer, "select :id", &v).unwrap();
        assert_eq!((b.sql.as_str(), b.params.len()), ("select @P1", 1));
        // Bare names match any sigil; NULL is the keyword.
        let v = vals(&[("id", Cell::Null)]);
        let b = prepare(DbKind::Sqlite, "select :id, ${id}", &v).unwrap();
        assert_eq!((b.sql.as_str(), b.params.len()), ("select NULL, NULL", 0));
        assert_eq!(prepare(DbKind::Sqlite, "select :x", &v).unwrap_err(), "no value for parameter `:x`");
        // Strings and comments keep their text.
        let v = vals(&[(":a", Cell::Text("x".into()))]);
        let b = prepare(DbKind::Postgres, "select ':a', :a -- :a", &v).unwrap();
        assert_eq!(b.sql, "select ':a', $1 -- :a");
        // DuckDB inlines escaped literals.
        let v = vals(&[(":a", Cell::Text("it's".into()))]);
        let b = prepare(DbKind::DuckDb, "select :a", &v).unwrap();
        assert_eq!((b.sql.as_str(), b.params.len()), ("select 'it''s'", 0));
        // SQL expressions are inlined as typed.
        let mut v = ParamValues::new();
        v.insert(":t".into(), ParamValue::Sql("now()".into()));
        assert_eq!(prepare(DbKind::Postgres, "select :t", &v).unwrap().sql, "select now()");
        assert_eq!(expand_sql_values("select :t, :u", Dialect::Postgres, &v), "select now(), :u");
    }

    #[test]
    fn redis_arguments_are_quoted_safely() {
        let hostile = "a \"b\" \\ c\nd\0 🦀";
        let v = vals(&[(":v", Cell::Text(hostile.into())), (":b", Cell::Bytes(vec![0, 255, b' ']))]);
        let b = prepare(DbKind::Redis, "SET k :v", &v).unwrap();
        assert!(b.params.is_empty());
        let args = crate::redis::split_args(&b.sql).unwrap();
        assert_eq!(args, vec![b"SET".to_vec(), b"k".to_vec(), hostile.as_bytes().to_vec()]);
        let b = prepare(DbKind::Redis, "SET k :b", &v).unwrap();
        assert_eq!(crate::redis::split_args(&b.sql).unwrap().get(2), Some(&vec![0, 255, b' ']));
        assert!(prepare(DbKind::Redis, "GET :n", &vals(&[(":n", Cell::Null)])).is_err());
    }

    #[test]
    fn clickhouse_native_parameters() {
        let v = vals(&[("id", Cell::Int(7)), (":name", Cell::Text("a".into()))]);
        let b = prepare(DbKind::ClickHouse, "select * from t where id = {id:UInt32} and n = :name", &v).unwrap();
        assert_eq!(b.sql, "select * from t where id = {p1:UInt32} and n = {p2:String}");
        assert_eq!(b.params, vec![Cell::Int(7), Cell::Text("a".into())]);
        assert_eq!(user_param_names(DbKind::ClickHouse, "select {id:UInt32}, '{x:String}'"), ["id"]);
        assert!(user_param_names(DbKind::Postgres, "select '{id:Int8}'").is_empty());
        assert_eq!(clickhouse_param_text(&Cell::Text("a\\b\tc\nd\0'".into())), "a\\\\b\\tc\\nd\\0'");
        assert_eq!(clickhouse_param_text(&Cell::Bytes(vec![0, 0xff])), "\\x00\\xFF");
        assert_eq!(clickhouse_param_text(&Cell::Float(f64::NEG_INFINITY)), "-inf");
    }

    #[test]
    fn prompt_values_and_json_params() {
        assert_eq!(parse_param("NULL", ParamKind::Auto), ParamValue::Value(Cell::Null));
        assert_eq!(parse_param("42", ParamKind::Auto), ParamValue::Value(Cell::Int(42)));
        assert_eq!(parse_param("-7", ParamKind::Auto), ParamValue::Value(Cell::Int(-7)));
        assert_eq!(parse_param("007", ParamKind::Auto), ParamValue::Value(Cell::Text("007".into())));
        assert_eq!(parse_param("1.50", ParamKind::Auto), ParamValue::Value(Cell::Text("1.50".into())));
        assert_eq!(parse_param("'it''s'", ParamKind::Auto), ParamValue::Value(Cell::Text("it's".into())));
        assert_eq!(parse_param("'a' || 'b'", ParamKind::Auto), ParamValue::Value(Cell::Text("'a' || 'b'".into())));
        assert_eq!(parse_param("true", ParamKind::Auto), ParamValue::Value(Cell::Bool(true)));
        assert_eq!(parse_param("NULL", ParamKind::Text), ParamValue::Value(Cell::Text("NULL".into())));
        assert_eq!(parse_param("now()", ParamKind::Sql), ParamValue::Sql("now()".into()));
        assert_eq!(
            parse_param("18446744073709551615", ParamKind::Auto),
            ParamValue::Value(Cell::UInt(u64::MAX))
        );

        let names = vec!["?1".to_string(), "?2".to_string()];
        let v = values_from_json(&names, &serde_json::json!(["x", 2]), false).unwrap().unwrap();
        assert_eq!(v.get("?2"), Some(&ParamValue::Value(Cell::Int(2))));
        assert!(values_from_json(&names, &serde_json::json!(["x"]), false).is_err());
        assert!(values_from_json(&names, &serde_json::json!(["x", 1, 2]), false).is_err());
        let names = vec![":a".to_string(), "$2".to_string(), ":b".to_string()];
        let v = values_from_json(&names, &serde_json::json!([1, 2, ]), false);
        assert!(v.is_err(), "{v:?}");
        let v = values_from_json(&[":a".into(), ":b".into()], &serde_json::json!([{"hex": "00ff"}, null]), false)
            .unwrap()
            .unwrap();
        assert_eq!(v.get(":a"), Some(&ParamValue::Value(Cell::Bytes(vec![0, 255]))));
        assert_eq!(v.get(":b"), Some(&ParamValue::Value(Cell::Null)));
        assert!(values_from_json(&[], &serde_json::json!({"a": {"sql": "1"}}), false).is_err());
        assert!(values_from_json(&[], &serde_json::json!({"a": {"sql": "1"}}), true).is_ok());
        assert!(values_from_json(&[], &serde_json::json!({"a": [1]}), true).is_err());
        assert!(values_from_json(&[], &serde_json::json!("x"), true).is_err());
        assert_eq!(values_from_json(&[], &serde_json::Value::Null, true).unwrap(), None);
        assert!(param_from_json(&serde_json::json!({"hex": "0"}), false).is_err());
    }
}
