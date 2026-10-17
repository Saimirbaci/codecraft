//! Import CSV/TSV/JSON files into a table: parsing, mapping file columns to table columns, and
//! inserting in batches inside one transaction (all rows or none).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::edit::{begin_statement, literal};
use crate::sql::{qualified, quote_ident};
use crate::{Cell, Connection, DbError, DbKind, DbResult, ResultSink};

/// Files larger than this are refused (they are read into memory).
pub const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;
/// Columns accepted from a file.
const MAX_COLUMNS: usize = 4096;
/// Rows per INSERT statement.
pub const BATCH_ROWS: usize = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImportFormat {
    #[default]
    Csv,
    Tsv,
    Json,
}

impl ImportFormat {
    pub fn parse(s: &str) -> Option<ImportFormat> {
        match s.trim().to_ascii_lowercase().as_str() {
            "csv" => Some(ImportFormat::Csv),
            "tsv" | "tab" => Some(ImportFormat::Tsv),
            "json" => Some(ImportFormat::Json),
            _ => None,
        }
    }

    /// From the file extension (`.json`, `.tsv`/`.tab`, anything else is CSV).
    pub fn for_path(path: &Path) -> ImportFormat {
        match path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).as_deref() {
            Some("json") => ImportFormat::Json,
            Some("tsv" | "tab") => ImportFormat::Tsv,
            _ => ImportFormat::Csv,
        }
    }
}

/// How to read a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImportOptions {
    pub format: ImportFormat,
    /// The first CSV/TSV line names the columns.
    pub header: bool,
    /// CSV field separator; `None` detects `,` `;` or tab from the first line.
    pub delimiter: Option<char>,
    /// Empty CSV fields become NULL (otherwise empty strings).
    pub empty_as_null: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self { format: ImportFormat::Csv, header: true, delimiter: None, empty_as_null: true }
    }
}

/// Parsed file contents.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportData {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

/// Read and parse a file (refusing very large ones).
pub fn read_file(path: &Path, options: &ImportOptions) -> DbResult<ImportData> {
    let meta = std::fs::metadata(path).map_err(|e| DbError::Io(format!("{}: {e}", path.display())))?;
    if meta.len() > MAX_IMPORT_BYTES {
        return Err(DbError::Io(format!(
            "{} is {} MB; files up to {} MB can be imported",
            path.display(),
            meta.len() / (1024 * 1024),
            MAX_IMPORT_BYTES / (1024 * 1024)
        )));
    }
    let bytes = std::fs::read(path).map_err(|e| DbError::Io(format!("{}: {e}", path.display())))?;
    let text = String::from_utf8_lossy(&bytes);
    parse(&text, options)
}

pub fn parse(text: &str, options: &ImportOptions) -> DbResult<ImportData> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    match options.format {
        ImportFormat::Json => parse_json(text),
        ImportFormat::Tsv => parse_delimited(text, options.delimiter.unwrap_or('\t'), options),
        ImportFormat::Csv => {
            let sep = options.delimiter.unwrap_or_else(|| sniff_delimiter(text));
            parse_delimited(text, sep, options)
        }
    }
}

/// `,` unless the first line has none but has `;` or tabs.
fn sniff_delimiter(text: &str) -> char {
    let first = text.lines().next().unwrap_or("");
    if first.contains(',') {
        ','
    } else if first.contains('\t') {
        '\t'
    } else if first.contains(';') {
        ';'
    } else {
        ','
    }
}

/// RFC 4180 records: quoted fields may hold separators, quotes (doubled) and newlines.
fn records(text: &str, sep: char) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    // The record had a quoted field (a line holding just `""` is a record, not a blank line).
    let mut had_quote = false;
    let mut at_start = true;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if at_start => {
                quoted = true;
                had_quote = true;
                at_start = false;
            }
            c if c == sep => {
                row.push(std::mem::take(&mut field));
                at_start = true;
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut field));
                if had_quote || !(row.len() == 1 && row.first().is_some_and(String::is_empty)) {
                    out.push(std::mem::take(&mut row));
                }
                row.clear();
                had_quote = false;
                at_start = true;
            }
            c => {
                field.push(c);
                at_start = false;
            }
        }
    }
    if !field.is_empty() || !row.is_empty() || quoted {
        row.push(field);
        out.push(row);
    }
    out
}

fn parse_delimited(text: &str, sep: char, options: &ImportOptions) -> DbResult<ImportData> {
    let mut recs = records(text, sep).into_iter();
    let width = |r: &Vec<String>| r.len();
    let (columns, rows): (Vec<String>, Vec<Vec<String>>) = if options.header {
        let header = recs.next().unwrap_or_default();
        (header.into_iter().map(|h| h.trim().to_string()).collect(), recs.collect())
    } else {
        let rows: Vec<Vec<String>> = recs.collect();
        let n = rows.iter().map(width).max().unwrap_or(0);
        ((1..=n).map(|i| format!("column{i}")).collect(), rows)
    };
    if columns.len() > MAX_COLUMNS {
        return Err(DbError::Io(format!(
            "the file has {} columns; at most {MAX_COLUMNS} are supported",
            columns.len()
        )));
    }
    let cell = |s: String| if s.is_empty() && options.empty_as_null { Cell::Null } else { Cell::Text(s) };
    let rows = rows.into_iter().map(|r| r.into_iter().take(MAX_COLUMNS).map(cell).collect()).collect();
    Ok(ImportData { columns, rows })
}

/// An array of objects (columns are the keys, in order of first appearance) or of arrays.
fn parse_json(text: &str) -> DbResult<ImportData> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| DbError::Io(format!("not valid JSON: {e}")))?;
    let items = match v {
        serde_json::Value::Array(a) => a,
        obj @ serde_json::Value::Object(_) => vec![obj],
        _ => return Err(DbError::Io("expected a JSON array of objects or arrays".into())),
    };
    let value = |v: &serde_json::Value| -> Cell {
        match v {
            serde_json::Value::Null => Cell::Null,
            serde_json::Value::String(s) => Cell::Text(s.clone()),
            serde_json::Value::Bool(b) => Cell::Bool(*b),
            serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
                (Some(i), _) => Cell::Int(i),
                (None, Some(f)) => Cell::Float(f),
                _ => Cell::Text(n.to_string()),
            },
            other => Cell::Text(other.to_string()),
        }
    };
    let mut columns: Vec<String> = Vec::new();
    for item in &items {
        match item {
            serde_json::Value::Object(m) => {
                for k in m.keys() {
                    if !columns.contains(k) && columns.len() < MAX_COLUMNS {
                        columns.push(k.clone());
                    }
                }
            }
            serde_json::Value::Array(a) => {
                while columns.len() < a.len().min(MAX_COLUMNS) {
                    columns.push(format!("column{}", columns.len() + 1));
                }
            }
            _ => return Err(DbError::Io("expected a JSON array of objects or arrays".into())),
        }
    }
    let rows = items
        .iter()
        .map(|item| match item {
            serde_json::Value::Object(m) => columns.iter().map(|k| m.get(k).map(value).unwrap_or(Cell::Null)).collect(),
            serde_json::Value::Array(a) => a.iter().take(MAX_COLUMNS).map(value).collect(),
            _ => Vec::new(),
        })
        .collect();
    Ok(ImportData { columns, rows })
}

/// For each table column, the file column to take its value from: matching names (ignoring
/// case and `_`/space differences) first; when no name matches at all, by position.
pub fn auto_mapping(file_columns: &[String], table_columns: &[String]) -> Vec<Option<usize>> {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let by_name: Vec<Option<usize>> =
        table_columns.iter().map(|t| file_columns.iter().position(|f| norm(f) == norm(t))).collect();
    if by_name.iter().any(Option::is_some) {
        return by_name;
    }
    (0..table_columns.len()).map(|i| (i < file_columns.len()).then_some(i)).collect()
}

/// Multi-row INSERT statements for the mapped columns (`mapping[i]` feeds table column `i`),
/// with literal values (scripts). [`insert_bound`] is what an import runs.
pub fn insert_statements(
    kind: DbKind,
    schema: &str,
    table: &str,
    table_columns: &[String],
    mapping: &[Option<usize>],
    data: &ImportData,
    batch: usize,
) -> DbResult<Vec<String>> {
    let pairs: Vec<(&String, usize)> =
        table_columns.iter().zip(mapping).filter_map(|(t, m)| m.map(|m| (t, m))).collect();
    if pairs.is_empty() {
        return Err(DbError::Config("map at least one file column to a table column".into()));
    }
    let target = qualified(kind, schema, table);
    let cols = pairs.iter().map(|(t, _)| quote_ident(kind, t)).collect::<Vec<_>>().join(", ");
    let mut out = Vec::new();
    for chunk in data.rows.chunks(batch.max(1)) {
        let mut values = Vec::with_capacity(chunk.len());
        for row in chunk {
            let mut v = Vec::with_capacity(pairs.len());
            for (_, src) in &pairs {
                v.push(literal(kind, row.get(*src).unwrap_or(&Cell::Null))?);
            }
            values.push(format!("({})", v.join(", ")));
        }
        out.push(format!("INSERT INTO {target} ({cols}) VALUES {}", values.join(", ")));
    }
    Ok(out)
}

struct Discard;

impl ResultSink for Discard {
    fn columns(&mut self, _: &[crate::Column]) {}
    fn page(&mut self, _: crate::Page) -> bool {
        true
    }
}

/// Multi-row INSERT statements whose values are bind parameters ([`crate::bind`]; literals
/// where the driver cannot bind). Batches shrink so a statement stays under the server's
/// parameter limit ([`crate::bind::max_params`]).
pub fn insert_bound(
    kind: DbKind,
    schema: &str,
    table: &str,
    table_columns: &[String],
    mapping: &[Option<usize>],
    data: &ImportData,
    batch: usize,
) -> DbResult<Vec<crate::bind::Bound>> {
    let pairs: Vec<(&String, usize)> =
        table_columns.iter().zip(mapping).filter_map(|(t, m)| m.map(|m| (t, m))).collect();
    if pairs.is_empty() {
        return Err(DbError::Config("map at least one file column to a table column".into()));
    }
    let target = qualified(kind, schema, table);
    let cols = pairs.iter().map(|(t, _)| quote_ident(kind, t)).collect::<Vec<_>>().join(", ");
    let per_statement = if crate::bind::binds_generated(kind) {
        (crate::bind::max_params(kind) / pairs.len().max(1)).max(1)
    } else {
        usize::MAX
    };
    let mut out = Vec::new();
    for chunk in data.rows.chunks(batch.max(1).min(per_statement)) {
        let mut b = crate::bind::Binder::new(kind);
        let mut values = Vec::with_capacity(chunk.len());
        for row in chunk {
            let mut v = Vec::with_capacity(pairs.len());
            for (_, src) in &pairs {
                v.push(b.value(row.get(*src).unwrap_or(&Cell::Null))?);
            }
            values.push(format!("({})", v.join(", ")));
        }
        out.push(b.finish(format!("INSERT INTO {target} ({cols}) VALUES {}", values.join(", "))));
    }
    Ok(out)
}

/// Run the statements in one transaction; `progress` gets the statements done so far. Any
/// failure rolls everything back. Returns the rows inserted.
pub fn run_import(conn: &mut dyn Connection, statements: &[String], progress: &mut dyn FnMut(usize)) -> DbResult<u64> {
    let bound: Vec<crate::bind::Bound> = statements.iter().map(crate::bind::Bound::plain).collect();
    run_import_bound(conn, &bound, progress)
}

/// [`run_import`] for statements with bind parameters ([`insert_bound`]).
pub fn run_import_bound(
    conn: &mut dyn Connection,
    statements: &[crate::bind::Bound],
    progress: &mut dyn FnMut(usize),
) -> DbResult<u64> {
    let kind = conn.kind();
    conn.execute(begin_statement(kind), 1, &mut Discard)?;
    let mut total = 0u64;
    for (i, st) in statements.iter().enumerate() {
        match conn.execute_bound(&st.sql, &st.params, 1, &mut Discard) {
            Ok(s) => total = total.saturating_add(s.rows_affected.unwrap_or(0)),
            Err(e) => {
                let _ = conn.execute("ROLLBACK", 1, &mut Discard);
                return Err(match e {
                    DbError::Query(m) => DbError::Query(format!(
                        "{m}\n  in batch {} of {}; nothing was imported",
                        i + 1,
                        statements.len()
                    )),
                    other => other,
                });
            }
        }
        progress(i + 1);
    }
    if let Err(e) = conn.execute("COMMIT", 1, &mut Discard) {
        let _ = conn.execute("ROLLBACK", 1, &mut Discard);
        return Err(e);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(format: ImportFormat) -> ImportOptions {
        ImportOptions { format, ..ImportOptions::default() }
    }

    #[test]
    fn csv_with_quotes_newlines_and_bom() {
        let text = "\u{feff}id,name,note\r\n1,\"Ann, the \"\"first\"\"\",\r\n2,\"multi\nline\",x\n\n3,Cy\n";
        let d = parse(text, &opts(ImportFormat::Csv)).unwrap();
        assert_eq!(d.columns, ["id", "name", "note"]);
        assert_eq!(d.rows.len(), 3);
        assert_eq!(d.rows[0], [Cell::Text("1".into()), Cell::Text("Ann, the \"first\"".into()), Cell::Null]);
        assert_eq!(d.rows[1][1], Cell::Text("multi\nline".into()));
        assert_eq!(d.rows[2].len(), 2);
        let semi = parse("a;b\n1;2\n", &opts(ImportFormat::Csv)).unwrap();
        assert_eq!(semi.columns, ["a", "b"]);
        let tsv = parse("x\n1\t2\n", &ImportOptions { header: false, ..opts(ImportFormat::Tsv) }).unwrap();
        assert_eq!(tsv.columns, ["column1", "column2"]);
        assert_eq!(tsv.rows.len(), 2);
        let keep = parse("a,b\n,1\n", &ImportOptions { empty_as_null: false, ..opts(ImportFormat::Csv) }).unwrap();
        assert_eq!(keep.rows[0][0], Cell::Text(String::new()));
        assert!(parse("", &opts(ImportFormat::Csv)).unwrap().rows.is_empty());
    }

    #[test]
    fn json_objects_and_arrays() {
        let d = parse(
            r#"[{"id": 1, "name": "a"}, {"name": "b", "extra": true, "n": 1.5, "o": {"k": 1}}]"#,
            &opts(ImportFormat::Json),
        )
        .unwrap();
        assert_eq!(d.columns, ["id", "name", "extra", "n", "o"]);
        assert_eq!(d.rows[0], [Cell::Int(1), Cell::Text("a".into()), Cell::Null, Cell::Null, Cell::Null]);
        assert_eq!(d.rows[1][2], Cell::Bool(true));
        assert_eq!(d.rows[1][4], Cell::Text("{\"k\":1}".into()));
        let a = parse("[[1, 2], [3]]", &opts(ImportFormat::Json)).unwrap();
        assert_eq!(a.columns, ["column1", "column2"]);
        assert!(parse("[1]", &opts(ImportFormat::Json)).is_err());
        assert!(parse("{", &opts(ImportFormat::Json)).is_err());
        assert!(parse("3", &opts(ImportFormat::Json)).is_err());
    }

    #[test]
    fn mapping_and_statements() {
        let file = vec!["Name".to_string(), "ID".to_string()];
        let table = vec!["id".to_string(), "name".to_string(), "created_at".to_string()];
        assert_eq!(auto_mapping(&file, &table), [Some(1), Some(0), None]);
        assert_eq!(auto_mapping(&["x".into()], &table), [Some(0), None, None]);
        let data = ImportData {
            columns: file,
            rows: vec![
                vec![Cell::Text("O'Neil".into()), Cell::Text("1".into())],
                vec![Cell::Text("B".into())],
                vec![Cell::Null, Cell::Text("3".into())],
            ],
        };
        let sql = insert_statements(DbKind::Sqlite, "main", "t", &table, &[Some(1), Some(0), None], &data, 2).unwrap();
        assert_eq!(
            sql,
            [
                "INSERT INTO \"main\".\"t\" (\"id\", \"name\") VALUES ('1', 'O''Neil'), (NULL, 'B')",
                "INSERT INTO \"main\".\"t\" (\"id\", \"name\") VALUES ('3', NULL)",
            ]
        );
        assert!(insert_statements(DbKind::Sqlite, "", "t", &table, &[None, None, None], &data, 2).is_err());
        assert_eq!(ImportFormat::for_path(Path::new("a/b.JSON")), ImportFormat::Json);
        assert_eq!(ImportFormat::for_path(Path::new("b.txt")), ImportFormat::Csv);
    }
}
