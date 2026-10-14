//! Schema tools as commands (track DA): Create/Modify Table, Drop/Rename/Truncate, SQL Generator
//! and Copy DDL, the ER diagram and Compare Schemas. Every dialog the UI shows is driven by these
//! commands, so agents and the CLI can do the same headlessly (`execute`/`confirm` params run at
//! once instead of waiting for a button).

use serde_json::{Value, json};

use codecraft_db::ddl::{ObjectKind, ObjectRef, TableDef};
use codecraft_workspace::ToolWindowId;

use super::{bad, ensure_connected, failed, target_source};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::database::schema_tools::{SchemaOp, SchemaSide, ToolState, open_in_console};
use crate::database::sync_project;
use crate::params::{bool_or, opt_str, req_path, req_str};
use crate::{CommandError, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    vec![
        // ---- Create / Modify Table
        spec(
            "db.createTable",
            "New Table…",
            None,
            "{dataSource?, schema?, definition?: {name, columns: [{name, dataType, nullable?, default?, autoIncrement?, \
             comment?}], primaryKey?: [col], indexes?: [{name?, columns, unique?}], foreignKeys?: [{name?, columns, \
             refSchema?, refTable, refColumns, onDelete?: noAction|restrict|cascade|setNull|setDefault, onUpdate?}], \
             checks?: [{name?, expression}], comment?}, execute?: bool, confirm?: bool} — opens the Create Table dialog \
             with its SQL preview; `execute` runs it (one transaction where the database allows; production data \
             sources need `confirm`)",
            Context::Global,
            always,
            create_table,
        ),
        spec(
            "db.modifyTable",
            "Modify Table…",
            None,
            "{dataSource?, schema?, table, definition?: like db.createTable (columns keep their data through \
             `original`: the current name; same-named columns are matched), execute?: bool, confirm?: bool} — reads \
             the table and opens the Modify Table dialog",
            Context::Global,
            always,
            modify_table,
        ),
        spec(
            "db.tableEditor.get",
            "Get Table Dialog",
            None,
            "the Create/Modify Table dialog: definition, state and the SQL preview",
            Context::Global,
            always,
            |s, _| {
                let e = s.db.schema.table_editor.as_ref();
                Ok(json!({
                    "editor": e,
                    "sql": e.map(|e| e.preview()),
                    "error": e.and_then(|e| e.script().err()),
                }))
            },
        ),
        spec(
            "db.tableEditor.update",
            "Update Table Definition",
            None,
            "{definition} — replaces what the dialog edits (the preview follows)",
            Context::Global,
            always,
            |s, p| {
                let def = definition(p)?.ok_or_else(|| bad("missing `definition`"))?;
                let e = s.db.schema.table_editor.as_mut().ok_or_else(|| disabled("no table dialog is open"))?;
                e.table = TableDef { schema: e.schema.clone(), ..def };
                if matches!(e.state, ToolState::Failed(_) | ToolState::Confirm) {
                    e.state = ToolState::Ready;
                }
                Ok(json!({ "sql": e.preview() }))
            },
        ),
        spec(
            "db.tableEditor.execute",
            "Execute Table Changes",
            None,
            "{confirm?: bool} — runs the dialog's SQL; on a production data source asks first unless `confirm`",
            Context::Global,
            always,
            |s, p| match s.db.execute_table_editor(bool_or(p, "confirm", false)).map_err(failed)? {
                Some(n) => {
                    s.status = format!("Executing {n} statement(s)…");
                    Ok(json!({ "statements": n }))
                }
                None => Ok(json!({ "confirm": true })),
            },
        ),
        spec(
            "db.tableEditor.openInConsole",
            "Open Table SQL in Console",
            None,
            "writes the dialog's SQL into a new console of the data source",
            Context::Global,
            always,
            |s, _| {
                let e = s.db.schema.table_editor.as_ref().ok_or_else(|| disabled("no table dialog is open"))?;
                let script = e.script().map_err(failed)?;
                let source = e.source.clone();
                let path = open_in_console(s, &source, &script.to_sql()).map_err(failed)?;
                s.db.schema.table_editor = None;
                Ok(json!({ "path": path }))
            },
        ),
        spec("db.tableEditor.close", "Close Table Dialog", None, "", Context::Global, always, |s, _| {
            s.db.schema.table_editor = None;
            ok()
        }),
        // ---- Drop / Rename / Truncate
        spec(
            "db.dropObject",
            "Drop…",
            None,
            "{dataSource?, kind: table|view|materializedView|column|index|foreignKey|schema, schema?, name, table? (of a \
             column, index or foreign key), cascade?: bool, confirm?: bool} — shows the SQL and asks; `confirm` drops at once",
            Context::Global,
            always,
            |s, p| schema_op(s, p, |p| Ok(SchemaOp::Drop { cascade: bool_or(p, "cascade", false) })),
        ),
        spec(
            "db.renameObject",
            "Rename…",
            None,
            "{dataSource?, kind, schema?, name, table?, newName?, updateSqlFiles?: bool (true: offer to rename a \
             column's uses in the project's .sql files), confirm?: bool}",
            Context::Global,
            always,
            |s, p| {
                schema_op(s, p, |p| {
                    Ok(SchemaOp::Rename {
                        new_name: opt_str(p, "newName").unwrap_or_default().to_string(),
                        update_sql_files: bool_or(p, "updateSqlFiles", true),
                    })
                })
            },
        ),
        spec(
            "db.truncateTable",
            "Truncate",
            None,
            "{dataSource?, schema?, table, cascade?: bool, confirm?: bool} — deletes every row",
            Context::Global,
            always,
            truncate,
        ),
        spec("db.schemaOp.get", "Get Schema Change Prompt", None, "", Context::Global, always, |s, _| {
            let p = s.db.schema.prompt.as_ref();
            Ok(json!({ "prompt": p, "title": p.map(|p| p.title()), "sql": p.map(|p| p.preview()) }))
        }),
        spec(
            "db.schemaOp.update",
            "Update Schema Change",
            None,
            "{newName?, cascade?: bool, updateSqlFiles?: bool}",
            Context::Global,
            always,
            |s, p| {
                let pr = s.db.schema.prompt.as_mut().ok_or_else(|| disabled("no schema change is pending"))?;
                match &mut pr.op {
                    SchemaOp::Drop { cascade } | SchemaOp::Truncate { cascade } => {
                        *cascade = bool_or(p, "cascade", *cascade);
                    }
                    SchemaOp::Rename { new_name, update_sql_files } => {
                        if let Some(n) = opt_str(p, "newName") {
                            *new_name = n.to_string();
                        }
                        *update_sql_files = bool_or(p, "updateSqlFiles", *update_sql_files);
                    }
                }
                Ok(json!({ "sql": pr.preview() }))
            },
        ),
        spec(
            "db.schemaOp.confirm",
            "Confirm Schema Change",
            None,
            "runs the pending Drop/Rename/Truncate",
            Context::Global,
            always,
            |s, _| {
                let n = s.db.confirm_schema_op().map_err(failed)?;
                s.status = "Executing…".into();
                Ok(json!({ "statements": n }))
            },
        ),
        spec("db.schemaOp.cancel", "Cancel Schema Change", None, "", Context::Global, always, |s, _| {
            if s.db.schema.prompt.as_ref().is_some_and(|p| p.state == ToolState::Running) {
                return Err(disabled("the change is running"));
            }
            s.db.schema.prompt = None;
            ok()
        }),
        // ---- SQL Generator / Copy DDL
        spec(
            "db.generateDdl",
            "SQL Generator…",
            None,
            "{dataSource?, objects?: [{kind, schema?, name, table?}] | kind, schema?, name, table?} — the DDL of tables \
             (with indexes, keys and comments), views, indexes, columns, foreign keys or whole schemas",
            Context::Global,
            always,
            |s, p| generate(s, p, false),
        ),
        spec(
            "db.copyDdl",
            "Copy DDL",
            None,
            "{…as db.generateDdl} — copies the DDL to the clipboard",
            Context::Global,
            always,
            |s, p| generate(s, p, true),
        ),
        spec("db.generateDdl.get", "Get Generated DDL", None, "", Context::Global, always, |s, _| {
            Ok(json!({ "generated": s.db.schema.generated }))
        }),
        spec(
            "db.generateDdl.openInConsole",
            "Open Generated DDL in Console",
            None,
            "",
            Context::Global,
            always,
            |s, _| {
                let g = s.db.schema.generated.as_ref().ok_or_else(|| disabled("no generated DDL"))?;
                if g.state != ToolState::Ready {
                    return Err(disabled("the DDL is not ready"));
                }
                let (source, sql) = (g.source.clone(), g.sql.clone());
                let path = open_in_console(s, &source, &sql).map_err(failed)?;
                Ok(json!({ "path": path }))
            },
        ),
        spec("db.generateDdl.close", "Close SQL Generator", None, "", Context::Global, always, |s, _| {
            s.db.schema.generated = None;
            ok()
        }),
        // ---- ER diagram
        spec(
            "db.diagram.show",
            "Show Visualization",
            None,
            "{dataSource?, schema?, tables?: [name]} — the ER diagram of a schema (or of the tables): columns with \
             PK/FK markers, foreign key edges, automatic layout",
            Context::Global,
            always,
            show_diagram,
        ),
        spec("db.diagram.get", "Get Diagram", None, "", Context::Global, always, |s, _| {
            Ok(json!({ "diagram": s.db.schema.diagram }))
        }),
        spec(
            "db.diagram.moveTable",
            "Move Diagram Table",
            None,
            "{table, x, y} — diagram coordinates of the table's top-left corner",
            Context::Global,
            always,
            |s, p| {
                let table = req_str(p, "table")?;
                let (x, y) = (num(p, "x")?, num(p, "y")?);
                let d = diagram_mut(s)?;
                let n =
                    d.nodes.iter_mut().find(|n| n.name == table).ok_or_else(|| bad(format!("no table `{table}`")))?;
                n.x = x;
                n.y = y;
                ok()
            },
        ),
        spec(
            "db.diagram.select",
            "Select Diagram Table",
            None,
            "{table?} — highlights it and the tables it references or is referenced by",
            Context::Global,
            always,
            |s, p| {
                let d = diagram_mut(s)?;
                let table = opt_str(p, "table").filter(|t| !t.is_empty());
                if let Some(t) = table
                    && d.node_index(t).is_none()
                {
                    return Err(bad(format!("no table `{t}`")));
                }
                d.selected = table.map(str::to_string);
                let related = table.map(|t| d.related(t)).unwrap_or_default();
                Ok(json!({ "selected": d.selected, "related": related }))
            },
        ),
        spec(
            "db.diagram.setView",
            "Zoom Diagram",
            None,
            "{zoom?: 0.1–4, pan?: [x, y]}",
            Context::Global,
            always,
            |s, p| {
                let zoom = p.get("zoom").and_then(Value::as_f64);
                let pan = p.get("pan").and_then(Value::as_array).map(|a| {
                    let v: Vec<f32> = a.iter().filter_map(Value::as_f64).map(|f| f as f32).collect();
                    v
                });
                let d = diagram_mut(s)?;
                if let Some(z) = zoom.filter(|z| z.is_finite()) {
                    d.zoom = (z as f32).clamp(0.1, 4.0);
                }
                if let Some([x, y]) = pan.as_deref()
                    && x.is_finite()
                    && y.is_finite()
                {
                    d.pan = [*x, *y];
                }
                Ok(json!({ "zoom": d.zoom, "pan": d.pan }))
            },
        ),
        spec(
            "db.diagram.fit",
            "Fit Diagram",
            None,
            "{width?, height?} — zoom to show every table in a view of that size (the UI fits its own view)",
            Context::Global,
            always,
            |s, p| {
                let w = p.get("width").and_then(Value::as_f64).unwrap_or(1200.0) as f32;
                let h = p.get("height").and_then(Value::as_f64).unwrap_or(800.0) as f32;
                let d = diagram_mut(s)?;
                if w.is_finite() && h.is_finite() && w > 10.0 && h > 10.0 {
                    d.fit(w, h);
                } else {
                    d.fit_pending = true;
                }
                Ok(json!({ "zoom": d.zoom, "pan": d.pan }))
            },
        ),
        spec(
            "db.diagram.layout",
            "Layout Diagram",
            None,
            "lays the tables out again",
            Context::Global,
            always,
            |s, _| {
                let d = diagram_mut(s)?;
                crate::database::schema_tools::layout(&mut d.nodes, &d.edges);
                d.fit_pending = true;
                ok()
            },
        ),
        spec(
            "db.diagram.openTable",
            "Open Diagram Table",
            None,
            "{table} — opens the table's data",
            Context::Global,
            always,
            |s, p| {
                let table = req_str(p, "table")?.to_string();
                let d = s.db.schema.diagram.as_ref().ok_or_else(|| disabled("no diagram is open"))?;
                if d.node_index(&table).is_none() {
                    return Err(bad(format!("no table `{table}`")));
                }
                let params = json!({ "dataSource": d.source, "schema": d.schema, "table": table });
                s.execute("db.openTable", &params)
            },
        ),
        spec(
            "db.diagram.export",
            "Export Diagram",
            None,
            "{path, format?: svg|png (from the extension)} — SVG is written at once; PNG is drawn by the window",
            Context::Global,
            always,
            export_diagram,
        ),
        spec("db.diagram.close", "Close Diagram", None, "", Context::Global, always, |s, _| {
            s.db.schema.diagram = None;
            ok()
        }),
        // ---- Compare
        spec(
            "db.compareSchemas",
            "Compare Schemas…",
            None,
            "{source: {dataSource, schema?}, target: {dataSource, schema?}} — what differs (tables, columns, keys, \
             indexes, foreign keys, checks, views) and the migration script that makes the target like the source",
            Context::Global,
            always,
            compare,
        ),
        spec("db.compare.get", "Get Comparison", None, "", Context::Global, always, |s, _| {
            Ok(json!({ "compare": s.db.schema.compare }))
        }),
        spec(
            "db.compare.openScript",
            "Open Migration in Console",
            None,
            "writes the migration into a new console of the target data source to review and run",
            Context::Global,
            always,
            |s, _| {
                let c = s.db.schema.compare.as_ref().ok_or_else(|| disabled("no comparison is open"))?;
                if c.state.is_busy() {
                    return Err(disabled("the comparison is still running"));
                }
                let (target, sql) = (c.target.data_source.clone(), c.sql.clone());
                let path = open_in_console(s, &target, &sql).map_err(failed)?;
                Ok(json!({ "path": path }))
            },
        ),
        spec(
            "db.compare.apply",
            "Apply Migration",
            None,
            "{confirm?: bool} — runs the migration on the target (one transaction where the database allows; \
             production targets need `confirm`)",
            Context::Global,
            always,
            |s, p| match s.db.apply_compare(bool_or(p, "confirm", false)).map_err(failed)? {
                Some(n) => Ok(json!({ "statements": n })),
                None => Ok(json!({ "confirm": true })),
            },
        ),
        spec("db.compare.refresh", "Compare Again", None, "", Context::Global, always, |s, _| {
            let c = s.db.schema.compare.as_ref().ok_or_else(|| disabled("no comparison is open"))?;
            let (a, b) = (c.source.clone(), c.target.clone());
            s.db.compare_schemas(a, b).map_err(failed)?;
            ok()
        }),
        spec("db.compare.close", "Close Comparison", None, "", Context::Global, always, |s, _| {
            s.db.schema.compare = None;
            ok()
        }),
    ]
}

fn disabled(msg: &str) -> CommandError {
    CommandError::Disabled(msg.into())
}

fn num(p: &Value, key: &str) -> Result<f32, CommandError> {
    let v = p.get(key).and_then(Value::as_f64).ok_or_else(|| bad(format!("missing number `{key}`")))?;
    if !v.is_finite() || v.abs() > 1e7 {
        return Err(bad(format!("`{key}` is out of range")));
    }
    Ok(v as f32)
}

fn diagram_mut(s: &mut Session) -> Result<&mut crate::database::schema_tools::Diagram, CommandError> {
    s.db.schema.diagram.as_mut().ok_or_else(|| disabled("no diagram is open"))
}

fn definition(p: &Value) -> Result<Option<TableDef>, CommandError> {
    match p.get("definition") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => serde_json::from_value(v.clone()).map(Some).map_err(|e| bad(format!("`definition`: {e}"))),
    }
}

/// `(schema, table)` of the selected tree node, when it belongs to `source`.
fn selected_node(s: &Session, source: &str) -> (Option<String>, Option<String>) {
    let Some(sel) = &s.db.selected else { return (None, None) };
    let mut parts = sel.split('\u{1f}');
    if parts.next() != Some(source) {
        return (None, None);
    }
    (parts.next().map(str::to_string), parts.next().map(str::to_string))
}

fn schema_param(s: &Session, p: &Value, source: &str) -> String {
    opt_str(p, "schema")
        .map(str::to_string)
        .or_else(|| selected_node(s, source).0)
        .unwrap_or_else(|| s.db.default_schema(source))
}

fn create_table(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let schema = schema_param(s, p, &source);
    let def = definition(p)?;
    s.db.open_create_table(&source, &schema, def).map_err(failed)?;
    if bool_or(p, "execute", false) {
        return match s.db.execute_table_editor(bool_or(p, "confirm", false)).map_err(failed)? {
            Some(n) => Ok(json!({ "statements": n, "dataSource": source, "schema": schema })),
            None => Ok(json!({ "confirm": true })),
        };
    }
    let sql = s.db.schema.table_editor.as_ref().map(|e| e.preview());
    Ok(json!({ "dialog": true, "dataSource": source, "schema": schema, "sql": sql }))
}

fn modify_table(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let schema = schema_param(s, p, &source);
    let table = match opt_str(p, "table") {
        Some(t) => t.to_string(),
        None => selected_node(s, &source).1.ok_or_else(|| bad("missing `table`"))?,
    };
    let then = definition(p)?.map(|d| (d, bool_or(p, "execute", false).then(|| bool_or(p, "confirm", false))));
    s.db.open_modify_table(&source, &schema, &table, then).map_err(failed)?;
    s.status = format!("Reading {table}…");
    Ok(json!({ "dialog": true, "dataSource": source, "schema": schema, "table": table }))
}

/// The object a Drop/Rename/Copy DDL acts on.
fn object(s: &Session, p: &Value, source: &str) -> Result<ObjectRef, CommandError> {
    let kind_s = req_str(p, "kind")?;
    let kind = ObjectKind::parse(kind_s).ok_or_else(|| bad(format!("unknown kind `{kind_s}`")))?;
    let name = req_str(p, "name")?.to_string();
    let schema = if kind == ObjectKind::Schema {
        opt_str(p, "schema").map(str::to_string).unwrap_or_else(|| name.clone())
    } else {
        schema_param(s, p, source)
    };
    let table = opt_str(p, "table").unwrap_or_default().to_string();
    if kind.in_table() && table.is_empty() {
        return Err(bad(format!("a {} needs `table`", kind.label())));
    }
    Ok(ObjectRef { kind, schema, name, table })
}

fn schema_op(s: &mut Session, p: &Value, op: impl Fn(&Value) -> Result<SchemaOp, CommandError>) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let obj = object(s, p, &source)?;
    s.db.open_schema_op(&source, obj, op(p)?).map_err(failed)?;
    finish_op(s, p)
}

fn truncate(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let schema = schema_param(s, p, &source);
    let table = match opt_str(p, "table") {
        Some(t) => t.to_string(),
        None => selected_node(s, &source).1.ok_or_else(|| bad("missing `table`"))?,
    };
    let obj = ObjectRef { kind: ObjectKind::Table, schema, name: table, table: String::new() };
    s.db.open_schema_op(&source, obj, SchemaOp::Truncate { cascade: bool_or(p, "cascade", false) }).map_err(failed)?;
    finish_op(s, p)
}

/// Confirm at once with `confirm`, else leave the prompt open and return its SQL.
fn finish_op(s: &mut Session, p: &Value) -> CmdResult {
    let Some(pr) = s.db.schema.prompt.as_ref() else { return ok() };
    let (title, sql) = (pr.title(), pr.preview());
    if bool_or(p, "confirm", false) {
        let n = s.db.confirm_schema_op().map_err(|e| {
            s.db.schema.prompt = None;
            failed(e)
        })?;
        s.status = format!("{title}…");
        return Ok(json!({ "statements": n, "sql": sql }));
    }
    s.status = format!("{title}: confirm");
    Ok(json!({ "prompt": true, "title": title, "sql": sql }))
}

fn generate(s: &mut Session, p: &Value, copy: bool) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let objects: Vec<ObjectRef> = match p.get("objects") {
        Some(Value::Array(list)) => {
            let mut out = Vec::new();
            for o in list {
                out.push(object(s, o, &source)?);
            }
            out
        }
        Some(_) => return Err(bad("`objects` must be an array")),
        None if opt_str(p, "kind").is_some() => vec![object(s, p, &source)?],
        None => match selected_node(s, &source) {
            (Some(schema), Some(table)) => {
                vec![ObjectRef { kind: ObjectKind::Table, schema, name: table, table: String::new() }]
            }
            (Some(schema), None) => {
                vec![ObjectRef { kind: ObjectKind::Schema, schema: schema.clone(), name: schema, table: String::new() }]
            }
            _ => return Err(bad("missing `objects` (or kind and name)")),
        },
    };
    let n = objects.len();
    s.db.generate_ddl(&source, objects, copy).map_err(failed)?;
    s.status = "Generating DDL…".into();
    Ok(json!({ "objects": n }))
}

fn show_diagram(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let schema = schema_param(s, p, &source);
    let tables: Vec<String> = match p.get("tables") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Some(_) => return Err(bad("`tables` must be an array of names")),
    };
    s.db.show_diagram(&source, &schema, tables).map_err(failed)?;
    let _ = ensure_connected(s, &source);
    s.status = format!("Reading {schema}…");
    Ok(json!({ "dataSource": source, "schema": schema }))
}

fn export_diagram(s: &mut Session, p: &Value) -> CmdResult {
    let path = req_path(p, "path")?;
    let path = if path.is_relative() { s.db.project_root().map(|r| r.join(&path)).unwrap_or(path) } else { path };
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let format = opt_str(p, "format").map(str::to_ascii_lowercase).unwrap_or(if ext == "png" {
        "png".into()
    } else {
        "svg".into()
    });
    let d = s.db.schema.diagram.as_ref().ok_or_else(|| disabled("no diagram is open"))?;
    if d.state != ToolState::Ready {
        return Err(disabled("the diagram is not ready"));
    }
    match format.as_str() {
        "svg" => {
            codecraft_fs::write_text_file_atomic(&path, &d.to_svg())
                .map_err(|e| failed(format!("{}: {e}", path.display())))?;
            s.status = format!("Diagram saved to {}", path.display());
            Ok(json!({ "path": path }))
        }
        "png" => {
            s.db.schema.png_request = Some(path.clone());
            s.status = format!("Saving the diagram to {}…", path.display());
            Ok(json!({ "path": path, "pending": true }))
        }
        other => Err(bad(format!("unknown format `{other}` (svg, png)"))),
    }
}

fn side(s: &Session, p: &Value, key: &str) -> Result<SchemaSide, CommandError> {
    let v = p.get(key).ok_or_else(|| bad(format!("missing `{key}`: {{dataSource, schema}}")))?;
    let id = req_str(v, "dataSource")?;
    let ds = s.db.source(id).ok_or_else(|| bad(format!("no data source `{id}`")))?;
    let schema = opt_str(v, "schema").map(str::to_string).unwrap_or_else(|| s.db.default_schema(id));
    Ok(SchemaSide { data_source: id.to_string(), label: format!("{} / {schema}", ds.config.name), schema })
}

fn compare(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let a = side(s, p, "source")?;
    let b = side(s, p, "target")?;
    if a == b {
        return Err(bad("compare two different schemas"));
    }
    s.db.compare_schemas(a.clone(), b.clone()).map_err(failed)?;
    s.ws.layout.show(ToolWindowId::Database);
    s.status = format!("Comparing {} with {}…", a.label, b.label);
    Ok(json!({ "source": a, "target": b }))
}
