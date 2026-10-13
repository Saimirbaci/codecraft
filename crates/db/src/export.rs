//! Result export and Copy As: CSV, TSV, JSON, SQL INSERT statements and Markdown tables.

use serde::{Deserialize, Serialize};

use crate::edit::literal;
use crate::sql::qualified;
use crate::{Cell, Column, DbKind, DbResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    /// `INSERT INTO … VALUES …;` per row.
    Sql,
    Markdown,
}

impl ExportFormat {
    pub const ALL: [ExportFormat; 5] =
        [ExportFormat::Csv, ExportFormat::Tsv, ExportFormat::Json, ExportFormat::Sql, ExportFormat::Markdown];

    pub fn parse(s: &str) -> Option<ExportFormat> {
        match s.trim().to_ascii_lowercase().as_str() {
            "csv" => Some(ExportFormat::Csv),
            "tsv" | "tab" => Some(ExportFormat::Tsv),
            "json" => Some(ExportFormat::Json),
            "sql" | "insert" | "sqlinsert" | "sql-insert" => Some(ExportFormat::Sql),
            "markdown" | "md" => Some(ExportFormat::Markdown),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::Sql => "SQL INSERT",
            ExportFormat::Markdown => "Markdown",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::Sql => "sql",
            ExportFormat::Markdown => "md",
        }
    }
}

/// Where SQL INSERT statements insert (the result's table, or a name the user chose).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InsertTarget {
    pub kind: DbKind,
    /// May be empty (unqualified).
    pub schema: String,
    pub table: String,
}

/// Format rows. `target` is used by [`ExportFormat::Sql`] (a missing one inserts into `my_table`).
pub fn export(
    format: ExportFormat,
    columns: &[Column],
    rows: &[Vec<Cell>],
    target: Option<&InsertTarget>,
) -> DbResult<String> {
    Ok(match format {
        ExportFormat::Csv => delimited(columns, rows, ','),
        ExportFormat::Tsv => delimited(columns, rows, '\t'),
        ExportFormat::Json => json(columns, rows),
        ExportFormat::Sql => inserts(columns, rows, target),
        ExportFormat::Markdown => markdown(columns, rows),
    })
}

fn delimited(columns: &[Column], rows: &[Vec<Cell>], sep: char) -> String {
    let field = |s: &str| -> String {
        if s.contains([sep, '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let sep_s = sep.to_string();
    let mut out = columns.iter().map(|c| field(&c.name)).collect::<Vec<_>>().join(&sep_s);
    out.push('\n');
    for row in rows {
        out.push_str(&row.iter().map(|c| field(&c.to_plain())).collect::<Vec<_>>().join(&sep_s));
        out.push('\n');
    }
    out
}

/// Distinct keys for the columns (`a`, `a_2` for a repeated `a`).
fn keys(columns: &[Column]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(columns.len());
    for c in columns {
        let mut k = c.name.clone();
        let mut n = 2;
        while out.contains(&k) {
            k = format!("{}_{n}", c.name);
            n += 1;
        }
        out.push(k);
    }
    out
}

fn json(columns: &[Column], rows: &[Vec<Cell>]) -> String {
    let keys = keys(columns);
    let objects: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            let mut m = serde_json::Map::new();
            for (k, cell) in keys.iter().zip(row) {
                m.insert(k.clone(), serde_json::to_value(cell).unwrap_or(serde_json::Value::Null));
            }
            serde_json::Value::Object(m)
        })
        .collect();
    let mut s = serde_json::to_string_pretty(&objects).unwrap_or_else(|_| "[]".into());
    s.push('\n');
    s
}

fn inserts(columns: &[Column], rows: &[Vec<Cell>], target: Option<&InsertTarget>) -> String {
    let fallback = InsertTarget { table: "my_table".into(), ..InsertTarget::default() };
    let t = target.unwrap_or(&fallback);
    let table = qualified(t.kind, &t.schema, &t.table);
    let cols = columns.iter().map(|c| crate::sql::quote_ident(t.kind, &c.name)).collect::<Vec<_>>().join(", ");
    let mut out = String::new();
    for row in rows {
        let values: Vec<String> = row
            .iter()
            .map(|c| {
                literal(t.kind, c)
                    .or_else(|_| literal(t.kind, &Cell::Text(c.to_plain())))
                    .unwrap_or_else(|_| "NULL".into())
            })
            .collect();
        out.push_str(&format!("INSERT INTO {table} ({cols}) VALUES ({});\n", values.join(", ")));
    }
    out
}

fn markdown(columns: &[Column], rows: &[Vec<Cell>]) -> String {
    let esc =
        |s: &str| s.replace('\\', "\\\\").replace('|', "\\|").replace("\r\n", "<br>").replace(['\n', '\r'], "<br>");
    let mut out = String::from("|");
    for c in columns {
        out.push_str(&format!(" {} |", esc(&c.name)));
    }
    out.push_str("\n|");
    for _ in columns {
        out.push_str(" --- |");
    }
    out.push('\n');
    for row in rows {
        out.push('|');
        for c in row {
            out.push_str(&format!(" {} |", esc(&c.to_plain())));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(n: &[&str]) -> Vec<Column> {
        n.iter().map(|n| Column { name: (*n).into(), type_name: String::new() }).collect()
    }

    #[test]
    fn every_format() {
        let c = cols(&["id", "name", "name"]);
        let rows = vec![
            vec![Cell::Int(1), Cell::Text("a|b".into()), Cell::Null],
            vec![Cell::Int(2), Cell::Text("x\ty \"q\"".into()), Cell::Bytes(vec![0xAB])],
        ];
        assert_eq!(
            export(ExportFormat::Csv, &c, &rows, None).unwrap(),
            "id,name,name\n1,a|b,\n2,\"x\ty \"\"q\"\"\",0xAB\n"
        );
        assert_eq!(
            export(ExportFormat::Tsv, &c, &rows, None).unwrap(),
            "id\tname\tname\n1\ta|b\t\n2\t\"x\ty \"\"q\"\"\"\t0xAB\n"
        );
        let j: serde_json::Value = serde_json::from_str(&export(ExportFormat::Json, &c, &rows, None).unwrap()).unwrap();
        assert_eq!(j[0]["name"], "a|b");
        assert_eq!(j[0]["name_2"], serde_json::Value::Null);
        assert_eq!(j[1]["id"], 2);
        let t = InsertTarget { kind: DbKind::Postgres, schema: "public".into(), table: "t".into() };
        assert_eq!(
            export(ExportFormat::Sql, &c, &rows[..1], Some(&t)).unwrap(),
            "INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"name\") VALUES (1, 'a|b', NULL);\n"
        );
        assert!(export(ExportFormat::Sql, &c, &rows[..1], None).unwrap().starts_with("INSERT INTO \"my_table\""));
        assert_eq!(
            export(ExportFormat::Markdown, &cols(&["a"]), &[vec![Cell::Text("x|y\nz".into())]], None).unwrap(),
            "| a |\n| --- |\n| x\\|y<br>z |\n"
        );
        assert_eq!(ExportFormat::parse("MD"), Some(ExportFormat::Markdown));
        assert_eq!(ExportFormat::parse("xml"), None);
    }
}
