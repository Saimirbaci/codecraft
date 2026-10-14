//! Import Data from File: read a CSV/TSV/JSON file and the target table's columns (preview and
//! column mapping), then insert every row in one transaction. Both steps run on their own
//! threads with their own connection and report through the database event channel.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;

use codecraft_db::import::{BATCH_ROWS, ImportOptions, auto_mapping, insert_statements, read_file, run_import};
use codecraft_db::{Cell, DataSourceConfig, SecretStore};
use codecraft_workspace::NotificationLevel;

use super::{DbState, Event, MetaReq, OutputLevel};
use crate::Session;

/// File rows shown in the preview.
pub const PREVIEW_ROWS: usize = 20;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", content = "detail", rename_all = "camelCase")]
pub enum ImportState {
    /// Reading the file and the table's columns.
    Loading,
    Ready,
    /// Batches done of the total.
    Importing {
        done: usize,
        total: usize,
    },
    /// Rows imported.
    Done(u64),
    Failed(String),
}

/// The Import Data dialog. The UI edits `mapping` through `db.setImportMapping`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportDialog {
    pub source: String,
    pub schema: String,
    pub table: String,
    pub path: PathBuf,
    pub options: ImportOptions,
    pub file_columns: Vec<String>,
    /// The first rows of the file.
    pub preview: Vec<Vec<Cell>>,
    pub total_rows: usize,
    pub table_columns: Vec<String>,
    /// For each table column, the file column that feeds it (`None`: left to its default).
    pub mapping: Vec<Option<usize>>,
    pub state: ImportState,
    /// Start importing as soon as the file is read.
    #[serde(skip)]
    pub(crate) run_when_ready: bool,
    #[serde(skip)]
    nonce: u64,
}

impl ImportDialog {
    pub fn is_busy(&self) -> bool {
        matches!(self.state, ImportState::Loading | ImportState::Importing { .. })
    }
}

pub(crate) struct Loaded {
    file_columns: Vec<String>,
    preview: Vec<Vec<Cell>>,
    total_rows: usize,
    table_columns: Vec<String>,
}

pub(crate) enum ImportEvent {
    Loaded { nonce: u64, result: Result<Loaded, String> },
    Progress { nonce: u64, done: usize, total: usize },
    Finished { nonce: u64, source: String, table: String, result: Result<u64, String> },
}

/// Connect for an import thread (password: this session's, else the keychain), through the
/// data source's SSH tunnel when it has one.
fn connect(
    cfg: DataSourceConfig,
    tunnel: Option<crate::ssh_tunnel::TunnelPlan>,
    password: Option<String>,
    secrets: &Arc<dyn SecretStore>,
) -> Result<Box<dyn codecraft_db::Connection>, String> {
    let cfg = crate::ssh_tunnel::route(tunnel, cfg)?;
    let pw = match password {
        Some(p) => Some(p),
        None if cfg.password_saved => secrets.get(&cfg.id).map_err(|e| e.to_string())?,
        None => None,
    };
    codecraft_db::connect(&cfg, pw.as_deref()).map_err(|e| e.to_string())
}

impl DbState {
    /// Open the dialog for `schema.table` and read `path` in the background.
    pub(crate) fn start_import(
        &mut self,
        source: &str,
        schema: &str,
        table: &str,
        path: PathBuf,
        options: ImportOptions,
        run_when_ready: bool,
    ) -> Result<(), String> {
        let ds = self.source(source).ok_or_else(|| format!("no data source `{source}`"))?;
        if ds.config.read_only {
            return Err(format!("{} is read-only", ds.config.name));
        }
        let cfg = self.resolved(&ds.config);
        let nonce = self.next();
        let key = format!("import:{nonce}");
        let tunnel = self.plan_tunnel(&cfg)?.map(|t| t.keyed(key.clone()));
        let tunnels = self.tunnels.clone();
        let password = self.worker_password(source);
        let secrets = self.secrets.clone();
        let events = self.tx.clone();
        let (schema_s, table_s, path_c, opts) = (schema.to_string(), table.to_string(), path.clone(), options.clone());
        std::thread::Builder::new()
            .name("db-import-read".into())
            .spawn(move || {
                let result = (|| {
                    let data = read_file(&path_c, &opts).map_err(|e| e.to_string())?;
                    let mut conn = connect(cfg, tunnel, password, &secrets)?;
                    let details = conn.table_details(&schema_s, &table_s).map_err(|e| e.to_string())?;
                    if details.columns.is_empty() {
                        return Err(format!("no table {schema_s}.{table_s}"));
                    }
                    Ok(Loaded {
                        total_rows: data.rows.len(),
                        preview: data.rows.into_iter().take(PREVIEW_ROWS).collect(),
                        file_columns: data.columns,
                        table_columns: details.columns.into_iter().map(|c| c.name).collect(),
                    })
                })();
                tunnels.close(Some(&key));
                let _ = events.send(Event::Import(ImportEvent::Loaded { nonce, result }));
            })
            .map_err(|e| format!("cannot start the import: {e}"))?;
        self.import = Some(ImportDialog {
            source: source.to_string(),
            schema: schema.to_string(),
            table: table.to_string(),
            path,
            options,
            file_columns: Vec::new(),
            preview: Vec::new(),
            total_rows: 0,
            table_columns: Vec::new(),
            mapping: Vec::new(),
            state: ImportState::Loading,
            run_when_ready,
            nonce,
        });
        Ok(())
    }

    /// Insert the file's rows with the dialog's mapping (in the background, one transaction).
    pub(crate) fn run_import(&mut self) -> Result<usize, String> {
        let d = self.import.as_ref().ok_or("no import in progress")?;
        if d.state != ImportState::Ready && !matches!(d.state, ImportState::Failed(_)) || d.table_columns.is_empty() {
            return Err("the file is not read yet".into());
        }
        if d.mapping.iter().all(Option::is_none) {
            return Err("map at least one file column to a table column".into());
        }
        let ds = self.source(&d.source).ok_or_else(|| format!("no data source `{}`", d.source))?;
        if ds.config.read_only {
            return Err(format!("{} is read-only", ds.config.name));
        }
        let cfg = self.resolved(&ds.config);
        let key = format!("import-run:{}", d.nonce);
        let tunnel = self.plan_tunnel(&cfg)?.map(|t| t.keyed(key.clone()));
        let tunnels = self.tunnels.clone();
        let password = self.worker_password(&d.source);
        let secrets = self.secrets.clone();
        let events = self.tx.clone();
        let nonce = d.nonce;
        let (source, schema, table) = (d.source.clone(), d.schema.clone(), d.table.clone());
        let (path, options, columns, mapping) =
            (d.path.clone(), d.options.clone(), d.table_columns.clone(), d.mapping.clone());
        let batches = d.total_rows.div_ceil(BATCH_ROWS);
        let log = format!("Importing {} rows from {} into {schema}.{table}", d.total_rows, path.display());
        std::thread::Builder::new()
            .name("db-import".into())
            .spawn(move || {
                let result = (|| {
                    let data = read_file(&path, &options).map_err(|e| e.to_string())?;
                    let statements =
                        insert_statements(cfg.kind, &schema, &table, &columns, &mapping, &data, BATCH_ROWS)
                            .map_err(|e| e.to_string())?;
                    let total = statements.len();
                    let mut conn = connect(cfg, tunnel, password, &secrets)?;
                    let progress_events = events.clone();
                    run_import(conn.as_mut(), &statements, &mut |done| {
                        let _ = progress_events.send(Event::Import(ImportEvent::Progress { nonce, done, total }));
                    })
                    .map_err(|e| e.to_string())
                })();
                tunnels.close(Some(&key));
                let _ = events.send(Event::Import(ImportEvent::Finished { nonce, source, table, result }));
            })
            .map_err(|e| format!("cannot start the import: {e}"))?;
        let source = self.import.as_ref().map(|d| d.source.clone()).unwrap_or_default();
        self.log(OutputLevel::Info, &source, log);
        if let Some(d) = self.import.as_mut() {
            d.state = ImportState::Importing { done: 0, total: batches };
        }
        Ok(batches)
    }
}

pub(crate) fn on_event(s: &mut Session, ev: ImportEvent) {
    match ev {
        ImportEvent::Loaded { nonce, result } => {
            let Some(d) = s.db.import.as_mut().filter(|d| d.nonce == nonce) else { return };
            match result {
                Ok(l) => {
                    d.mapping = auto_mapping(&l.file_columns, &l.table_columns);
                    d.file_columns = l.file_columns;
                    d.preview = l.preview;
                    d.total_rows = l.total_rows;
                    d.table_columns = l.table_columns;
                    d.state = ImportState::Ready;
                    s.status = format!("{} rows read from {}", d.total_rows, d.path.display());
                    if d.run_when_ready {
                        d.run_when_ready = false;
                        if let Err(e) = s.db.run_import() {
                            if let Some(d) = s.db.import.as_mut() {
                                d.state = ImportState::Failed(e.clone());
                            }
                            s.status = e;
                        }
                    }
                }
                Err(e) => {
                    d.state = ImportState::Failed(e.clone());
                    s.status = format!("Cannot import: {e}");
                }
            }
        }
        ImportEvent::Progress { nonce, done, total } => {
            if let Some(d) = s.db.import.as_mut().filter(|d| d.nonce == nonce) {
                d.state = ImportState::Importing { done, total };
            }
        }
        ImportEvent::Finished { nonce, source, table, result } => {
            let msg = match &result {
                Ok(n) => format!("Imported {n} row{} into {table}", if *n == 1 { "" } else { "s" }),
                Err(e) => format!("Import into {table} failed: {e}"),
            };
            let schema = s.db.import.as_ref().filter(|d| d.nonce == nonce).map(|d| d.schema.clone());
            if let Some(d) = s.db.import.as_mut().filter(|d| d.nonce == nonce) {
                d.state = match &result {
                    Ok(n) => ImportState::Done(*n),
                    Err(e) => ImportState::Failed(e.clone()),
                };
            }
            match result {
                Ok(_) => {
                    s.db.log(OutputLevel::Info, &source, msg.clone());
                    // Refresh the table's row count if the tree shows it.
                    if let Some(schema) = schema
                        && s.db.source(&source).and_then(|d| d.table(&schema, &table)).is_some()
                    {
                        let _ = s.db.meta(&source, MetaReq::Count(schema, table));
                    }
                    s.notify(NotificationLevel::Info, &msg, String::new());
                }
                Err(e) => {
                    s.db.log(OutputLevel::Error, &source, msg.clone());
                    s.notify(NotificationLevel::Error, "Import failed", e);
                }
            }
            s.status = msg;
        }
    }
}
