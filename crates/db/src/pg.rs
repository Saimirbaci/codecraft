//! PostgreSQL through the `postgres` crate. Rows stream with backpressure; values are decoded
//! from the binary protocol into [`Cell`]s (unknown types fall back to text or hex).

use std::error::Error as StdError;
use std::sync::Arc;
use std::time::Instant;

use postgres::error::SqlState;
use postgres::fallible_iterator::FallibleIterator;
use postgres::types::{FromSql, Kind, ToSql, Type};
use postgres::{CancelToken, Client, NoTls};

use crate::sql::count_rows;
use crate::{
    Canceller, Cell, Column, ColumnInfo, Connection, DataSourceConfig, DatabaseInfo, DbError, DbKind, DbResult,
    ExecSummary, ForeignKeyInfo, IndexInfo, Pager, ResultSink, SslMode, TableDetails, TableInfo, TableKind, TlsFiles,
};

#[derive(Clone)]
enum Tls {
    None,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    Native(postgres_native_tls::MakeTlsConnector),
}

pub(crate) struct PgConn {
    client: Client,
    tls: Tls,
}

struct PgCancel {
    token: CancelToken,
    tls: Tls,
}

impl Canceller for PgCancel {
    fn cancel(&self) -> DbResult<()> {
        cancel(&self.token, &self.tls)
    }
}

fn cancel(token: &CancelToken, tls: &Tls) -> DbResult<()> {
    let r = match tls {
        Tls::None => token.cancel_query(NoTls),
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        Tls::Native(t) => token.cancel_query(t.clone()),
    };
    r.map_err(|e| DbError::Query(format!("cancel failed: {e}")))
}

fn err(e: postgres::Error) -> DbError {
    if let Some(db) = e.as_db_error() {
        if *db.code() == SqlState::QUERY_CANCELED {
            return DbError::Cancelled;
        }
        let mut msg = format!("{}: {}", db.severity(), db.message());
        if let Some(d) = db.detail() {
            msg.push_str(&format!("\n  Detail: {d}"));
        }
        if let Some(h) = db.hint() {
            msg.push_str(&format!("\n  Hint: {h}"));
        }
        if let Some(postgres::error::ErrorPosition::Original(p)) = db.position() {
            msg.push_str(&format!("\n  Position: {p}"));
        }
        return DbError::Query(msg);
    }
    DbError::Query(e.to_string())
}

impl PgConn {
    pub(crate) fn open(cfg: &DataSourceConfig, password: Option<&str>) -> DbResult<Self> {
        let mut pc = postgres::Config::new();
        pc.host(cfg.host.trim()).port(cfg.port_or_default()).connect_timeout(cfg.connect_timeout());
        pc.application_name("CodeCraft");
        if !cfg.user.is_empty() {
            pc.user(&cfg.user);
        } else if let Ok(u) = std::env::var("USER") {
            pc.user(&u);
        }
        if !cfg.database.is_empty() {
            pc.dbname(&cfg.database);
        }
        if let Some(p) = password.filter(|p| !p.is_empty()) {
            pc.password(p);
        }
        let mode = cfg.ssl_mode()?;
        let tls = make_tls(mode, &cfg.tls_files(None)?)?;
        let connect_err = |e: postgres::Error| {
            let mut msg = e.to_string();
            if let Some(src) = e.source() {
                let s = src.to_string();
                if !msg.contains(&s) {
                    msg = format!("{msg}: {s}");
                }
            }
            DbError::Connect(msg)
        };
        let client = match &tls {
            Tls::None => {
                pc.ssl_mode(postgres::config::SslMode::Disable);
                pc.connect(NoTls).map_err(connect_err)?
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Tls::Native(t) => {
                pc.ssl_mode(if mode == SslMode::Prefer {
                    postgres::config::SslMode::Prefer
                } else {
                    postgres::config::SslMode::Require
                });
                pc.connect(t.clone()).map_err(connect_err)?
            }
        };
        Ok(Self { client, tls })
    }

    fn rows(&mut self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> DbResult<Vec<postgres::Row>> {
        self.client.query(sql, params).map_err(err)
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn make_tls(mode: SslMode, files: &TlsFiles) -> DbResult<Tls> {
    if mode == SslMode::Disable {
        return Ok(Tls::None);
    }
    let read = |p: &std::path::Path, what: &str| {
        // Certificates and keys are small; refuse anything that is clearly not one.
        match std::fs::metadata(p) {
            Ok(m) if m.len() > 1 << 20 => {
                return Err(DbError::Config(format!("{what} {} is too large", p.display())));
            }
            _ => {}
        }
        std::fs::read(p).map_err(|e| DbError::Config(format!("cannot read {what} {}: {e}", p.display())))
    };
    let mut b = native_tls::TlsConnector::builder();
    match mode {
        // libpq semantics: prefer/require encrypt but don't verify the server certificate…
        SslMode::Prefer | SslMode::Require => {
            b.danger_accept_invalid_certs(true).danger_accept_invalid_hostnames(true);
        }
        // …verify-ca checks the chain only, verify-full the host name too.
        SslMode::VerifyCa => {
            b.danger_accept_invalid_hostnames(true);
        }
        SslMode::Verify | SslMode::Disable => {}
    }
    if let Some(p) = &files.root_cert {
        let pem = read(p, "root certificate")?;
        let cert = native_tls::Certificate::from_pem(&pem)
            .map_err(|e| DbError::Config(format!("{} is not a PEM certificate: {e}", p.display())))?;
        b.add_root_certificate(cert);
    }
    if let (Some(c), Some(k)) = (&files.client_cert, &files.client_key) {
        let cert = read(c, "client certificate")?;
        let key = read(k, "client key")?;
        let id = native_tls::Identity::from_pkcs8(&cert, &key).map_err(|e| {
            DbError::Config(format!("client certificate {} / key {}: {e} (PEM, PKCS#8 key)", c.display(), k.display()))
        })?;
        b.identity(id);
    }
    let connector = b.build().map_err(|e| DbError::Connect(format!("TLS setup failed: {e}")))?;
    Ok(Tls::Native(postgres_native_tls::MakeTlsConnector::new(connector)))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn make_tls(mode: SslMode, _files: &TlsFiles) -> DbResult<Tls> {
    match mode {
        SslMode::Disable | SslMode::Prefer => Ok(Tls::None),
        SslMode::Require | SslMode::Verify | SslMode::VerifyCa => {
            Err(DbError::Unsupported("TLS not supported yet on this platform (sslmode=require)".into()))
        }
    }
}

fn get_string(row: &postgres::Row, i: usize) -> String {
    row.try_get::<_, Option<String>>(i).ok().flatten().unwrap_or_default()
}

impl Connection for PgConn {
    fn kind(&self) -> DbKind {
        DbKind::Postgres
    }

    fn server_version(&mut self) -> DbResult<String> {
        let rows = self.rows("SELECT current_setting('server_version')", &[])?;
        Ok(format!("PostgreSQL {}", rows.first().map(|r| get_string(r, 0)).unwrap_or_default()))
    }

    fn canceller(&self) -> Arc<dyn Canceller> {
        Arc::new(PgCancel { token: self.client.cancel_token(), tls: self.tls.clone() })
    }

    fn execute(&mut self, sql: &str, page_size: usize, sink: &mut dyn ResultSink) -> DbResult<ExecSummary> {
        let started = Instant::now();
        let stmt = self.client.prepare(sql).map_err(err)?;
        let columns: Vec<Column> = stmt
            .columns()
            .iter()
            .map(|c| Column { name: c.name().to_string(), type_name: c.type_().name().to_string() })
            .collect();
        sink.columns(&columns);
        let n = columns.len();
        let mut pager = Pager::new(sink, page_size, started);
        let mut affected = None;
        let mut abandoned = false;
        let token = self.client.cancel_token();
        {
            let mut it = self.client.query_raw(&stmt, std::iter::empty::<&(dyn ToSql + Sync)>()).map_err(err)?;
            while let Some(row) = it.next().map_err(err)? {
                let cells = (0..n)
                    .map(|i| {
                        row.try_get::<_, PgValue>(i).map(|v| v.0).unwrap_or_else(|e| Cell::Other(format!("<{e}>")))
                    })
                    .collect();
                if !pager.push(cells) {
                    abandoned = true;
                    break;
                }
            }
            if abandoned {
                // Stop the server from streaming the rest before the iterator drains it.
                let _ = cancel(&token, &self.tls);
            } else if n == 0 {
                affected = it.rows_affected();
            }
        }
        Ok(pager.finish(columns, affected))
    }

    fn database_info(&mut self) -> DbResult<DatabaseInfo> {
        let databases = self
            .rows(
                "SELECT datname::text FROM pg_database WHERE NOT datistemplate AND datallowconn ORDER BY datname",
                &[],
            )?
            .iter()
            .map(|r| get_string(r, 0))
            .collect();
        let current = self.rows("SELECT current_database()::text, current_schema()::text", &[])?;
        let schemas = self
            .rows(
                "SELECT nspname::text FROM pg_namespace WHERE nspname NOT LIKE 'pg\\_%' \
                 AND nspname <> 'information_schema' ORDER BY nspname",
                &[],
            )?
            .iter()
            .map(|r| get_string(r, 0))
            .collect();
        let default_schema = current.first().map(|r| get_string(r, 1)).filter(|s| !s.is_empty());
        Ok(DatabaseInfo {
            databases,
            current: current.first().map(|r| get_string(r, 0)),
            schemas,
            default_schema: default_schema.or_else(|| Some("public".into())),
        })
    }

    fn tables(&mut self, schema: &str) -> DbResult<Vec<TableInfo>> {
        let rows = self.rows(
            "SELECT c.relname::text, c.relkind::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = $1 AND c.relkind IN ('r', 'v', 'm', 'p', 'f') AND NOT c.relispartition \
             ORDER BY c.relname",
            &[&schema],
        )?;
        Ok(rows
            .iter()
            .map(|r| TableInfo {
                name: get_string(r, 0),
                kind: match get_string(r, 1).as_str() {
                    "v" => TableKind::View,
                    "m" => TableKind::MaterializedView,
                    "f" => TableKind::Foreign,
                    _ => TableKind::Table,
                },
            })
            .collect())
    }

    fn table_details(&mut self, schema: &str, table: &str) -> DbResult<TableDetails> {
        let columns = self
            .rows(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull, \
                        pg_get_expr(d.adbin, d.adrelid), \
                        EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.oid AND i.indisprimary \
                                AND a.attnum = ANY(i.indkey)) \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                &[&schema, &table],
            )?
            .iter()
            .map(|r| ColumnInfo {
                name: get_string(r, 0),
                data_type: get_string(r, 1),
                nullable: r.try_get::<_, bool>(2).unwrap_or(true),
                default: r.try_get::<_, Option<String>>(3).ok().flatten(),
                primary_key: r.try_get::<_, bool>(4).unwrap_or(false),
            })
            .collect();
        let indexes = self
            .rows(
                "SELECT ic.relname::text, i.indisunique, i.indisprimary, \
                        ARRAY(SELECT pg_get_indexdef(i.indexrelid, k + 1, true) \
                              FROM generate_subscripts(i.indkey, 1) AS k ORDER BY k) \
                 FROM pg_index i \
                 JOIN pg_class ic ON ic.oid = i.indexrelid \
                 JOIN pg_class c ON c.oid = i.indrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = $1 AND c.relname = $2 ORDER BY ic.relname",
                &[&schema, &table],
            )?
            .iter()
            .map(|r| IndexInfo {
                name: get_string(r, 0),
                unique: r.try_get::<_, bool>(1).unwrap_or(false),
                primary: r.try_get::<_, bool>(2).unwrap_or(false),
                columns: r.try_get::<_, Vec<String>>(3).unwrap_or_default(),
            })
            .collect();
        let foreign_keys = self
            .rows(
                "SELECT con.conname::text, \
                        ARRAY(SELECT a.attname::text FROM unnest(con.conkey) WITH ORDINALITY k(num, ord) \
                              JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.num ORDER BY k.ord), \
                        fn.nspname::text, fc.relname::text, \
                        ARRAY(SELECT a.attname::text FROM unnest(con.confkey) WITH ORDINALITY k(num, ord) \
                              JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.num ORDER BY k.ord) \
                 FROM pg_constraint con \
                 JOIN pg_class c ON c.oid = con.conrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 JOIN pg_class fc ON fc.oid = con.confrelid \
                 JOIN pg_namespace fn ON fn.oid = fc.relnamespace \
                 WHERE con.contype = 'f' AND n.nspname = $1 AND c.relname = $2 ORDER BY con.conname",
                &[&schema, &table],
            )?
            .iter()
            .map(|r| ForeignKeyInfo {
                name: get_string(r, 0),
                columns: r.try_get::<_, Vec<String>>(1).unwrap_or_default(),
                ref_schema: Some(get_string(r, 2)),
                ref_table: get_string(r, 3),
                ref_columns: r.try_get::<_, Vec<String>>(4).unwrap_or_default(),
            })
            .collect();
        Ok(TableDetails { columns, indexes, foreign_keys })
    }

    fn row_count(&mut self, schema: &str, table: &str) -> DbResult<u64> {
        let rows = self.rows(&count_rows(DbKind::Postgres, schema, table), &[])?;
        let n = rows.first().and_then(|r| r.try_get::<_, i64>(0).ok()).unwrap_or(0);
        Ok(u64::try_from(n).unwrap_or(0))
    }
}

// ---------------------------------------------------------------- binary value decoding

/// Accepts every type; decodes the binary representation into a [`Cell`].
struct PgValue(Cell);

impl<'a> FromSql<'a> for PgValue {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(PgValue(decode(ty, raw)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, Box<dyn StdError + Sync + Send>> {
        Ok(PgValue(Cell::Null))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

fn be<const N: usize>(raw: &[u8], at: usize) -> Option<[u8; N]> {
    raw.get(at..at.checked_add(N)?)?.try_into().ok()
}

fn i16_at(raw: &[u8], at: usize) -> Option<i16> {
    be::<2>(raw, at).map(i16::from_be_bytes)
}

fn i32_at(raw: &[u8], at: usize) -> Option<i32> {
    be::<4>(raw, at).map(i32::from_be_bytes)
}

fn i64_at(raw: &[u8], at: usize) -> Option<i64> {
    be::<8>(raw, at).map(i64::from_be_bytes)
}

fn text(raw: &[u8]) -> Cell {
    Cell::Text(String::from_utf8_lossy(raw).into_owned())
}

pub(crate) fn decode(ty: &Type, raw: &[u8]) -> Cell {
    decode_opt(ty, raw).unwrap_or_else(|| Cell::Bytes(raw.to_vec()))
}

fn decode_opt(ty: &Type, raw: &[u8]) -> Option<Cell> {
    Some(match *ty {
        Type::BOOL => Cell::Bool(*raw.first()? != 0),
        Type::INT2 => Cell::Int(i64::from(i16_at(raw, 0)?)),
        Type::INT4 => Cell::Int(i64::from(i32_at(raw, 0)?)),
        Type::INT8 => Cell::Int(i64_at(raw, 0)?),
        Type::OID | Type::XID | Type::CID | Type::REGPROC | Type::REGCLASS | Type::REGTYPE => {
            Cell::UInt(u64::from(u32::from_be_bytes(be::<4>(raw, 0)?)))
        }
        Type::FLOAT4 => Cell::Float(f64::from(f32::from_be_bytes(be::<4>(raw, 0)?))),
        Type::FLOAT8 => Cell::Float(f64::from_be_bytes(be::<8>(raw, 0)?)),
        Type::NUMERIC => Cell::Other(numeric(raw)?),
        Type::MONEY => {
            let cents = i64_at(raw, 0)?;
            let sign = if cents < 0 { "-" } else { "" };
            let abs = cents.unsigned_abs();
            Cell::Other(format!("{sign}{}.{:02}", abs / 100, abs % 100))
        }
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN | Type::XML | Type::JSON => text(raw),
        Type::CHAR => Cell::Text(char::from(*raw.first()?).to_string()),
        Type::JSONB => text(raw.get(1..)?),
        Type::BYTEA => Cell::Bytes(raw.to_vec()),
        Type::UUID => {
            let b = raw.get(..16)?;
            let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
            Cell::Other(format!(
                "{}-{}-{}-{}-{}",
                h.get(0..8)?,
                h.get(8..12)?,
                h.get(12..16)?,
                h.get(16..20)?,
                h.get(20..32)?
            ))
        }
        Type::DATE => Cell::Other(date(i32_at(raw, 0)?)),
        Type::TIME => Cell::Other(time(i64_at(raw, 0)?)),
        Type::TIMETZ => {
            let off = -i32_at(raw, 8)?;
            Cell::Other(format!("{}{}", time(i64_at(raw, 0)?), offset(off)))
        }
        Type::TIMESTAMP => Cell::Other(timestamp(i64_at(raw, 0)?)),
        Type::TIMESTAMPTZ => Cell::Other(format!("{}+00", timestamp(i64_at(raw, 0)?))),
        Type::INTERVAL => Cell::Other(interval(i64_at(raw, 0)?, i32_at(raw, 8)?, i32_at(raw, 12)?)),
        Type::INET | Type::CIDR => Cell::Other(inet(raw)?),
        _ => match ty.kind() {
            Kind::Array(elem) => Cell::Other(array(elem, raw)?),
            Kind::Domain(base) => decode_opt(base, raw)?,
            Kind::Enum(_) => text(raw),
            _ => match std::str::from_utf8(raw) {
                Ok(s) if !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t' && c != '\r') => {
                    Cell::Text(s.to_string())
                }
                _ => return None,
            },
        },
    })
}

fn numeric(raw: &[u8]) -> Option<String> {
    let ndigits = usize::from(u16::from_be_bytes(be::<2>(raw, 0)?));
    let weight = i64::from(i16_at(raw, 2)?);
    let sign = u16::from_be_bytes(be::<2>(raw, 4)?);
    let dscale = usize::from(u16::from_be_bytes(be::<2>(raw, 6)?));
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        _ => {}
    }
    let digit = |i: i64| -> u16 {
        usize::try_from(i)
            .ok()
            .filter(|i| *i < ndigits)
            .and_then(|i| be::<2>(raw, 8 + i * 2))
            .map(u16::from_be_bytes)
            .unwrap_or(0)
    };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                s.push_str(&digit(i).to_string());
            } else {
                s.push_str(&format!("{:04}", digit(i)));
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::new();
        let groups = dscale.div_ceil(4);
        for k in 0..groups {
            frac.push_str(&format!("{:04}", digit(weight + 1 + i64::try_from(k).ok()?)));
        }
        frac.truncate(dscale);
        s.push('.');
        s.push_str(&frac);
    }
    Some(s)
}

/// Days since 1970-01-01 → (year, month, day) (Howard Hinnant's civil_from_days).
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// PostgreSQL dates count from 2000-01-01.
const PG_EPOCH_DAYS: i64 = 10_957;

fn date(days: i32) -> String {
    match days {
        i32::MAX => "infinity".into(),
        i32::MIN => "-infinity".into(),
        d => {
            let (y, m, dd) = civil(i64::from(d) + PG_EPOCH_DAYS);
            if y <= 0 { format!("{:04}-{m:02}-{dd:02} BC", 1 - y) } else { format!("{y:04}-{m:02}-{dd:02}") }
        }
    }
}

fn time(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let base = format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60);
    if frac == 0 {
        base
    } else {
        let f = format!("{frac:06}");
        format!("{base}.{}", f.trim_end_matches('0'))
    }
}

fn offset(secs: i32) -> String {
    let sign = if secs < 0 { '-' } else { '+' };
    let a = secs.unsigned_abs();
    if a.is_multiple_of(3600) {
        format!("{sign}{:02}", a / 3600)
    } else {
        format!("{sign}{:02}:{:02}", a / 3600, (a / 60) % 60)
    }
}

fn timestamp(micros: i64) -> String {
    match micros {
        i64::MAX => "infinity".into(),
        i64::MIN => "-infinity".into(),
        m => {
            let day = m.div_euclid(86_400_000_000);
            let rest = m.rem_euclid(86_400_000_000);
            let d = i32::try_from(day).unwrap_or(0);
            format!("{} {}", date(d), time(rest))
        }
    }
}

fn interval(micros: i64, days: i32, months: i32) -> String {
    let mut parts = Vec::new();
    let (y, mo) = (months / 12, months % 12);
    if y != 0 {
        parts.push(format!("{y} year{}", if y.abs() == 1 { "" } else { "s" }));
    }
    if mo != 0 {
        parts.push(format!("{mo} mon{}", if mo.abs() == 1 { "" } else { "s" }));
    }
    if days != 0 {
        parts.push(format!("{days} day{}", if days.abs() == 1 { "" } else { "s" }));
    }
    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 { "-" } else { "" };
        parts.push(format!("{sign}{}", time(micros.saturating_abs())));
    }
    parts.join(" ")
}

fn inet(raw: &[u8]) -> Option<String> {
    let family = *raw.first()?;
    let bits = *raw.get(1)?;
    let is_cidr = *raw.get(2)? != 0;
    let addr = raw.get(4..)?;
    let (s, full) = match family {
        2 => {
            let b: [u8; 4] = addr.get(..4)?.try_into().ok()?;
            (std::net::Ipv4Addr::from(b).to_string(), 32)
        }
        3 => {
            let b: [u8; 16] = addr.get(..16)?.try_into().ok()?;
            (std::net::Ipv6Addr::from(b).to_string(), 128)
        }
        _ => return None,
    };
    Some(if bits == full && !is_cidr { s } else { format!("{s}/{bits}") })
}

fn array(elem: &Type, raw: &[u8]) -> Option<String> {
    let arr = postgres_protocol::types::array_from_sql(raw).ok()?;
    let mut values = arr.values();
    let mut items = Vec::new();
    while let Ok(Some(v)) = values.next() {
        items.push(match v {
            None => "NULL".to_string(),
            Some(b) => {
                let c = decode(elem, b);
                match c {
                    Cell::Text(s) if s.is_empty() || s.contains([',', '{', '}', '"', ' ']) => {
                        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
                    }
                    other => other.display(),
                }
            }
        });
        if items.len() > 10_000 {
            items.push("…".into());
            break;
        }
    }
    Some(format!("{{{}}}", items.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(ndigits: u16, weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(ndigits.to_be_bytes());
        v.extend(weight.to_be_bytes());
        v.extend(sign.to_be_bytes());
        v.extend(dscale.to_be_bytes());
        for d in digits {
            v.extend(d.to_be_bytes());
        }
        v
    }

    #[test]
    fn numeric_decoding() {
        // 12345.678 = [1, 2345, 6780], weight 1, dscale 3
        assert_eq!(numeric(&num(3, 1, 0, 3, &[1, 2345, 6780])).unwrap(), "12345.678");
        // -0.0012 = [12], weight -1, dscale 4
        assert_eq!(numeric(&num(1, -1, 0x4000, 4, &[12])).unwrap(), "-0.0012");
        // 0.00000001 = [1], weight -2, dscale 8
        assert_eq!(numeric(&num(1, -2, 0, 8, &[1])).unwrap(), "0.00000001");
        assert_eq!(numeric(&num(0, 0, 0, 0, &[])).unwrap(), "0");
        assert_eq!(numeric(&num(1, 2, 0, 0, &[7])).unwrap(), "700000000");
        assert_eq!(numeric(&num(0, 0, 0xC000, 0, &[])).unwrap(), "NaN");
        assert!(numeric(&[1, 2]).is_none());
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(date(0), "2000-01-01");
        assert_eq!(date(-1), "1999-12-31");
        assert_eq!(date(9_190), "2025-02-28");
        assert_eq!(time(3_723_500_000), "01:02:03.5");
        assert_eq!(timestamp(86_400_000_000 + 1_000_000), "2000-01-02 00:00:01");
        assert_eq!(timestamp(-1), "1999-12-31 23:59:59.999999");
        assert_eq!(interval(3_600_000_000, 2, 14), "1 year 2 mons 2 days 01:00:00");
        assert_eq!(interval(0, 0, 0), "00:00:00");
        assert_eq!(offset(-5 * 3600), "-05");
    }

    #[test]
    fn tls_setup_reports_bad_files() {
        assert!(matches!(make_tls(SslMode::Disable, &TlsFiles::default()), Ok(Tls::None)));
        let dir = std::env::temp_dir().join(format!("codecraft-pg-tls-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.pem");
        std::fs::write(&junk, "not a certificate").unwrap();
        let files = TlsFiles { root_cert: Some(junk.clone()), ..TlsFiles::default() };
        let missing = TlsFiles { root_cert: Some(dir.join("missing.pem")), ..TlsFiles::default() };
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            for mode in [SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::Verify] {
                assert!(make_tls(mode, &TlsFiles::default()).is_ok(), "{mode:?}");
            }
            let e = make_tls(SslMode::Verify, &files).err().unwrap().to_string();
            assert!(e.contains("junk.pem"), "{e}");
            let e = make_tls(SslMode::VerifyCa, &missing).err().unwrap().to_string();
            assert!(e.contains("cannot read root certificate"), "{e}");
        } else {
            assert!(make_tls(SslMode::Require, &files).is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hostile_bytes_do_not_panic() {
        for ty in [Type::INT4, Type::NUMERIC, Type::UUID, Type::INET, Type::TIMESTAMP, Type::INTERVAL, Type::INT4_ARRAY]
        {
            for raw in [&[][..], &[0xff][..], &[0, 1, 2][..], &[0xff; 40][..]] {
                let _ = decode(&ty, raw);
            }
        }
        assert_eq!(decode(&Type::INT4, &[0, 0, 0, 7]), Cell::Int(7));
        assert_eq!(decode(&Type::INET, &[2, 32, 0, 4, 10, 0, 0, 1]), Cell::Other("10.0.0.1".into()));
    }
}
