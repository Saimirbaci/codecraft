//! Microsoft SQL Server (and Azure SQL) through `tiberius` (TDS 7.4/8.0, SQL Server
//! authentication, TLS through the platform stack).
//!
//! tiberius is asynchronous; each connection owns a current-thread tokio runtime and blocks on it,
//! so the rest of the database layer stays synchronous and nothing is shared between threads.
//! Cancel sets a flag the running statement polls every 50 ms; the statement then sends the TDS
//! attention signal (`cancel_query`) and drains the acknowledgement, keeping the connection
//! usable. Plain INSERT/UPDATE/DELETE/MERGE run through `sp_executesql` so the grid can report
//! rows affected; everything else runs as a batch, and every result set of the batch is shown
//! (each in its own result tab: [`BatchResults`]).
//!
//! Options: `encrypt` (`off` = login only, `on` (default), `strict` (TDS 8.0), `disable`),
//! `trust_server_certificate` (`true` for self-signed certificates), `sslrootcert`,
//! `tls_server_name`, `application_intent=readonly`. Named instances need their port.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use futures_util::TryStreamExt;
use futures_util::future::Either;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::{
    Canceller, Cell, Column, ColumnInfo, Connection, DataSourceConfig, DatabaseInfo, DbError, DbKind, DbResult,
    ExecSummary, ForeignKeyInfo, IndexInfo, Pager, ResultSink, TableDetails, TableInfo, TableKind,
};

type Tds = Client<Compat<TcpStream>>;

/// How often a running statement looks at the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);

struct MssqlCancel {
    flag: AtomicBool,
}

impl Canceller for MssqlCancel {
    fn cancel(&self) -> DbResult<()> {
        self.flag.store(true, Ordering::SeqCst);
        Ok(())
    }
}

pub struct MssqlConn {
    rt: tokio::runtime::Runtime,
    client: Option<Tds>,
    cancel: Arc<MssqlCancel>,
    broken: bool,
    /// `SET SHOWPLAN_XML ON` is active: statements must go as batches to return their plan.
    showplan: bool,
}

fn option<'a>(cfg: &'a DataSourceConfig, key: &str) -> Option<&'a str> {
    cfg.options.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn truthy(v: Option<&str>) -> bool {
    v.is_some_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// The tiberius configuration of a data source.
pub fn config(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<Config> {
    let mut c = Config::new();
    let host = cfg.host.trim();
    if host.contains('\\') {
        return Err(DbError::Config(
            "named instances (host\\instance) need the SQL Browser; enter the instance's TCP port instead".into(),
        ));
    }
    c.host(host);
    c.port(cfg.port_or_default());
    if !cfg.database.trim().is_empty() {
        c.database(cfg.database.trim());
    }
    c.application_name("CodeCraft");
    c.authentication(AuthMethod::sql_server(cfg.user.trim(), password.unwrap_or_default()));
    let level = match option(cfg, "encrypt").map(str::to_ascii_lowercase).as_deref() {
        None | Some("on" | "true" | "yes" | "mandatory" | "required") => EncryptionLevel::Required,
        Some("off" | "false" | "no" | "optional") => EncryptionLevel::Off,
        Some("strict") => EncryptionLevel::Strict,
        Some("disable" | "disabled" | "none") => EncryptionLevel::NotSupported,
        Some(other) => {
            return Err(DbError::Config(format!("unknown encrypt option `{other}` (off, on, strict, disable)")));
        }
    };
    c.encryption(level);
    if truthy(option(cfg, "trust_server_certificate")) || truthy(option(cfg, "trustservercertificate")) {
        c.trust_cert();
    } else if let Some(ca) = cfg.tls_files(None)?.root_cert {
        c.trust_cert_ca(ca.display().to_string());
    }
    if let Some(name) = option(cfg, "tls_server_name") {
        c.hostname_in_certificate(name);
    }
    if option(cfg, "application_intent").is_some_and(|v| v.eq_ignore_ascii_case("readonly")) {
        c.readonly(true);
    }
    c.handshake_timeout(Some(cfg.connect_timeout()));
    // The engine's statement timeout and Cancel stop long statements; tiberius' 30 s default
    // would break them.
    c.command_timeout(None);
    Ok(c)
}

/// A tiberius error as the user should see it.
fn err(e: tiberius::error::Error) -> DbError {
    use tiberius::error::Error as E;
    match e {
        E::Server(t) => {
            let mut msg = format!("[{}] {}", t.code(), t.message());
            if t.line() > 0 {
                msg.push_str(&format!(" (line {})", t.line()));
            }
            DbError::Query(msg)
        }
        E::Tls(t) => DbError::Connect(format!(
            "TLS: {t}. For a self-signed certificate set the option trust_server_certificate=true"
        )),
        other => DbError::Query(other.to_string()),
    }
}

/// Errors after which the connection is out of sync or gone.
fn fatal(e: &tiberius::error::Error) -> bool {
    use tiberius::error::Error as E;
    matches!(e, E::Io { .. } | E::Protocol(_) | E::Tls(_) | E::Utf16 | E::Encoding(_))
        || e.code().is_some_and(|c| matches!(c, 596 | 3980 | 233))
}

/// Run `fut` unless cancel is requested first (`None` then).
async fn race<F: Future>(fut: F, flag: &AtomicBool) -> Option<F::Output> {
    let watch = async {
        loop {
            if flag.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(CANCEL_POLL).await;
        }
    };
    let fut = std::pin::pin!(fut);
    let watch = std::pin::pin!(watch);
    match futures_util::future::select(fut, watch).await {
        Either::Left((out, _)) => Some(out),
        Either::Right(_) => None,
    }
}

async fn open_client(config: Config, timeout: Duration) -> Result<Tds, DbError> {
    let mut config = config;
    for _ in 0..2 {
        let addr = config.get_addr();
        let tcp = match tokio::time::timeout(timeout, TcpStream::connect(&addr)).await {
            Ok(Ok(t)) => t,
            Ok(Err(e)) => return Err(DbError::Connect(format!("cannot connect to {addr}: {e}"))),
            Err(_) => return Err(DbError::Connect(format!("connecting to {addr} timed out"))),
        };
        let _ = tcp.set_nodelay(true);
        match Client::connect(config.clone(), tcp.compat_write()).await {
            Ok(c) => return Ok(c),
            // Azure SQL's gateway redirects to the database's node.
            Err(tiberius::error::Error::Routing { host, port }) => {
                config.host(&host);
                config.port(port);
            }
            Err(tiberius::error::Error::Server(t)) if t.code() == 18456 => {
                return Err(DbError::Connect(format!("login failed: {}", t.message())));
            }
            Err(e) => {
                return Err(match err(e) {
                    DbError::Query(m) => DbError::Connect(m),
                    other => other,
                });
            }
        }
    }
    Err(DbError::Connect("the server redirected the connection too often".into()))
}

/// `INSERT/UPDATE/DELETE/MERGE …` without OUTPUT: run through sp_executesql for the row count.
fn plain_dml(sql: &str) -> bool {
    let words = crate::lex::words(sql, crate::Dialect::TSql);
    matches!(words.first().map(String::as_str), Some("INSERT" | "UPDATE" | "DELETE" | "MERGE"))
        && !words.iter().any(|w| w == "OUTPUT" || w == "GO")
}

impl MssqlConn {
    pub fn open(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<Self> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(|e| DbError::Connect(format!("cannot start the SQL Server client: {e}")))?;
        let config = config(cfg, password)?;
        let client = rt.block_on(open_client(config, cfg.connect_timeout()))?;
        Ok(Self {
            rt,
            client: Some(client),
            cancel: Arc::new(MssqlCancel { flag: AtomicBool::new(false) }),
            broken: false,
            showplan: false,
        })
    }

    fn client(&mut self) -> DbResult<&mut Tds> {
        if self.broken {
            return Err(DbError::Query("the connection to SQL Server was lost; reconnect".into()));
        }
        self.client.as_mut().ok_or_else(|| DbError::Query("not connected".into()))
    }

    /// Every row of an introspection query (parameters are `@P1`, `@P2`, …), as text.
    fn strings(&mut self, sql: &str, params: &[&str]) -> DbResult<Vec<Vec<Option<String>>>> {
        let rt = &self.rt;
        let client = match self.client.as_mut() {
            Some(c) if !self.broken => c,
            _ => return Err(DbError::Query("the connection to SQL Server was lost; reconnect".into())),
        };
        let owned: Vec<String> = params.iter().map(|p| p.to_string()).collect();
        let refs: Vec<&dyn tiberius::ToSql> = owned.iter().map(|p| p as &dyn tiberius::ToSql).collect();
        let result: Result<Vec<Vec<Option<String>>>, tiberius::error::Error> = rt.block_on(async {
            let stream = client.query(sql, &refs).await?;
            let rows = stream.into_first_result().await?;
            Ok(rows.iter().map(|r| r.cells().map(|(_, d)| text(d)).collect()).collect())
        });
        result.map_err(|e| {
            if fatal(&e) {
                self.broken = true;
            }
            err(e)
        })
    }
}

fn text(d: &ColumnData<'static>) -> Option<String> {
    match cell(d) {
        Cell::Null => None,
        c => Some(c.display()),
    }
}

fn col(row: &[Option<String>], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

// ---------------------------------------------------------------- values

/// Days since 1970-01-01 → (year, month, day) (proleptic Gregorian).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, u32::try_from(m).unwrap_or(1), u32::try_from(d).unwrap_or(1))
}

/// 0001-01-01 and 1900-01-01 relative to 1970-01-01.
const DAY_ONE: i64 = -719_162;
const DAY_1900: i64 = -25_567;

fn date_text(days_since_1970: i64) -> String {
    let (y, m, d) = civil(days_since_1970);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Time of day from `increments` of 10^-scale seconds.
fn time_text(increments: u64, scale: u8) -> String {
    let scale = u32::from(scale.min(7));
    let unit = 10u64.pow(scale);
    let secs = increments / unit;
    let frac = increments % unit;
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if scale == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{frac:0w$}", w = scale as usize)
    }
}

/// A decimal from its scaled integer value.
fn numeric_text(value: i128, scale: u8) -> String {
    if scale == 0 {
        return value.to_string();
    }
    let digits = value.unsigned_abs().to_string();
    let scale = usize::from(scale);
    let padded =
        if digits.len() <= scale { format!("{}{digits}", "0".repeat(scale + 1 - digits.len())) } else { digits };
    let (int, frac) = padded.split_at(padded.len() - scale);
    format!("{}{int}.{frac}", if value < 0 { "-" } else { "" })
}

/// A TDS value as a grid cell.
pub fn cell(d: &ColumnData<'_>) -> Cell {
    match d {
        ColumnData::U8(v) => v.map(|v| Cell::Int(i64::from(v))).unwrap_or(Cell::Null),
        ColumnData::I16(v) => v.map(|v| Cell::Int(i64::from(v))).unwrap_or(Cell::Null),
        ColumnData::I32(v) => v.map(|v| Cell::Int(i64::from(v))).unwrap_or(Cell::Null),
        ColumnData::I64(v) => v.map(Cell::Int).unwrap_or(Cell::Null),
        ColumnData::F32(v) => v.map(|v| Cell::Float(f64::from(v))).unwrap_or(Cell::Null),
        ColumnData::F64(v) => v.map(Cell::Float).unwrap_or(Cell::Null),
        ColumnData::Bit(v) => v.map(Cell::Bool).unwrap_or(Cell::Null),
        ColumnData::String(v) => v.as_ref().map(|s| Cell::Text(s.to_string())).unwrap_or(Cell::Null),
        ColumnData::Guid(v) => v.map(|g| Cell::Other(g.to_string().to_uppercase())).unwrap_or(Cell::Null),
        ColumnData::Binary(v) => v.as_ref().map(|b| Cell::Bytes(b.to_vec())).unwrap_or(Cell::Null),
        ColumnData::Numeric(v) => v.map(|n| Cell::Other(numeric_text(n.value(), n.scale()))).unwrap_or(Cell::Null),
        ColumnData::Xml(v) => v.as_ref().map(|x| Cell::Text(x.to_string())).unwrap_or(Cell::Null),
        ColumnData::DateTime(v) => v
            .map(|dt| {
                let ms = u64::from(dt.seconds_fragments()) * 1000 / 300;
                Cell::Other(format!("{} {}", date_text(DAY_1900 + i64::from(dt.days())), time_text(ms, 3)))
            })
            .unwrap_or(Cell::Null),
        ColumnData::SmallDateTime(v) => v
            .map(|dt| {
                let mins = u64::from(dt.seconds_fragments());
                Cell::Other(format!("{} {}", date_text(DAY_1900 + i64::from(dt.days())), time_text(mins * 60, 0)))
            })
            .unwrap_or(Cell::Null),
        ColumnData::Time(v) => v.map(|t| Cell::Other(time_text(t.increments(), t.scale()))).unwrap_or(Cell::Null),
        ColumnData::Date(v) => v.map(|d| Cell::Other(date_text(DAY_ONE + i64::from(d.days())))).unwrap_or(Cell::Null),
        ColumnData::DateTime2(v) => v
            .map(|dt| {
                let t = dt.time();
                Cell::Other(format!(
                    "{} {}",
                    date_text(DAY_ONE + i64::from(dt.date().days())),
                    time_text(t.increments(), t.scale())
                ))
            })
            .unwrap_or(Cell::Null),
        ColumnData::DateTimeOffset(v) => v
            .map(|dto| {
                // Stored as UTC; shown in the value's own offset.
                let dt = dto.datetime2();
                let t = dt.time();
                let scale = u32::from(t.scale().min(7));
                let unit = 10u64.pow(scale);
                let offset = i64::from(dto.offset());
                let total = i64::from(dt.date().days()) * 86_400
                    + i64::try_from(t.increments() / unit).unwrap_or(0)
                    + offset * 60;
                let days = total.div_euclid(86_400);
                let secs = u64::try_from(total.rem_euclid(86_400)).unwrap_or(0);
                let local = time_text(secs * unit + t.increments() % unit, t.scale());
                let sign = if offset < 0 { '-' } else { '+' };
                let (oh, om) = (offset.abs() / 60, offset.abs() % 60);
                Cell::Other(format!("{} {local} {sign}{oh:02}:{om:02}", date_text(DAY_ONE + days)))
            })
            .unwrap_or(Cell::Null),
    }
}

/// The SQL type name of a result column.
pub fn type_name(t: ColumnType) -> String {
    match t {
        ColumnType::Null => "null",
        ColumnType::Bit | ColumnType::Bitn => "bit",
        ColumnType::Int1 => "tinyint",
        ColumnType::Int2 => "smallint",
        ColumnType::Int4 | ColumnType::Intn => "int",
        ColumnType::Int8 => "bigint",
        ColumnType::Datetime4 => "smalldatetime",
        ColumnType::Float4 => "real",
        ColumnType::Float8 | ColumnType::Floatn => "float",
        ColumnType::Money | ColumnType::Money4 => "money",
        ColumnType::Datetime | ColumnType::Datetimen => "datetime",
        ColumnType::Guid => "uniqueidentifier",
        ColumnType::Decimaln => "decimal",
        ColumnType::Numericn => "numeric",
        ColumnType::Daten => "date",
        ColumnType::Timen => "time",
        ColumnType::Datetime2 => "datetime2",
        ColumnType::DatetimeOffsetn => "datetimeoffset",
        ColumnType::BigVarBin => "varbinary",
        ColumnType::BigVarChar => "varchar",
        ColumnType::BigBinary => "binary",
        ColumnType::BigChar => "char",
        ColumnType::NVarchar => "nvarchar",
        ColumnType::NChar => "nchar",
        ColumnType::Xml => "xml",
        ColumnType::Udt => "udt",
        ColumnType::Text => "text",
        ColumnType::Image => "image",
        ColumnType::NText => "ntext",
        ColumnType::SSVariant => "sql_variant",
    }
    .to_string()
}

/// `nvarchar(50)`, `decimal(10,2)`, `varchar(max)` from sys.columns.
pub fn column_type(name: &str, max_length: i64, precision: i64, scale: i64) -> String {
    match name {
        "varchar" | "char" | "varbinary" | "binary" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!("{name}({max_length})")
            }
        }
        "nvarchar" | "nchar" => {
            if max_length < 0 {
                format!("{name}(max)")
            } else {
                format!("{name}({})", max_length / 2)
            }
        }
        "decimal" | "numeric" => format!("{name}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" if scale != 7 => format!("{name}({scale})"),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------- statements

/// How streaming a batch's rows ended (owned: the client is free again).
enum Streamed {
    Done(ExecSummary),
    /// The sink wanted no more rows: the server must be told to stop.
    Stopped(ExecSummary),
    Failed(tiberius::error::Error),
    Cancelled,
}

/// Run one statement. Returns the summary, or the error and whether the connection broke.
#[allow(clippy::too_many_arguments)]
async fn run(
    client: &mut Tds,
    sql: &str,
    params: &[&dyn tiberius::ToSql],
    page_size: usize,
    sink: &mut dyn ResultSink,
    flag: &AtomicBool,
    started: Instant,
    batch_only: bool,
) -> Result<ExecSummary, (DbError, bool)> {
    let fail = |e: tiberius::error::Error| {
        let f = fatal(&e);
        (err(e), f)
    };
    if !batch_only && plain_dml(sql) {
        let result = race(client.execute(sql, params), flag).await.map(|r| r.map(|r| r.total()));
        return match result {
            Some(Ok(total)) => {
                sink.columns(&[]);
                let pager = Pager::new(sink, page_size, started);
                Ok(pager.finish(Vec::new(), Some(total)))
            }
            Some(Err(e)) => Err(fail(e)),
            None => Err(cancelled(client).await),
        };
    }
    match stream_batch(client, sql, params, page_size, sink, flag, started).await {
        Streamed::Done(s) => Ok(s),
        Streamed::Stopped(s) => {
            // Stop the server from producing the rest (the next request would drain it all).
            match tokio::time::timeout(Duration::from_secs(30), client.cancel_query()).await {
                Ok(Ok(())) => Ok(s),
                _ => Err((DbError::Query("could not stop the query; the connection is reopened".into()), true)),
            }
        }
        Streamed::Failed(e) => Err(fail(e)),
        Streamed::Cancelled => Err(cancelled(client).await),
    }
}

/// The result sets of a batch, handed to the sink one after another: the first like any
/// statement's rows; each later one only when the sink asks for it ([`ResultSink::next_result`]),
/// else its rows are read and dropped.
pub(crate) struct BatchResults<'a> {
    page_size: usize,
    started: Instant,
    /// The sink while no result set is being paged.
    slot: Option<&'a mut dyn ResultSink>,
    pager: Option<Pager<'a>>,
    columns: Vec<Column>,
    /// Summaries of the finished result sets.
    done: Vec<ExecSummary>,
    /// The sink declined a later result set: the rest of the batch is drained.
    ignore_rest: bool,
}

impl<'a> BatchResults<'a> {
    pub(crate) fn new(sink: &'a mut dyn ResultSink, page_size: usize, started: Instant) -> Self {
        Self {
            page_size,
            started,
            slot: Some(sink),
            pager: None,
            columns: Vec::new(),
            done: Vec::new(),
            ignore_rest: false,
        }
    }

    /// A result set starts.
    pub(crate) fn metadata(&mut self, cols: Vec<Column>) {
        if self.ignore_rest {
            return;
        }
        let sink = match self.pager.take() {
            Some(p) => {
                let (summary, sink) = p.finish_keep(std::mem::take(&mut self.columns), None);
                self.done.push(summary);
                if !sink.next_result() {
                    self.ignore_rest = true;
                    self.slot = Some(sink);
                    return;
                }
                sink
            }
            None => match self.slot.take() {
                Some(s) => s,
                None => return,
            },
        };
        sink.columns(&cols);
        self.pager = Some(Pager::new(sink, self.page_size, self.started));
        self.columns = cols;
    }

    /// A row of the current result set; false when the sink wants no more rows.
    pub(crate) fn row(&mut self, cells: Vec<Cell>) -> bool {
        if self.ignore_rest {
            return true;
        }
        match self.pager.as_mut() {
            Some(p) => p.push(cells),
            None => true,
        }
    }

    /// The batch ended (or stopped). Returns the summary of all result sets (the last one's
    /// columns, every fetched row counted) and whether the sink stopped early.
    pub(crate) fn finish(mut self) -> (ExecSummary, bool) {
        let stopped = self.pager.as_ref().is_some_and(Pager::stopped);
        match self.pager.take() {
            Some(p) => {
                let cols = std::mem::take(&mut self.columns);
                self.done.push(p.finish(cols, None));
            }
            None if self.done.is_empty() => {
                if let Some(s) = self.slot.take() {
                    s.columns(&[]);
                    self.done.push(Pager::new(s, self.page_size, self.started).finish(Vec::new(), Some(0)));
                }
            }
            None => {}
        }
        let fetched = self.done.iter().map(|d| d.rows_fetched).sum();
        let complete = self.done.iter().all(|d| d.complete);
        let mut last = self.done.pop().unwrap_or_default();
        last.rows_fetched = fetched;
        last.complete = complete && !stopped;
        last.elapsed_ms = crate::elapsed_ms(self.started);
        (last, stopped)
    }
}

async fn stream_batch(
    client: &mut Tds,
    sql: &str,
    params: &[&dyn tiberius::ToSql],
    page_size: usize,
    sink: &mut dyn ResultSink,
    flag: &AtomicBool,
    started: Instant,
) -> Streamed {
    // With parameters the statement runs through sp_executesql (RPC); without, as a batch.
    let opened = if params.is_empty() {
        race(client.simple_query(sql), flag).await
    } else {
        race(client.query(sql, params), flag).await
    };
    let mut stream = match opened {
        Some(Ok(s)) => s,
        Some(Err(e)) => return Streamed::Failed(e),
        None => return Streamed::Cancelled,
    };
    let mut batch = BatchResults::new(sink, page_size, started);
    loop {
        let item = match race(stream.try_next(), flag).await {
            Some(Ok(Some(i))) => i,
            Some(Ok(None)) => break,
            Some(Err(e)) => return Streamed::Failed(e),
            None => return Streamed::Cancelled,
        };
        match item {
            QueryItem::Metadata(m) => {
                let cols: Vec<Column> = m
                    .columns()
                    .iter()
                    .map(|c| Column { name: c.name().to_string(), type_name: type_name(c.column_type()) })
                    .collect();
                batch.metadata(cols);
            }
            QueryItem::Row(row) => {
                let cells = row.cells().map(|(_, d)| cell(d)).collect();
                if !batch.row(cells) {
                    break;
                }
            }
        }
    }
    let (summary, stopped) = batch.finish();
    if stopped { Streamed::Stopped(summary) } else { Streamed::Done(summary) }
}

/// A bind value: text as `nvarchar` (the server converts it to the column's type: dates,
/// decimals, uniqueidentifier…), integers as `bigint`, floats as `float`, booleans as `bit`,
/// bytes as `varbinary`. Unsigned values above `i64::MAX` go as text (a decimal converts).
fn bind_value(c: &Cell) -> DbResult<ColumnData<'static>> {
    use std::borrow::Cow;
    Ok(match c {
        Cell::Null => ColumnData::String(None),
        Cell::Bool(b) => ColumnData::Bit(Some(*b)),
        Cell::Int(i) => ColumnData::I64(Some(*i)),
        Cell::UInt(u) => match i64::try_from(*u) {
            Ok(i) => ColumnData::I64(Some(i)),
            Err(_) => ColumnData::String(Some(Cow::Owned(u.to_string()))),
        },
        Cell::Float(f) if f.is_finite() => ColumnData::F64(Some(*f)),
        Cell::Float(f) => return Err(DbError::Unsupported(format!("SQL Server cannot store {f}"))),
        Cell::Text(s) | Cell::Other(s) => ColumnData::String(Some(Cow::Owned(s.clone()))),
        Cell::Bytes(b) => ColumnData::Binary(Some(Cow::Owned(b.clone()))),
    })
}

/// After a cancelled request: send the attention signal and drain the acknowledgement.
async fn cancelled(client: &mut Tds) -> (DbError, bool) {
    match tokio::time::timeout(Duration::from_secs(30), client.cancel_query()).await {
        Ok(Ok(())) => (DbError::Cancelled, false),
        // The connection is out of sync: it must be reopened.
        _ => (DbError::Cancelled, true),
    }
}

impl Connection for MssqlConn {
    fn kind(&self) -> DbKind {
        DbKind::SqlServer
    }

    fn server_version(&mut self) -> DbResult<String> {
        let rows = self.strings(
            "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), \
             CAST(SERVERPROPERTY('Edition') AS nvarchar(128))",
            &[],
        )?;
        let r = rows.first().cloned().unwrap_or_default();
        let edition = col(&r, 1);
        let name = if edition.contains("Azure") { "Azure SQL" } else { "SQL Server" };
        Ok(format!("{name} {}{}", col(&r, 0), if edition.is_empty() { String::new() } else { format!(" ({edition})") }))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        self.cancel.clone()
    }

    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary> {
        self.execute_bound(sql, &[], page_size, sink)
    }

    fn execute_bound(
        &mut self,
        sql: &str,
        params: &[Cell],
        page_size: usize,
        sink: &mut dyn ResultSink,
    ) -> DbResult<ExecSummary> {
        let started = Instant::now();
        self.cancel.flag.store(false, Ordering::SeqCst);
        self.client()?;
        let values = params.iter().map(bind_value).collect::<DbResult<Vec<ColumnData<'static>>>>()?;
        let refs: Vec<&dyn tiberius::ToSql> = values.iter().map(|v| v as &dyn tiberius::ToSql).collect();
        let flag = self.cancel.clone();
        let rt = &self.rt;
        let Some(client) = self.client.as_mut() else { return Err(DbError::Query("not connected".into())) };
        let r = rt.block_on(run(client, sql, &refs, page_size, sink, &flag.flag, started, self.showplan));
        self.cancel.flag.store(false, Ordering::SeqCst);
        if r.is_ok() {
            let words = crate::lex::words(sql, crate::Dialect::TSql);
            if words.len() == 3
                && words.first().is_some_and(|w| w == "SET")
                && words.get(1).is_some_and(|w| w == "SHOWPLAN_XML")
            {
                self.showplan = words.get(2).is_some_and(|w| w == "ON");
            }
        }
        r.map_err(|(e, broken)| {
            if broken {
                self.broken = true;
            }
            e
        })
    }

    fn database_info(&mut self) -> DbResult<DatabaseInfo> {
        let databases = self
            .strings("SELECT name FROM sys.databases WHERE HAS_DBACCESS(name) = 1 ORDER BY name", &[])?
            .iter()
            .map(|r| col(r, 0))
            .collect();
        let cur = self.strings("SELECT DB_NAME(), SCHEMA_NAME()", &[])?.first().cloned().unwrap_or_default();
        let schemas = self
            .strings(
                "SELECT name FROM sys.schemas WHERE schema_id < 16384 AND name NOT IN ('sys', 'INFORMATION_SCHEMA', \
                 'guest') ORDER BY name",
                &[],
            )?
            .iter()
            .map(|r| col(r, 0))
            .collect();
        Ok(DatabaseInfo {
            databases,
            current: Some(col(&cur, 0)).filter(|s| !s.is_empty()),
            schemas,
            default_schema: Some(col(&cur, 1)).filter(|s| !s.is_empty()).or(Some("dbo".into())),
        })
    }

    fn tables(&mut self, schema: &str) -> DbResult<Vec<TableInfo>> {
        Ok(self
            .strings(
                "SELECT o.name, o.type FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
                 WHERE s.name = @P1 AND o.type IN ('U', 'V') AND o.is_ms_shipped = 0 ORDER BY o.name",
                &[schema],
            )?
            .iter()
            .map(|r| TableInfo {
                name: col(r, 0),
                kind: if col(r, 1).trim() == "V" { TableKind::View } else { TableKind::Table },
            })
            .collect())
    }

    fn table_details(&mut self, schema: &str, table: &str) -> DbResult<TableDetails> {
        const OBJ: &str = "OBJECT_ID(QUOTENAME(@P1) + '.' + QUOTENAME(@P2))";
        let columns = self
            .strings(
                &format!(
                    "SELECT c.name, t.name, CAST(c.max_length AS int), CAST(c.precision AS int), CAST(c.scale AS int), \
                     c.is_nullable, dc.definition, c.is_identity, \
                     CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id \
                     AND ic.index_id = i.index_id WHERE i.object_id = c.object_id AND i.is_primary_key = 1 \
                     AND ic.column_id = c.column_id) THEN 1 ELSE 0 END \
                     FROM sys.columns c JOIN sys.types t ON t.user_type_id = c.user_type_id \
                     LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id \
                     WHERE c.object_id = {OBJ} ORDER BY c.column_id"
                ),
                &[schema, table],
            )?
            .iter()
            .map(|r| {
                let n = |i: usize| col(r, i).parse::<i64>().unwrap_or(0);
                let mut data_type = column_type(&col(r, 1), n(2), n(3), n(4));
                if col(r, 7) == "true" {
                    data_type.push_str(" identity");
                }
                ColumnInfo {
                    name: col(r, 0),
                    data_type,
                    nullable: col(r, 5) == "true",
                    default: r.get(6).cloned().flatten(),
                    primary_key: col(r, 8) == "1",
                }
            })
            .collect();
        let mut indexes: Vec<IndexInfo> = Vec::new();
        for r in self.strings(
            &format!(
                "SELECT i.name, i.is_unique, i.is_primary_key, c.name FROM sys.indexes i \
                 JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                 JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = {OBJ} AND i.name IS NOT NULL AND ic.is_included_column = 0 \
                 ORDER BY i.name, ic.key_ordinal"
            ),
            &[schema, table],
        )? {
            let name = col(&r, 0);
            if indexes.last().map(|i| &i.name) != Some(&name) {
                indexes.push(IndexInfo {
                    unique: col(&r, 1) == "true",
                    primary: col(&r, 2) == "true",
                    name,
                    columns: Vec::new(),
                });
            }
            if let Some(i) = indexes.last_mut() {
                i.columns.push(col(&r, 3));
            }
        }
        let mut foreign_keys: Vec<ForeignKeyInfo> = Vec::new();
        for r in self.strings(
            &format!(
                "SELECT fk.name, pc.name, SCHEMA_NAME(rt.schema_id), rt.name, rc.name FROM sys.foreign_keys fk \
                 JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                 JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                 JOIN sys.objects rt ON rt.object_id = fkc.referenced_object_id \
                 JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                 WHERE fk.parent_object_id = {OBJ} ORDER BY fk.name, fkc.constraint_column_id"
            ),
            &[schema, table],
        )? {
            let name = col(&r, 0);
            if foreign_keys.last().map(|f| &f.name) != Some(&name) {
                foreign_keys.push(ForeignKeyInfo {
                    name,
                    ref_schema: r.get(2).cloned().flatten(),
                    ref_table: col(&r, 3),
                    ..Default::default()
                });
            }
            if let Some(f) = foreign_keys.last_mut() {
                f.columns.push(col(&r, 1));
                f.ref_columns.push(col(&r, 4));
            }
        }
        Ok(TableDetails { columns, indexes, foreign_keys })
    }

    fn row_count(&mut self, schema: &str, table: &str) -> DbResult<u64> {
        let sql = format!("SELECT COUNT_BIG(*) FROM {}", crate::sql::qualified(DbKind::SqlServer, schema, table));
        let rows = self.strings(&sql, &[])?;
        Ok(rows.first().map(|r| col(r, 0)).and_then(|s| s.parse().ok()).unwrap_or(0))
    }

    /// `@@TRANCOUNT` says whether a transaction is open; `XACT_STATE() = -1` that it can only be
    /// rolled back.
    fn transaction_status(&mut self) -> crate::tx::TxStatus {
        use crate::tx::TxStatus;
        if self.broken {
            return TxStatus::Unknown;
        }
        match self.strings("SELECT @@TRANCOUNT, XACT_STATE()", &[]) {
            Ok(rows) => {
                let r = rows.first().cloned().unwrap_or_default();
                match (col(&r, 0).parse::<i64>().unwrap_or(0), col(&r, 1).as_str()) {
                    (_, "-1") => TxStatus::Failed,
                    (n, _) if n > 0 => TxStatus::Active,
                    _ => TxStatus::Idle,
                }
            }
            Err(_) => TxStatus::Unknown,
        }
    }

    fn is_broken(&mut self) -> bool {
        self.broken
    }

    fn ping(&mut self) -> DbResult<()> {
        self.strings("SELECT 1", &[]).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_values_are_typed() {
        use std::borrow::Cow;
        let hostile = "it's ]] \\ \0 🦀 ' OR 1=1 --";
        assert_eq!(bind_value(&Cell::Text(hostile.into())).unwrap(), ColumnData::String(Some(Cow::Owned(hostile.into()))));
        assert_eq!(bind_value(&Cell::Bytes(vec![0, 255])).unwrap(), ColumnData::Binary(Some(Cow::Owned(vec![0, 255]))));
        assert_eq!(bind_value(&Cell::Int(i64::MIN)).unwrap(), ColumnData::I64(Some(i64::MIN)));
        assert_eq!(bind_value(&Cell::UInt(u64::MAX)).unwrap(), ColumnData::String(Some(Cow::Owned(u64::MAX.to_string()))));
        assert_eq!(bind_value(&Cell::UInt(7)).unwrap(), ColumnData::I64(Some(7)));
        assert_eq!(bind_value(&Cell::Bool(false)).unwrap(), ColumnData::Bit(Some(false)));
        assert_eq!(bind_value(&Cell::Float(0.1)).unwrap(), ColumnData::F64(Some(0.1)));
        assert!(bind_value(&Cell::Float(f64::INFINITY)).is_err());
    }

    #[test]
    fn dates_and_times_format() {
        assert_eq!(date_text(0), "1970-01-01");
        assert_eq!(date_text(DAY_ONE), "0001-01-01");
        assert_eq!(date_text(DAY_1900), "1900-01-01");
        assert_eq!(date_text(DAY_ONE + 3_652_058), "9999-12-31");
        assert_eq!(date_text(19_723), "2024-01-01");
        assert_eq!(time_text(0, 7), "00:00:00.0000000");
        assert_eq!(time_text(45_296_123, 3), "12:34:56.123");
        assert_eq!(time_text(86_399, 0), "23:59:59");
        assert_eq!(time_text(u64::MAX, 200), time_text(u64::MAX, 7));
        let dt = tiberius::time::DateTime::new(45_290, 300 * 3600 + 150);
        assert_eq!(cell(&ColumnData::DateTime(Some(dt))), Cell::Other("2024-01-01 01:00:00.500".into()));
        let sdt = tiberius::time::SmallDateTime::new(45_290, 61);
        assert_eq!(cell(&ColumnData::SmallDateTime(Some(sdt))), Cell::Other("2024-01-01 01:01:00".into()));
        let d = tiberius::time::Date::new(738_885);
        assert_eq!(cell(&ColumnData::Date(Some(d))), Cell::Other("2024-01-01".into()));
        let t = tiberius::time::Time::new(10_000_000 * 61, 7);
        let dt2 = tiberius::time::DateTime2::new(d, t);
        assert_eq!(cell(&ColumnData::DateTime2(Some(dt2))), Cell::Other("2024-01-01 00:01:01.0000000".into()));
        let dto = tiberius::time::DateTimeOffset::new(dt2, -90);
        assert_eq!(
            cell(&ColumnData::DateTimeOffset(Some(dto))),
            Cell::Other("2023-12-31 22:31:01.0000000 -01:30".into())
        );
        // Hostile values never panic.
        let _ = cell(&ColumnData::Date(Some(tiberius::time::Date::new(u32::MAX >> 8))));
        let _ = cell(&ColumnData::DateTime(Some(tiberius::time::DateTime::new(i32::MIN, u32::MAX))));
        let _ = cell(&ColumnData::Time(Some(tiberius::time::Time::new(u64::MAX, 255))));
    }

    #[test]
    fn values_and_types() {
        assert_eq!(numeric_text(12345, 2), "123.45");
        assert_eq!(numeric_text(-5, 3), "-0.005");
        assert!(numeric_text(i128::MIN, 38).starts_with('-'));
        assert_eq!(numeric_text(7, 0), "7");
        assert_eq!(cell(&ColumnData::I32(None)), Cell::Null);
        assert_eq!(cell(&ColumnData::Bit(Some(true))), Cell::Bool(true));
        assert_eq!(cell(&ColumnData::String(Some("x".into()))), Cell::Text("x".into()));
        assert_eq!(column_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(column_type("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(column_type("decimal", 9, 10, 2), "decimal(10,2)");
        assert_eq!(column_type("datetime2", 8, 27, 7), "datetime2");
        assert_eq!(column_type("int", 4, 10, 0), "int");
        assert_eq!(type_name(ColumnType::NVarchar), "nvarchar");
        assert!(plain_dml("update t set a = 1"));
        assert!(!plain_dml("UPDATE t SET a = 1 OUTPUT inserted.a"));
        assert!(!plain_dml("SELECT 1"));
    }

    struct Parts {
        parts: Vec<(Vec<Column>, Vec<Vec<Cell>>)>,
        take_more: bool,
        stop_after: Option<usize>,
        rows: usize,
    }

    impl ResultSink for Parts {
        fn columns(&mut self, columns: &[Column]) {
            self.parts.push((columns.to_vec(), Vec::new()));
        }
        fn page(&mut self, page: crate::Page) -> bool {
            self.rows += page.rows.len();
            if let Some(p) = self.parts.last_mut() {
                p.1.extend(page.rows);
            }
            self.stop_after.is_none_or(|n| self.rows < n)
        }
        fn next_result(&mut self) -> bool {
            self.take_more
        }
    }

    fn col(name: &str) -> Column {
        Column { name: name.into(), type_name: "int".into() }
    }

    #[test]
    fn every_result_set_of_a_batch_reaches_the_sink() {
        let mut sink = Parts { parts: Vec::new(), take_more: true, stop_after: None, rows: 0 };
        let mut b = BatchResults::new(&mut sink, 2, Instant::now());
        b.metadata(vec![col("a")]);
        for i in 0..3 {
            assert!(b.row(vec![Cell::Int(i)]));
        }
        b.metadata(vec![col("b"), col("c")]);
        assert!(b.row(vec![Cell::Int(9), Cell::Int(8)]));
        b.metadata(vec![col("empty")]);
        let (sum, stopped) = b.finish();
        assert!(!stopped && sum.complete);
        assert_eq!(sum.rows_fetched, 4);
        assert_eq!(sum.columns, vec![col("empty")]);
        assert_eq!(sink.parts.len(), 3);
        assert_eq!(sink.parts[0].1.len(), 3);
        assert_eq!(sink.parts[1].0.len(), 2);
        assert!(sink.parts[2].1.is_empty());

        // A sink that keeps one result: later result sets are drained, not shown.
        let mut one = Parts { parts: Vec::new(), take_more: false, stop_after: None, rows: 0 };
        let mut b = BatchResults::new(&mut one, 10, Instant::now());
        b.metadata(vec![col("a")]);
        assert!(b.row(vec![Cell::Int(1)]));
        b.metadata(vec![col("b")]);
        assert!(b.row(vec![Cell::Int(2)]));
        let (sum, _) = b.finish();
        assert_eq!(one.parts.len(), 1);
        assert_eq!(sum.rows_fetched, 1);

        // No result set at all (DDL, SET …).
        let mut none = Parts { parts: Vec::new(), take_more: true, stop_after: None, rows: 0 };
        let (sum, stopped) = BatchResults::new(&mut none, 10, Instant::now()).finish();
        assert!(!stopped && sum.columns.is_empty() && sum.rows_affected == Some(0));
        assert_eq!(none.parts.len(), 1);

        // The sink stops in the first result set: the batch stops there.
        let mut stop = Parts { parts: Vec::new(), take_more: true, stop_after: Some(2), rows: 0 };
        let mut b = BatchResults::new(&mut stop, 2, Instant::now());
        b.metadata(vec![col("a")]);
        let mut pushed = 0;
        for i in 0..10 {
            if !b.row(vec![Cell::Int(i)]) {
                break;
            }
            pushed += 1;
        }
        assert!(pushed < 10);
        let (sum, stopped) = b.finish();
        assert!(stopped && !sum.complete);
    }

    #[test]
    fn config_options() {
        let mut cfg = DataSourceConfig {
            kind: DbKind::SqlServer,
            host: "db".into(),
            user: "sa".into(),
            database: "app".into(),
            ..Default::default()
        };
        let c = config(&cfg, Some("pw")).unwrap();
        assert_eq!(c.get_addr(), "db:1433");
        cfg.options.insert("encrypt".into(), "bogus".into());
        assert!(config(&cfg, None).is_err());
        cfg.options.insert("encrypt".into(), "off".into());
        cfg.options.insert("trust_server_certificate".into(), "true".into());
        assert!(config(&cfg, None).is_ok());
        cfg.host = "db\\SQLEXPRESS".into();
        assert!(config(&cfg, None).is_err());
    }
}
