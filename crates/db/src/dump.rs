//! Dump and Restore with the databases' own tools (`pg_dump`, `pg_restore`, `psql`,
//! `mysqldump`, `mysql`), SQLite dumps written and restored natively (like `sqlite3 .dump`),
//! and full-table export to a file streamed page by page ([`StreamWriter`]).
//!
//! Passwords reach the tools through the environment (`PGPASSWORD`, `MYSQL_PWD`), never the
//! command line. Output goes to `<file>.partial` first and is renamed when the tool succeeds,
//! so a failed or cancelled dump never looks like a complete one.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::edit::literal;
use crate::export::{ExportFormat, InsertTarget};
use crate::sql::{qualified, quote_ident};
use crate::tx::{self, Discard};
use crate::{Cell, Column, Connection, DataSourceConfig, DbError, DbKind, DbResult, Page, ResultSink};

/// What a dump contains.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DumpOptions {
    /// Limit to these schemas (PostgreSQL); empty: the whole database.
    pub schemas: Vec<String>,
    /// Limit to these tables (`schema.table` or `table`).
    pub tables: Vec<String>,
    pub data_only: bool,
    pub schema_only: bool,
    /// PostgreSQL custom format (`pg_restore` reads it); plain SQL otherwise.
    pub custom_format: bool,
}

/// A tool run: program, arguments, environment, and where stdin comes from / stdout goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Extra environment (the password lives here).
    pub env: Vec<(String, String)>,
    /// Feed this file to stdin.
    pub stdin: Option<PathBuf>,
}

impl ToolCommand {
    /// The command as shown in the log, without secrets (the environment is not shown).
    pub fn display(&self) -> String {
        let mut s = self.program.display().to_string();
        for a in &self.args {
            s.push(' ');
            if a.contains([' ', '"', '\'']) {
                s.push_str(&format!("'{}'", a.replace('\'', "'\\''")));
            } else {
                s.push_str(a);
            }
        }
        if let Some(p) = &self.stdin {
            s.push_str(&format!(" < {}", p.display()));
        }
        s
    }
}

/// Find a program: `configured` when set (a path or a name), else on `PATH`.
pub fn find_tool(name: &str, configured: Option<&str>, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if let Some(c) = configured.map(str::trim).filter(|c| !c.is_empty()) {
        let p = PathBuf::from(c);
        if p.components().count() > 1 {
            return p.is_file().then_some(p);
        }
        return find_tool(c, None, path_var);
    }
    let paths = path_var.map(std::ffi::OsString::from).or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&paths).map(|d| d.join(name)).find(|p| p.is_file())
}

fn host_port(cfg: &DataSourceConfig) -> (String, String) {
    let host = if cfg.host.trim().is_empty() { "localhost".to_string() } else { cfg.host.trim().to_string() };
    (host, cfg.port_or_default().to_string())
}

fn pg_env(cfg: &DataSourceConfig, password: Option<&str>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(p) = password.filter(|p| !p.is_empty()) {
        env.push(("PGPASSWORD".into(), p.to_string()));
    }
    if let Some(m) = cfg.options.get("sslmode") {
        env.push(("PGSSLMODE".into(), m.trim().to_string()));
    }
    env.push(("PGCONNECT_TIMEOUT".into(), cfg.connect_timeout().as_secs().max(1).to_string()));
    env.push(("PGAPPNAME".into(), "CodeCraft".into()));
    env
}

fn my_env(password: Option<&str>) -> Vec<(String, String)> {
    password.filter(|p| !p.is_empty()).map(|p| vec![("MYSQL_PWD".to_string(), p.to_string())]).unwrap_or_default()
}

fn pg_conn_args(cfg: &DataSourceConfig) -> Vec<String> {
    let (host, port) = host_port(cfg);
    let mut a = vec!["--host".into(), host, "--port".into(), port, "--no-password".into()];
    if !cfg.user.is_empty() {
        a.extend(["--username".into(), cfg.user.clone()]);
    }
    a
}

fn my_conn_args(cfg: &DataSourceConfig) -> Vec<String> {
    let (host, port) = host_port(cfg);
    let mut a = vec![format!("--host={host}"), format!("--port={port}"), "--protocol=TCP".into()];
    if !cfg.user.is_empty() {
        a.push(format!("--user={}", cfg.user));
    }
    a
}

/// The `pg_dump` / `mysqldump` command writing to `out`. `cfg` is the routed config (through
/// the SSH tunnel when there is one).
pub fn dump_command(
    cfg: &DataSourceConfig,
    password: Option<&str>,
    program: PathBuf,
    out: &Path,
    opts: &DumpOptions,
) -> DbResult<ToolCommand> {
    if opts.data_only && opts.schema_only {
        return Err(DbError::Config("choose data only or schema only, not both".into()));
    }
    match cfg.kind {
        DbKind::Postgres => {
            let mut args = pg_conn_args(cfg);
            args.extend(["--verbose".into(), format!("--file={}", out.display())]);
            args.push(if opts.custom_format { "--format=custom" } else { "--format=plain" }.into());
            for s in &opts.schemas {
                args.push(format!("--schema={s}"));
            }
            for t in &opts.tables {
                args.push(format!("--table={t}"));
            }
            if opts.data_only {
                args.push("--data-only".into());
            }
            if opts.schema_only {
                args.push("--schema-only".into());
            }
            if !cfg.database.is_empty() {
                args.push(format!("--dbname={}", cfg.database));
            }
            Ok(ToolCommand { program, args, env: pg_env(cfg, password), stdin: None })
        }
        DbKind::Mysql | DbKind::Mariadb => {
            if cfg.database.is_empty() && opts.schemas.is_empty() {
                return Err(DbError::Config("choose the database (schema) to dump".into()));
            }
            let mut args = my_conn_args(cfg);
            args.extend([
                "--verbose".into(),
                "--single-transaction".into(),
                "--routines".into(),
                "--triggers".into(),
                format!("--result-file={}", out.display()),
            ]);
            if opts.data_only {
                args.push("--no-create-info".into());
            }
            if opts.schema_only {
                args.push("--no-data".into());
            }
            let db = opts.schemas.first().cloned().unwrap_or_else(|| cfg.database.clone());
            args.push(db);
            for t in &opts.tables {
                args.push(t.rsplit('.').next().unwrap_or(t).to_string());
            }
            Ok(ToolCommand { program, args, env: my_env(password), stdin: None })
        }
        DbKind::Sqlite => Err(DbError::Unsupported("SQLite dumps are written natively (no tool)".into())),
    }
}

/// A PostgreSQL custom-format archive starts with `PGDMP`.
pub fn is_pg_archive(path: &Path) -> bool {
    let mut b = [0u8; 5];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)).is_ok() && &b == b"PGDMP"
}

/// Which tool restores `file`: `pg_restore` for a custom archive, `psql` for SQL, `mysql`.
pub fn restore_tool(kind: DbKind, file: &Path) -> Option<&'static str> {
    match kind {
        DbKind::Postgres if is_pg_archive(file) => Some("pg_restore"),
        DbKind::Postgres => Some("psql"),
        DbKind::Mysql | DbKind::Mariadb => Some("mysql"),
        DbKind::Sqlite => None,
    }
}

/// The restore command for `file` (all or nothing: one transaction, stop at the first error).
pub fn restore_command(
    cfg: &DataSourceConfig,
    password: Option<&str>,
    program: PathBuf,
    file: &Path,
) -> DbResult<ToolCommand> {
    let tool = program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match cfg.kind {
        DbKind::Postgres => {
            let mut args = pg_conn_args(cfg);
            if tool.starts_with("pg_restore") {
                args.extend([
                    "--verbose".into(),
                    "--single-transaction".into(),
                    "--exit-on-error".into(),
                    "--no-owner".into(),
                ]);
                if !cfg.database.is_empty() {
                    args.push(format!("--dbname={}", cfg.database));
                }
                args.push(file.display().to_string());
            } else {
                args.extend([
                    "--no-psqlrc".into(),
                    "--quiet".into(),
                    "--single-transaction".into(),
                    "--set=ON_ERROR_STOP=1".into(),
                    format!("--file={}", file.display()),
                ]);
                if !cfg.database.is_empty() {
                    args.push(format!("--dbname={}", cfg.database));
                }
            }
            Ok(ToolCommand { program, args, env: pg_env(cfg, password), stdin: None })
        }
        DbKind::Mysql | DbKind::Mariadb => {
            if cfg.database.is_empty() {
                return Err(DbError::Config("choose the database to restore into".into()));
            }
            let mut args = my_conn_args(cfg);
            args.push("--batch".into());
            args.push(cfg.database.clone());
            Ok(ToolCommand { program, args, env: my_env(password), stdin: Some(file.to_path_buf()) })
        }
        DbKind::Sqlite => Err(DbError::Unsupported("SQLite restores run natively (no tool)".into())),
    }
}

/// Run a tool to completion: stderr lines go to `log`, `progress` gets the bytes fed to stdin
/// (restores) or written to `watch` (dumps). `cancel` kills it.
pub fn run_tool(
    cmd: &ToolCommand,
    watch: Option<&Path>,
    cancel: &AtomicBool,
    log: &mut dyn FnMut(String),
    progress: &mut dyn FnMut(u64),
) -> DbResult<()> {
    let mut c = Command::new(&cmd.program);
    c.args(&cmd.args).stdout(Stdio::piped()).stderr(Stdio::piped());
    for (k, v) in &cmd.env {
        c.env(k, v);
    }
    let input = match &cmd.stdin {
        Some(p) => {
            c.stdin(Stdio::piped());
            Some(std::fs::File::open(p).map_err(|e| DbError::Io(format!("{}: {e}", p.display())))?)
        }
        None => {
            c.stdin(Stdio::null());
            None
        }
    };
    let mut child = c.spawn().map_err(|e| DbError::Io(format!("cannot run {}: {e}", cmd.program.display())))?;
    let (ltx, lrx) = crossbeam_channel::unbounded::<String>();
    let mut readers = Vec::new();
    for stream in [
        child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
        child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let ltx = ltx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream.take(64 << 20)).lines() {
                match line {
                    Ok(l) => {
                        if ltx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }));
    }
    let fed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let feeder = match (input, child.stdin.take()) {
        (Some(mut file), Some(mut stdin)) => {
            let fed = fed.clone();
            Some(std::thread::spawn(move || {
                let mut buf = vec![0u8; 64 * 1024];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if stdin.write_all(buf.get(..n).unwrap_or(&[])).is_err() {
                                break;
                            }
                            fed.fetch_add(n as u64, Ordering::SeqCst);
                        }
                    }
                }
            }))
        }
        _ => None,
    };
    drop(ltx);
    let mut tail: Vec<String> = Vec::new();
    let status = loop {
        while let Ok(l) = lrx.try_recv() {
            tail.push(l.clone());
            if tail.len() > 20 {
                tail.remove(0);
            }
            log(l);
        }
        if cancel.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(DbError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => return Err(DbError::Io(e.to_string())),
        }
        let bytes = match watch {
            Some(p) => std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
            None => fed.load(Ordering::SeqCst),
        };
        progress(bytes);
        std::thread::sleep(Duration::from_millis(50));
    };
    if let Some(f) = feeder {
        let _ = f.join();
    }
    for r in readers {
        let _ = r.join();
    }
    while let Ok(l) = lrx.try_recv() {
        tail.push(l.clone());
        log(l);
    }
    if status.success() {
        Ok(())
    } else {
        let last: Vec<&String> = tail.iter().rev().filter(|l| !l.trim().is_empty()).take(3).collect();
        let msg = last.into_iter().rev().cloned().collect::<Vec<_>>().join("\n");
        Err(DbError::Query(format!(
            "{} failed ({status}){}",
            cmd.program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            if msg.is_empty() { String::new() } else { format!(": {msg}") }
        )))
    }
}

/// `<file>.partial`, where output goes until it is complete.
pub fn partial_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".partial");
    PathBuf::from(s)
}

/// Move a finished `.partial` file into place (or remove it after a failure).
pub fn finish_partial(path: &Path, ok: bool) -> DbResult<()> {
    let partial = partial_path(path);
    if ok {
        std::fs::rename(&partial, path).map_err(|e| DbError::Io(format!("{}: {e}", path.display())))
    } else {
        let _ = std::fs::remove_file(&partial);
        Ok(())
    }
}

// ---------------------------------------------------------------- SQLite

struct WriteRows<'a> {
    out: &'a mut dyn Write,
    target: String,
    error: Option<std::io::Error>,
    cancel: &'a AtomicBool,
    rows: u64,
}

impl ResultSink for WriteRows<'_> {
    fn columns(&mut self, _: &[Column]) {}
    fn page(&mut self, page: Page) -> bool {
        for row in page.rows {
            let vals: Vec<String> =
                row.iter().map(|c| literal(DbKind::Sqlite, c).unwrap_or_else(|_| literal_text(c))).collect();
            if let Err(e) = writeln!(self.out, "INSERT INTO {} VALUES({});", self.target, vals.join(",")) {
                self.error = Some(e);
                return false;
            }
            self.rows += 1;
        }
        !self.cancel.load(Ordering::SeqCst)
    }
}

fn literal_text(c: &Cell) -> String {
    match c {
        Cell::Float(f) if f.is_nan() => "NULL".into(),
        Cell::Float(f) if *f > 0.0 => "1e999".into(),
        Cell::Float(_) => "-1e999".into(),
        other => format!("'{}'", other.to_plain().replace('\'', "''")),
    }
}

/// Write an SQLite database as SQL (`sqlite3 .dump`): tables with their rows, then indexes,
/// triggers and views, in one transaction. Returns the rows written.
pub fn sqlite_dump(
    conn: &mut dyn Connection,
    out: &mut dyn Write,
    tables: &[String],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> DbResult<u64> {
    let io = |e: std::io::Error| DbError::Io(e.to_string());
    let (_, objects, _) = crate::query_all(
        conn,
        "SELECT type, name, tbl_name, sql FROM sqlite_master WHERE sql IS NOT NULL \
         AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\' ORDER BY CASE type WHEN 'table' THEN 0 ELSE 1 END, rowid",
    )?;
    let wanted = |t: &str| tables.is_empty() || tables.iter().any(|w| w.rsplit('.').next() == Some(t));
    writeln!(out, "PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;").map_err(io)?;
    let mut rows = 0u64;
    let text = |c: Option<&Cell>| c.map(Cell::to_plain).unwrap_or_default();
    for o in &objects {
        let (ty, name, tbl, sql) = (text(o.first()), text(o.get(1)), text(o.get(2)), text(o.get(3)));
        if ty != "table" || !wanted(&name) {
            continue;
        }
        writeln!(out, "{sql};").map_err(io)?;
        let _ = tbl;
        let target = quote_ident(DbKind::Sqlite, &name);
        let mut sink = WriteRows { out: &mut *out, target: target.clone(), error: None, cancel, rows: 0 };
        conn.execute(&format!("SELECT * FROM {target}"), crate::PAGE_SIZE, &mut sink)?;
        if let Some(e) = sink.error {
            return Err(io(e));
        }
        rows += sink.rows;
        progress(rows);
        if cancel.load(Ordering::SeqCst) {
            return Err(DbError::Cancelled);
        }
    }
    for o in &objects {
        let (ty, tbl, sql) = (text(o.first()), text(o.get(2)), text(o.get(3)));
        if ty != "table" && wanted(&tbl) {
            writeln!(out, "{sql};").map_err(io)?;
        }
    }
    writeln!(out, "COMMIT;").map_err(io)?;
    out.flush().map_err(io)?;
    Ok(rows)
}

/// Largest script restored natively.
pub const MAX_SCRIPT_BYTES: u64 = 512 << 20;

/// Run an SQL script (an SQLite dump) in one transaction: everything or nothing. The script's
/// own BEGIN/COMMIT are skipped. Returns the statements run; an error names the failing one.
pub fn run_script(
    conn: &mut dyn Connection,
    path: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize, usize),
) -> DbResult<usize> {
    let len = std::fs::metadata(path).map_err(|e| DbError::Io(format!("{}: {e}", path.display())))?.len();
    if len > MAX_SCRIPT_BYTES {
        return Err(DbError::Unsupported(format!("{} is larger than 512 MB", path.display())));
    }
    let text = std::fs::read_to_string(path).map_err(|e| DbError::Io(format!("{}: {e}", path.display())))?;
    let dialect = conn.kind().dialect();
    let stmts: Vec<String> = crate::split_statements(&text, dialect)
        .into_iter()
        .map(|s| s.text)
        .filter(|s| crate::transaction_effect(s, dialect) == crate::TxEffect::None)
        .collect();
    let total = stmts.len();
    if conn.transaction_status().is_open() {
        return Err(DbError::Query("a transaction is open on this connection; end it first".into()));
    }
    tx::begin(conn, None)?;
    for (i, sql) in stmts.iter().enumerate() {
        let failed = if cancel.load(Ordering::SeqCst) {
            Some(DbError::Cancelled)
        } else {
            conn.execute(sql, 1, &mut Discard).err()
        };
        if let Some(e) = failed {
            let _ = tx::run(conn, "ROLLBACK");
            if e == DbError::Cancelled {
                return Err(e);
            }
            let first: String = sql.lines().next().unwrap_or_default().chars().take(200).collect();
            return Err(DbError::Query(format!(
                "statement {} of {total} failed: {e}\n  {first}\nNothing was restored (rolled back).",
                i + 1
            )));
        }
        if i % 200 == 0 {
            progress(i, total);
        }
    }
    tx::end(conn, true)?;
    progress(total, total);
    Ok(total)
}

// ---------------------------------------------------------------- streamed export

/// Writes rows to a file page by page in an export format (CSV, TSV, JSON, SQL INSERTs,
/// Markdown); memory stays at one page whatever the size.
pub struct StreamWriter<W: Write> {
    out: W,
    format: ExportFormat,
    columns: Vec<Column>,
    keys: Vec<String>,
    target: InsertTarget,
    rows: u64,
}

impl<W: Write> StreamWriter<W> {
    pub fn new(out: W, format: ExportFormat, target: InsertTarget) -> Self {
        Self { out, format, columns: Vec::new(), keys: Vec::new(), target, rows: 0 }
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    fn field(s: &str, sep: char) -> String {
        if s.contains([sep, '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    }

    fn md(s: &str) -> String {
        s.replace('\\', "\\\\").replace('|', "\\|").replace("\r\n", "<br>").replace(['\n', '\r'], "<br>")
    }

    /// The header (call once, with the result's columns).
    pub fn begin(&mut self, columns: &[Column]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        self.keys.clear();
        for c in columns {
            let mut k = c.name.clone();
            let mut n = 2;
            while self.keys.contains(&k) {
                k = format!("{}_{n}", c.name);
                n += 1;
            }
            self.keys.push(k);
        }
        match self.format {
            ExportFormat::Csv | ExportFormat::Tsv => {
                let sep = if self.format == ExportFormat::Csv { ',' } else { '\t' };
                let h: Vec<String> = columns.iter().map(|c| Self::field(&c.name, sep)).collect();
                writeln!(self.out, "{}", h.join(&sep.to_string()))
            }
            ExportFormat::Json => write!(self.out, "["),
            ExportFormat::Sql => Ok(()),
            ExportFormat::Markdown => {
                let names: Vec<String> = columns.iter().map(|c| format!(" {} |", Self::md(&c.name))).collect();
                writeln!(self.out, "|{}\n|{}", names.concat(), " --- |".repeat(columns.len()))
            }
        }
    }

    pub fn write_rows(&mut self, rows: &[Vec<Cell>]) -> std::io::Result<()> {
        for row in rows {
            match self.format {
                ExportFormat::Csv | ExportFormat::Tsv => {
                    let sep = if self.format == ExportFormat::Csv { ',' } else { '\t' };
                    let f: Vec<String> = row.iter().map(|c| Self::field(&c.to_plain(), sep)).collect();
                    writeln!(self.out, "{}", f.join(&sep.to_string()))?;
                }
                ExportFormat::Json => {
                    let mut m = serde_json::Map::new();
                    for (k, c) in self.keys.iter().zip(row) {
                        m.insert(k.clone(), serde_json::to_value(c).unwrap_or(serde_json::Value::Null));
                    }
                    let sep = if self.rows == 0 { "\n  " } else { ",\n  " };
                    write!(self.out, "{sep}{}", serde_json::Value::Object(m))?;
                }
                ExportFormat::Sql => {
                    let t = &self.target;
                    let table = qualified(t.kind, &t.schema, if t.table.is_empty() { "my_table" } else { &t.table });
                    let cols: Vec<String> = self.columns.iter().map(|c| quote_ident(t.kind, &c.name)).collect();
                    let vals: Vec<String> = row
                        .iter()
                        .map(|c| {
                            literal(t.kind, c)
                                .or_else(|_| literal(t.kind, &Cell::Text(c.to_plain())))
                                .unwrap_or_else(|_| "NULL".into())
                        })
                        .collect();
                    writeln!(self.out, "INSERT INTO {table} ({}) VALUES ({});", cols.join(", "), vals.join(", "))?;
                }
                ExportFormat::Markdown => {
                    let f: Vec<String> = row.iter().map(|c| format!(" {} |", Self::md(&c.to_plain()))).collect();
                    writeln!(self.out, "|{}", f.concat())?;
                }
            }
            self.rows += 1;
        }
        Ok(())
    }

    /// The footer; returns the writer.
    pub fn finish(mut self) -> std::io::Result<W> {
        if self.format == ExportFormat::Json {
            writeln!(self.out, "{}]", if self.rows == 0 { "" } else { "\n" })?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

/// A sink writing pages through a [`StreamWriter`]; stops on `cancel` or a write error.
pub struct ExportSink<'a, W: Write> {
    pub writer: Option<StreamWriter<W>>,
    pub error: Option<std::io::Error>,
    pub cancel: &'a AtomicBool,
    pub progress: &'a mut dyn FnMut(u64),
}

impl<W: Write> ResultSink for ExportSink<'_, W> {
    fn columns(&mut self, columns: &[Column]) {
        if let Some(w) = &mut self.writer
            && let Err(e) = w.begin(columns)
        {
            self.error = Some(e);
        }
    }

    fn page(&mut self, page: Page) -> bool {
        if self.error.is_some() || self.cancel.load(Ordering::SeqCst) {
            return false;
        }
        let Some(w) = &mut self.writer else { return false };
        if let Err(e) = w.write_rows(&page.rows) {
            self.error = Some(e);
            return false;
        }
        (self.progress)(w.rows());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: DbKind) -> DataSourceConfig {
        DataSourceConfig {
            id: "x".into(),
            kind,
            host: "db.example".into(),
            port: Some(6543),
            user: "ann".into(),
            database: "shop".into(),
            ..Default::default()
        }
    }

    #[test]
    fn commands_keep_passwords_off_the_command_line() {
        let opts = DumpOptions { tables: vec!["public.t".into()], data_only: true, ..Default::default() };
        let c =
            dump_command(&cfg(DbKind::Postgres), Some("s3cret"), "pg_dump".into(), Path::new("/o.sql"), &opts).unwrap();
        assert!(!c.args.iter().any(|a| a.contains("s3cret")) && !c.display().contains("s3cret"));
        assert!(c.env.contains(&("PGPASSWORD".into(), "s3cret".into())));
        assert!(c.args.contains(&"--table=public.t".into()) && c.args.contains(&"--data-only".into()));
        assert!(c.args.contains(&"--dbname=shop".into()) && c.args.contains(&"6543".into()));
        let c = dump_command(&cfg(DbKind::Mysql), Some("pw"), "mysqldump".into(), Path::new("/o.sql"), &opts).unwrap();
        assert!(!c.args.iter().any(|a| a.contains("pw\"") || a == "pw" || a.contains("--password")));
        assert_eq!(c.env, vec![("MYSQL_PWD".to_string(), "pw".to_string())]);
        assert!(c.args.ends_with(&["shop".into(), "t".into()]));
        assert!(dump_command(&cfg(DbKind::Sqlite), None, "x".into(), Path::new("/o"), &opts).is_err());
        let both = DumpOptions { data_only: true, schema_only: true, ..Default::default() };
        assert!(dump_command(&cfg(DbKind::Postgres), None, "x".into(), Path::new("/o"), &both).is_err());

        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("a.dump");
        std::fs::write(&archive, b"PGDMP\x01").unwrap();
        let plain = dir.path().join("p.sql");
        std::fs::write(&plain, "select 1;").unwrap();
        assert_eq!(restore_tool(DbKind::Postgres, &archive), Some("pg_restore"));
        assert_eq!(restore_tool(DbKind::Postgres, &plain), Some("psql"));
        let r = restore_command(&cfg(DbKind::Postgres), Some("pw"), "psql".into(), &plain).unwrap();
        assert!(r.args.contains(&"--single-transaction".into()) && r.args.contains(&"--set=ON_ERROR_STOP=1".into()));
        let r = restore_command(&cfg(DbKind::Mariadb), Some("pw"), "mysql".into(), &plain).unwrap();
        assert_eq!(r.stdin.as_deref(), Some(plain.as_path()));
        assert!(r.display().contains("< "));
    }

    #[cfg(unix)]
    #[test]
    fn tools_run_with_logs_progress_failure_and_cancel() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let ok = script("ok", "echo working >&2; cat > \"$OUT\"; echo \"pw=$PGPASSWORD\" >&2");
        let input = dir.path().join("in.sql");
        std::fs::write(&input, "x".repeat(100_000)).unwrap();
        let out = dir.path().join("out");
        let cmd = ToolCommand {
            program: ok,
            args: Vec::new(),
            env: vec![("OUT".into(), out.display().to_string()), ("PGPASSWORD".into(), "pw".into())],
            stdin: Some(input),
        };
        let (mut logs, mut last) = (Vec::new(), 0);
        run_tool(&cmd, None, &AtomicBool::new(false), &mut |l| logs.push(l), &mut |b| last = b).unwrap();
        assert!(logs.contains(&"working".to_string()) && logs.contains(&"pw=pw".to_string()), "{logs:?}");
        assert_eq!(std::fs::metadata(&out).unwrap().len(), 100_000);
        let bad = ToolCommand {
            program: script("bad", "echo 'FATAL: nope' >&2; exit 3"),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
        };
        let e = run_tool(&bad, None, &AtomicBool::new(false), &mut |_| {}, &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("FATAL: nope"), "{e}");
        let slow = ToolCommand { program: script("slow", "sleep 30"), args: Vec::new(), env: Vec::new(), stdin: None };
        let cancel = AtomicBool::new(true);
        let t = std::time::Instant::now();
        assert_eq!(run_tool(&slow, None, &cancel, &mut |_| {}, &mut |_| {}).unwrap_err(), DbError::Cancelled);
        assert!(t.elapsed() < Duration::from_secs(10));
        let _ = last;
        assert_eq!(find_tool("ok", None, Some(dir.path().as_os_str())), Some(dir.path().join("ok")));
        assert_eq!(find_tool("missing-tool", None, Some(dir.path().as_os_str())), None);
        let abs = dir.path().join("bad").display().to_string();
        assert!(find_tool("pg_dump", Some(&abs), None).is_some());
    }

    #[test]
    fn sqlite_dump_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mk =
            |n: &str| DataSourceConfig { kind: DbKind::Sqlite, path: Some(dir.path().join(n)), ..Default::default() };
        let mut a = crate::connect(&mk("a.db"), None).unwrap();
        for s in [
            "create table t (id integer primary key autoincrement, v text, b blob, f real)",
            "insert into t (v, b, f) values ('it''s', x'00ff', 1.5), (null, null, null)",
            "create index t_v on t(v)",
            "create view tv as select v from t",
        ] {
            crate::query_all(a.as_mut(), s).unwrap();
        }
        let mut out = Vec::new();
        let n = sqlite_dump(a.as_mut(), &mut out, &[], &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!(n, 2);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("INSERT INTO \"t\" VALUES(1,'it''s',X'00FF',1.5);"), "{text}");
        let file = dir.path().join("dump.sql");
        std::fs::write(&file, &text).unwrap();
        let mut b = crate::connect(&mk("b.db"), None).unwrap();
        run_script(b.as_mut(), &file, &AtomicBool::new(false), &mut |_, _| {}).unwrap();
        let (_, rows, _) = crate::query_all(b.as_mut(), "select v, b from t order by id").unwrap();
        assert_eq!(rows[0], vec![Cell::Text("it's".into()), Cell::Bytes(vec![0, 0xff])]);
        assert_eq!(crate::query_all(b.as_mut(), "select count(*) from tv").unwrap().1[0][0], Cell::Int(2));
        // A failing script restores nothing.
        std::fs::write(&file, "create table u (a int);\ninsert into u values (1);\ninsert into nope values (2);")
            .unwrap();
        let e = run_script(b.as_mut(), &file, &AtomicBool::new(false), &mut |_, _| {}).unwrap_err();
        assert!(e.to_string().contains("statement 3 of 3"), "{e}");
        assert!(crate::query_all(b.as_mut(), "select * from u").is_err());
    }

    #[test]
    fn stream_writer_formats() {
        let cols = vec![
            Column { name: "id".into(), type_name: String::new() },
            Column { name: "id".into(), type_name: String::new() },
        ];
        let rows = [vec![Cell::Int(1), Cell::Text("a,b".into())], vec![Cell::Int(2), Cell::Null]];
        let run = |f: ExportFormat| {
            let mut w = StreamWriter::new(Vec::new(), f, InsertTarget { kind: DbKind::Sqlite, ..Default::default() });
            w.begin(&cols).unwrap();
            w.write_rows(&rows[..1]).unwrap();
            w.write_rows(&rows[1..]).unwrap();
            String::from_utf8(w.finish().unwrap()).unwrap()
        };
        assert_eq!(run(ExportFormat::Csv), "id,id\n1,\"a,b\"\n2,\n");
        assert_eq!(run(ExportFormat::Tsv), "id\tid\n1\ta,b\n2\t\n");
        let j: serde_json::Value = serde_json::from_str(&run(ExportFormat::Json)).unwrap();
        assert_eq!(j[1]["id_2"], serde_json::Value::Null);
        assert_eq!(j[0]["id_2"], "a,b");
        assert!(run(ExportFormat::Sql).starts_with("INSERT INTO \"my_table\" (\"id\", \"id\") VALUES (1, 'a,b');\n"));
        assert_eq!(run(ExportFormat::Markdown), "| id | id |\n| --- | --- |\n| 1 | a,b |\n| 2 |  |\n");
        let mut empty = StreamWriter::new(Vec::new(), ExportFormat::Json, InsertTarget::default());
        empty.begin(&cols).unwrap();
        let s = String::from_utf8(empty.finish().unwrap()).unwrap();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&s).unwrap(), serde_json::json!([]));
    }
}
