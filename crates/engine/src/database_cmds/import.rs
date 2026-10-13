//! Import Data from File: open the dialog for a table, adjust the column mapping, run.

use serde_json::{Value, json};

use codecraft_db::import::{ImportFormat, ImportOptions};

use super::{bad, failed, target_source};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::database::{ImportState, sync_project};
use crate::params::{bool_or, opt_str, req_path, req_str};
use crate::{CommandError, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "db.importData",
            "Import Data from File…",
            None,
            "{dataSource?, schema, table, path, format?: csv|tsv|json (from the extension), header?: bool (true), \
             delimiter?: char, emptyAsNull?: bool (true), run?: bool} — reads the file and the table's columns for the \
             mapping preview; `run` imports as soon as it is read",
            Context::Global,
            always,
            import_data,
        ),
        spec(
            "db.setImportMapping",
            "Set Import Column Mapping",
            None,
            "{mapping: [fileColumn index | null per table column] | {tableColumn: fileColumn index | name | null}}",
            Context::Global,
            always,
            set_mapping,
        ),
        spec(
            "db.runImport",
            "Import",
            None,
            "inserts every row in one transaction (batches of 100), off the UI thread",
            Context::Global,
            always,
            run_import,
        ),
        spec("db.getImport", "Get Import Status", None, "", Context::Global, always, |s, _| {
            Ok(json!({ "import": s.db.import }))
        }),
        spec("db.closeImport", "Close Import Dialog", None, "", Context::Global, always, |s, _| {
            s.db.import = None;
            ok()
        }),
    ]
}

fn import_data(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let schema = req_str(p, "schema")?.to_string();
    let table = req_str(p, "table")?.to_string();
    let mut path = req_path(p, "path")?;
    if path.is_relative()
        && let Some(root) = s.db.project_root()
    {
        path = root.join(path);
    }
    if !path.is_file() {
        return Err(bad(format!("{} is not a file", path.display())));
    }
    if s.db.import.as_ref().is_some_and(|d| matches!(d.state, ImportState::Importing { .. })) {
        return Err(CommandError::Disabled("an import is running".into()));
    }
    let format = match opt_str(p, "format") {
        Some(f) => ImportFormat::parse(f).ok_or_else(|| bad(format!("unknown format `{f}` (csv, tsv, json)")))?,
        None => ImportFormat::for_path(&path),
    };
    let delimiter = match opt_str(p, "delimiter") {
        None => None,
        Some("\\t" | "tab") => Some('\t'),
        Some(d) => {
            let mut chars = d.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c != '"' && c != '\n' && c != '\r' => Some(c),
                _ => return Err(bad("`delimiter` must be one character")),
            }
        }
    };
    let options = ImportOptions {
        format,
        header: bool_or(p, "header", true),
        delimiter,
        empty_as_null: bool_or(p, "emptyAsNull", true),
    };
    s.db.start_import(&source, &schema, &table, path.clone(), options, bool_or(p, "run", false)).map_err(failed)?;
    s.status = format!("Reading {}…", path.display());
    Ok(json!({ "dataSource": source, "path": path }))
}

fn set_mapping(s: &mut Session, p: &Value) -> CmdResult {
    let d = s.db.import.as_mut().ok_or_else(|| CommandError::Disabled("no import dialog".into()))?;
    if d.table_columns.is_empty() {
        return Err(CommandError::Disabled("the file is not read yet".into()));
    }
    let file_cols = d.file_columns.len();
    let source_of = |v: &Value| -> Result<Option<usize>, CommandError> {
        match v {
            Value::Null => Ok(None),
            Value::Number(n) => n
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n < file_cols)
                .map(Some)
                .ok_or_else(|| bad(format!("no file column {n}"))),
            Value::String(name) => d
                .file_columns
                .iter()
                .position(|c| c == name)
                .map(Some)
                .ok_or_else(|| bad(format!("no file column `{name}`"))),
            _ => Err(bad("a mapping value is a file column index, name or null")),
        }
    };
    let mut mapping = d.mapping.clone();
    mapping.resize(d.table_columns.len(), None);
    match p.get("mapping") {
        Some(Value::Array(a)) => {
            if a.len() != d.table_columns.len() {
                return Err(bad(format!("`mapping` needs {} entries (one per table column)", d.table_columns.len())));
            }
            for (slot, v) in mapping.iter_mut().zip(a) {
                *slot = source_of(v)?;
            }
        }
        Some(Value::Object(m)) => {
            for (col, v) in m {
                let i = d
                    .table_columns
                    .iter()
                    .position(|c| c == col)
                    .ok_or_else(|| bad(format!("the table has no column `{col}`")))?;
                if let Some(slot) = mapping.get_mut(i) {
                    *slot = source_of(v)?;
                }
            }
        }
        _ => return Err(bad("missing `mapping` (array or object)")),
    }
    d.mapping = mapping;
    if matches!(d.state, ImportState::Failed(_) | ImportState::Done(_)) {
        d.state = ImportState::Ready;
    }
    Ok(json!({ "mapping": d.mapping }))
}

fn run_import(s: &mut Session, _: &Value) -> CmdResult {
    let batches = s.db.run_import().map_err(failed)?;
    s.status = "Importing…".into();
    Ok(json!({ "batches": batches }))
}
