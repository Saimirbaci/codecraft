//! Database commands: data sources, connections, the schema tree, SQL consoles, execution,
//! result sets and schema-aware completion. State lives in [`crate::database`].

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_db::{DataSourceConfig, DbKind, Dialect, split_statements, sql, statement_at};
use codecraft_syntax::Language;
use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, ok, simple, spec};
pub(super) use crate::database::console_worker;
use crate::database::exec::{ExecAction, PendingRun};
use crate::database::{
    CONSOLES_DIR, ConnStatus, DataSourceDialog, DataSourceForm, MetaReq, ResultFilter, ResultSet, RunSpec, SortKey,
    TableRef, node_key, parse_options, sync_project,
};
use crate::params::{bool_or, opt_str, opt_usize, req_path, req_str, req_usize};
use crate::popup::{CompletionItem, CompletionPopup};
use crate::{CommandError, Popup, Session};

mod console;
mod data;
mod edit;
mod grid;
mod import;
pub(crate) mod sql_editor;
mod schema;

pub(crate) fn specs() -> Vec<CommandSpec> {
    let mut v = edit::specs();
    v.extend(console::specs());
    v.extend(grid::specs());
    v.extend(import::specs());
    v.extend(sql_editor::specs());
    v.extend(data::specs());
    v.extend(schema::specs());
    v.extend(vec![
        simple("view.toolWindows.services", "Services", always, |s, _| {
            s.ws.layout.toggle(ToolWindowId::Services);
            ok()
        }),
        spec(
            "db.addDataSource",
            "Add Data Source…",
            None,
            "{name?, kind: sqlite|postgres|mysql|mariadb, host?, port?, database?, user?, password?, \
             savePassword?: bool, path?, options?: {key: value}, readOnly?: bool, color?: red|orange|yellow|green|blue|violet|gray, \
             confirmWrites?: bool (production: confirm data modifications), manualCommit?: bool, \
             sshTunnel?: SSH server name (empty: direct)} — without params opens the dialog",
            Context::Global,
            always,
            add_data_source,
        ),
        spec(
            "db.editDataSource",
            "Edit Data Source…",
            None,
            "{dataSource, …fields as db.addDataSource} — only {dataSource} opens the dialog",
            Context::Global,
            always,
            edit_data_source,
        ),
        spec(
            "db.removeDataSource",
            "Remove Data Source",
            None,
            "{dataSource}",
            Context::Global,
            always,
            remove_data_source,
        ),
        spec(
            "db.testConnection",
            "Test Connection",
            None,
            "{dataSource} or a config like db.addDataSource; without params tests the open dialog",
            Context::Global,
            always,
            test_connection,
        ),
        spec("db.closeDialog", "Close Data Source Dialog", None, "", Context::Global, always, |s, _| {
            s.db.dialog = None;
            ok()
        }),
        spec("db.connect", "Connect", None, "{dataSource?}", Context::Global, always, connect),
        spec(
            "db.disconnect",
            "Disconnect",
            None,
            "{dataSource?} — all data sources when omitted",
            Context::Global,
            always,
            disconnect,
        ),
        spec("db.refresh", "Refresh", None, "{dataSource?}", Context::Global, always, refresh),
        spec(
            "db.expand",
            "Expand Database Node",
            None,
            "{dataSource, schema?, table?, expanded?: bool} — loads the level lazily",
            Context::Global,
            always,
            expand,
        ),
        spec("db.rowCount", "Count Rows", None, "{dataSource, schema, table}", Context::Global, always, row_count),
        spec(
            "db.newConsole",
            "New Query Console",
            None,
            "{dataSource?, new?: bool} — opens the data source's console (a new one with `new`)",
            Context::Global,
            always,
            new_console,
        ),
        spec(
            "db.setDataSource",
            "Set Console Data Source",
            None,
            "{dataSource, path?} — bind an SQL file (the active one by default)",
            Context::Global,
            always,
            set_data_source,
        ),
        spec(
            "db.execute",
            "Execute",
            Some("Cmd+Enter"),
            "{sql?, dataSource?, params?: {name: sqlText}, confirm?: bool} — the selection or the statement at the caret \
             of the active SQL console; statements with parameters (?, $1, :name, ${name}) open the parameter prompt unless \
             `params` has values; on a production data source data modifications ask first unless `confirm`",
            Context::Editor,
            always,
            |s, p| run_sql(s, p, false),
        ),
        spec(
            "db.executeAll",
            "Execute All Statements",
            None,
            "{dataSource?, params?, confirm?} — every statement of the active SQL console",
            Context::Global,
            always,
            |s, p| run_sql(s, p, true),
        ),
        spec("db.cancel", "Cancel Query", None, "{result?}", Context::Global, can_cancel, cancel),
        spec(
            "db.openTable",
            "Open Table Data",
            None,
            "{dataSource, schema, table, orderBy?: column, desc?: bool, where?: sql, order?: sql} — `where`/`order` \
             fill the grid's filter row",
            Context::Global,
            always,
            open_table,
        ),
        spec(
            "db.sortResult",
            "Sort Result",
            None,
            "{result, column: int, desc?: bool} — re-queries with ORDER BY; without `desc` cycles asc → desc → none",
            Context::Global,
            always,
            sort_result,
        ),
        spec("db.fetchMore", "Fetch Next Page", None, "{result?}", Context::Global, always, fetch_more),
        spec(
            "db.selectResult",
            "Show Result",
            None,
            "{result?} — no result shows the Output log",
            Context::Global,
            always,
            |s, p| {
                let id = opt_u64(p, "result")?;
                if let Some(id) = id
                    && s.db.result(id).is_none()
                {
                    return Err(CommandError::BadParams(format!("no result {id}")));
                }
                s.db.active_result = id;
                s.ws.layout.show(ToolWindowId::Services);
                ok()
            },
        ),
        spec("db.closeResult", "Close Result", None, "{result?}", Context::Global, always, close_result),
        spec("db.exportCsv", "Export Result to CSV", None, "{result?, path}", Context::Global, always, export_csv),
        spec(
            "db.getResult",
            "Get Result Rows",
            None,
            "{result?, offset?, limit?} — columns, rows (NULL as null) and status",
            Context::Global,
            always,
            get_result,
        ),
        spec("db.listDataSources", "List Data Sources", None, "", Context::Global, always, list_data_sources),
        spec(
            "db.complete",
            "SQL Completion",
            Some("Ctrl+Space"),
            "Tables and columns in an SQL console; other files get Code › Code Completion › Basic",
            Context::Editor,
            has_editor,
            complete,
        ),
    ]);
    v
}

// ---------------------------------------------------------------- helpers

fn opt_u64(p: &Value, key: &str) -> Result<Option<u64>, CommandError> {
    Ok(opt_usize(p, key)?.and_then(|n| u64::try_from(n).ok()))
}

fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::BadParams(msg.into())
}

fn failed(msg: impl Into<String>) -> CommandError {
    CommandError::Failed(msg.into())
}

/// The data source a command acts on: the param, else the selected tree node's, else the only one.
fn target_source(s: &Session, p: &Value) -> Result<String, CommandError> {
    if let Some(id) = opt_str(p, "dataSource") {
        return s.db.source(id).map(|_| id.to_string()).ok_or_else(|| bad(format!("no data source `{id}`")));
    }
    if let Some(sel) = &s.db.selected {
        let id = sel.split('\u{1f}').next().unwrap_or_default();
        if s.db.source(id).is_some() {
            return Ok(id.to_string());
        }
    }
    if let Some(id) = s.ws.active().and_then(|(_, d)| s.db.source_for_path(d.path.as_deref())) {
        return Ok(id);
    }
    Err(CommandError::BadParams(if s.db.sources.is_empty() {
        "no data sources: add one with db.addDataSource".into()
    } else {
        "missing `dataSource`".into()
    }))
}

fn kind_of(s: &Session, source: &str) -> DbKind {
    s.db.source(source).map(|d| d.config.kind).unwrap_or_default()
}

fn dialect_of(s: &Session, source: Option<&str>) -> Dialect {
    source.and_then(|id| s.db.source(id)).map(|d| d.config.kind.dialect()).unwrap_or(Dialect::Generic)
}

/// Connect the introspection connection (the tree and completion use it).
fn ensure_connected(s: &mut Session, source: &str) -> Result<(), CommandError> {
    let status =
        s.db.source(source).map(|d| d.status.clone()).ok_or_else(|| bad(format!("no data source `{source}`")))?;
    match status {
        ConnStatus::Disconnected | ConnStatus::Failed(_) => s.db.meta(source, MetaReq::Info).map_err(failed),
        ConnStatus::Connecting | ConnStatus::Connected(_) => Ok(()),
    }
}

/// A data source config from command params (`base` supplies missing fields when editing).
fn config_from_params(
    s: &Session,
    p: &Value,
    base: Option<&DataSourceConfig>,
) -> Result<(DataSourceConfig, Option<String>, bool), CommandError> {
    let mut cfg = base.cloned().unwrap_or_default();
    if let Some(k) = p.get("kind") {
        let k = k.as_str().ok_or_else(|| bad("`kind` must be a string"))?;
        cfg.kind = DbKind::parse(k).ok_or_else(|| bad(format!("unknown database kind `{k}`")))?;
    } else if base.is_none() {
        return Err(bad("missing `kind` (sqlite, postgres, mysql or mariadb)"));
    }
    let text = |key: &str| -> Result<Option<String>, CommandError> {
        match p.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(v)) => Ok(Some(v.trim().to_string())),
            Some(_) => Err(bad(format!("`{key}` must be a string"))),
        }
    };
    if let Some(v) = text("name")? {
        cfg.name = v;
    }
    if let Some(v) = text("host")? {
        cfg.host = v;
    }
    if let Some(v) = text("database")? {
        cfg.database = v;
    }
    if let Some(v) = text("user")? {
        cfg.user = v;
    }
    match p.get("port") {
        None | Some(Value::Null) => {}
        Some(Value::String(v)) if v.trim().is_empty() => cfg.port = None,
        Some(v) => {
            let port = v
                .as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .and_then(|n| u16::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or_else(|| bad("`port` must be a number between 1 and 65535"))?;
            cfg.port = Some(port);
        }
    }
    if let Some(v) = text("path")? {
        cfg.path = if v.is_empty() { None } else { Some(relative_to_project(s, Path::new(&v))) };
    }
    match p.get("options") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            cfg.options = m
                .iter()
                .filter(|(k, _)| !k.trim().is_empty())
                .map(|(k, v)| (k.trim().to_string(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                .collect();
        }
        Some(Value::String(o)) => cfg.options = parse_options(o),
        Some(_) => return Err(bad("`options` must be an object")),
    }
    if let Some(v) = text("sshTunnel")? {
        cfg.ssh_tunnel = (!v.is_empty()).then_some(v);
    }
    match p.get("ssh") {
        None => {}
        Some(Value::Null) => cfg.ssh = None,
        Some(Value::String(d)) if d.trim().is_empty() => cfg.ssh = None,
        // `user@host:port` (agent authentication).
        Some(Value::String(d)) => {
            let mut t = codecraft_db::SshTunnelConfig::parse_destination(d)
                .ok_or_else(|| bad(format!("`ssh`: `{d}` is not [user@]host[:port]")))?;
            t.secret_saved = cfg.ssh.as_ref().is_some_and(|o| o.secret_saved);
            cfg.ssh = Some(t);
        }
        Some(Value::Object(m)) => {
            let get = |k: &str| m.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
            let host = get("host");
            if host.is_empty() {
                cfg.ssh = None;
            } else {
                if host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(bad(format!("`ssh.host`: `{host}` is not a host name")));
                }
                // `user@host:port` typed into the host field fills the other fields.
                let parsed = codecraft_db::SshTunnelConfig::parse_destination(&host).unwrap_or_default();
                let port = match m.get("port") {
                    None | Some(Value::Null) => parsed.port,
                    Some(Value::String(v)) if v.trim().is_empty() => parsed.port,
                    Some(v) => Some(
                        v.as_u64()
                            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                            .and_then(|n| u16::try_from(n).ok())
                            .filter(|n| *n > 0)
                            .ok_or_else(|| bad("`ssh.port` must be a number between 1 and 65535"))?,
                    ),
                };
                let auth = match m.get("auth").and_then(Value::as_str) {
                    None => codecraft_db::SshAuth::Agent,
                    Some(a) => codecraft_db::SshAuth::parse(a)
                        .ok_or_else(|| bad(format!("`ssh.auth`: unknown `{a}` (agent, key or password)")))?,
                };
                let user = match get("user") {
                    u if u.is_empty() => parsed.user.clone(),
                    u => u,
                };
                if user.starts_with('-') || user.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(bad(format!("`ssh.user`: `{user}` is not a user name")));
                }
                let host = if parsed.host.is_empty() { host } else { parsed.host };
                let keep = cfg.ssh.as_ref().is_some_and(|o| o.secret_saved && o.auth == auth);
                cfg.ssh = Some(codecraft_db::SshTunnelConfig {
                    host,
                    port,
                    user,
                    auth,
                    identity_file: get("identityFile"),
                    secret_saved: keep,
                });
            }
        }
        Some(_) => return Err(bad("`ssh` must be an object, `user@host:port` or null")),
    }
    cfg.read_only = bool_or(p, "readOnly", cfg.read_only);
    cfg.confirm_writes = bool_or(p, "confirmWrites", cfg.confirm_writes);
    cfg.manual_commit = bool_or(p, "manualCommit", cfg.manual_commit);
    match p.get("color") {
        None => {}
        Some(Value::Null) => cfg.color = None,
        Some(Value::String(c)) if c.trim().is_empty() || c.trim().eq_ignore_ascii_case("none") => cfg.color = None,
        Some(Value::String(c)) => {
            let probe = DataSourceConfig { color: Some(c.clone()), ..Default::default() };
            let name = probe.color_name().ok_or_else(|| {
                bad(format!("unknown colour `{c}` ({})", codecraft_db::DATA_SOURCE_COLORS.join(", ")))
            })?;
            cfg.color = Some(name.to_string());
        }
        Some(_) => return Err(bad("`color` must be a string")),
    }
    if cfg.kind.is_file_based() {
        cfg.host.clear();
        cfg.port = None;
    }
    if cfg.name.trim().is_empty() {
        cfg.name = if cfg.kind.is_file_based() {
            cfg.path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "SQLite".into())
        } else {
            format!("{}@{}", cfg.kind.label(), cfg.host)
        };
    }
    cfg.validate().map_err(|e| bad(e.to_string()))?;
    let password = text("password")?.filter(|p| !p.is_empty());
    let save = bool_or(p, "savePassword", true);
    Ok((cfg, password, save))
}

/// Paths inside the project are stored relative to it (the config stays portable).
fn relative_to_project(s: &Session, path: &Path) -> PathBuf {
    match s.db.project_root() {
        Some(root) if path.is_absolute() => {
            path.strip_prefix(root).map(Path::to_path_buf).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

fn wants_dialog(p: &Value) -> bool {
    match p {
        Value::Null => true,
        Value::Object(m) => m.keys().all(|k| k == "kind" || k == "dataSource"),
        _ => false,
    }
}

fn save(s: &mut Session) -> Result<(), CommandError> {
    s.db.save().map_err(|e| failed(format!("cannot save data sources: {e}")))
}

// ---------------------------------------------------------------- data sources

fn add_data_source(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    if wants_dialog(p) {
        let kind = match p.get("kind") {
            None => DbKind::Postgres,
            Some(k) => k
                .as_str()
                .and_then(DbKind::parse)
                .ok_or_else(|| bad(format!("unknown database kind {k} (sqlite, postgres, mysql or mariadb)")))?,
        };
        s.db.dialog = Some(DataSourceDialog::new(None, DataSourceForm::for_kind(kind)));
        return Ok(json!({ "dialog": true }));
    }
    let (mut cfg, password, save_pw) = config_from_params(s, p, None)?;
    cfg.id = s.db.new_source_id(&cfg.name);
    if let Some(pw) = &password {
        s.db.store_password(&cfg.id, pw, save_pw);
        cfg.password_saved = save_pw;
    }
    store_ssh_secret(s, &mut cfg, p, false);
    let id = cfg.id.clone();
    s.db.sources.push(crate::database::DataSource {
        config: cfg,
        status: ConnStatus::Disconnected,
        tree: Default::default(),
    });
    s.db.selected = Some(node_key(&id, None, None));
    if s.db.dialog.as_ref().is_some_and(|d| d.editing.is_none()) {
        s.db.dialog = None;
    }
    save(s)?;
    s.ws.layout.show(ToolWindowId::Database);
    s.status = format!("Data source {id} added");
    Ok(json!({ "dataSource": id }))
}

fn edit_data_source(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = req_str(p, "dataSource")?.to_string();
    let base = s.db.source(&id).map(|d| d.config.clone()).ok_or_else(|| bad(format!("no data source `{id}`")))?;
    if wants_dialog(p) {
        s.db.dialog = Some(DataSourceDialog::new(Some(id), DataSourceForm::from_config(&base)));
        return Ok(json!({ "dialog": true }));
    }
    let (mut cfg, password, save_pw) = config_from_params(s, p, Some(&base))?;
    cfg.id = id.clone();
    if let Some(pw) = &password {
        s.db.store_password(&id, pw, save_pw);
        cfg.password_saved = save_pw;
    } else if !save_pw && base.password_saved {
        s.db.forget_password(&id, true);
        cfg.password_saved = false;
    }
    let had_secret = base.ssh.as_ref().is_some_and(|t| t.secret_saved);
    let ssh_secret_typed = store_ssh_secret(s, &mut cfg, p, had_secret);
    let reconnect = {
        let mut a = base.clone();
        let mut b = cfg.clone();
        a.name.clear();
        b.name.clear();
        a != b || password.is_some() || ssh_secret_typed
    };
    if reconnect {
        s.db.disconnect(Some(&id));
    }
    if let Some(ds) = s.db.source_mut(&id) {
        ds.config = cfg;
    }
    if s.db.dialog.as_ref().is_some_and(|d| d.editing.as_deref() == Some(id.as_str())) {
        s.db.dialog = None;
    }
    save(s)?;
    Ok(json!({ "dataSource": id }))
}

fn remove_data_source(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = target_source(s, p)?;
    s.db.disconnect(Some(&id));
    let had_password = s.db.source(&id).is_some_and(|d| d.config.password_saved);
    s.db.forget_password(&id, had_password);
    s.db.sources.retain(|d| d.config.id != id);
    s.db.bindings.retain(|_, v| *v != id);
    s.db.results.retain(|r| r.source != id);
    if s.db.active_result.is_some_and(|a| s.db.result(a).is_none()) {
        s.db.active_result = None;
    }
    let prefix = node_key(&id, None, None);
    s.db.expanded.retain(|k| k != &prefix && !k.starts_with(&format!("{prefix}\u{1f}")));
    if s.db.selected.as_deref().is_some_and(|k| k == prefix || k.starts_with(&format!("{prefix}\u{1f}"))) {
        s.db.selected = None;
    }
    save(s)?;
    s.status = format!("Data source {id} removed");
    ok()
}

/// The SSH password or passphrase from `sshPassword` (with `saveSshPassword`, default true):
/// remembered for the session and saved in the keychain. An untyped one keeps the saved one,
/// unless saving was turned off or the tunnel no longer needs a secret. Returns whether one was
/// typed.
fn store_ssh_secret(s: &mut Session, cfg: &mut DataSourceConfig, p: &Value, had_secret: bool) -> bool {
    let account = cfg.ssh_secret_account();
    let typed = p.get("sshPassword").and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    let save = bool_or(p, "saveSshPassword", true);
    let needs = cfg.ssh.as_ref().is_some_and(|t| t.auth != codecraft_db::SshAuth::Agent);
    let Some(t) = cfg.ssh.as_mut().filter(|_| needs) else {
        if had_secret {
            s.db.forget_password(&account, true);
        }
        return false;
    };
    match typed {
        Some(secret) => {
            s.db.store_password(&account, &secret, save);
            t.secret_saved = save;
            true
        }
        None if !save && t.secret_saved => {
            s.db.forget_password(&account, true);
            t.secret_saved = false;
            false
        }
        None => false,
    }
}

fn test_connection(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let (cfg, password) = if p.is_null() || p.as_object().is_some_and(|m| m.is_empty()) {
        let d = s.db.dialog.as_ref().ok_or_else(|| bad("missing `dataSource` or a configuration"))?;
        let params = d.form.to_params();
        let base = d.editing.as_ref().and_then(|id| s.db.source(id)).map(|d| d.config.clone());
        let (mut cfg, pw, _) = config_from_params(s, &params, base.as_ref())?;
        if let Some(b) = &base {
            cfg.id = b.id.clone();
        }
        (cfg, pw)
    } else if p.get("kind").is_some() {
        let (cfg, pw, _) = config_from_params(s, p, None)?;
        (cfg, pw)
    } else {
        let id = target_source(s, p)?;
        (s.db.source(&id).map(|d| d.config.clone()).unwrap_or_default(), None)
    };
    let typed_ssh = match (p.is_null() || p.as_object().is_some_and(|m| m.is_empty()), &s.db.dialog) {
        (true, Some(d)) => Some(d.form.ssh_password.clone()),
        _ => p.get("sshPassword").and_then(Value::as_str).map(str::to_string),
    };
    if let Some(secret) = typed_ssh.filter(|v| !v.is_empty()) {
        s.db.remember_secret(&cfg.ssh_secret_account(), &secret);
    }
    s.status = format!("Testing connection to {}…", cfg.summary());
    let nonce = s.db.test(cfg, password);
    Ok(json!({ "testing": nonce }))
}

fn connect(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = target_source(s, p)?;
    ensure_connected(s, &id)?;
    let key = node_key(&id, None, None);
    s.db.expanded.insert(key.clone());
    s.db.selected = Some(key);
    ok()
}

fn disconnect(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    match opt_str(p, "dataSource") {
        Some(_) => {
            let id = target_source(s, p)?;
            s.db.disconnect(Some(&id));
        }
        None => match s.db.selected.clone().and_then(|k| k.split('\u{1f}').next().map(str::to_string)) {
            Some(id) if s.db.source(&id).is_some() => s.db.disconnect(Some(&id)),
            _ => s.db.disconnect(None),
        },
    }
    ok()
}

fn refresh(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let ids: Vec<String> = match target_source(s, p) {
        Ok(id) => vec![id],
        Err(_) if opt_str(p, "dataSource").is_none() => {
            s.db.sources
                .iter()
                .filter(|d| !matches!(d.status, ConnStatus::Disconnected))
                .map(|d| d.config.id.clone())
                .collect()
        }
        Err(e) => return Err(e),
    };
    for id in ids {
        let Some(ds) = s.db.source(&id) else { continue };
        if !matches!(ds.status, ConnStatus::Connected(_)) {
            ensure_connected(s, &id)?;
            continue;
        }
        // Reload what was loaded, in order: schemas, tables, then table details.
        let mut reqs = vec![MetaReq::Info];
        for (schema, node) in &ds.tree.schemas {
            if let Some(tables) = &node.tables {
                reqs.push(MetaReq::Tables(schema.clone()));
                for t in tables.iter().filter(|t| t.details.is_some()) {
                    reqs.push(MetaReq::Details(schema.clone(), t.info.name.clone()));
                }
            }
        }
        for r in reqs {
            s.db.meta(&id, r).map_err(failed)?;
        }
    }
    ok()
}

fn expand(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = target_source(s, p)?;
    let schema = opt_str(p, "schema");
    let table = opt_str(p, "table");
    if table.is_some() && schema.is_none() {
        return Err(bad("`table` needs `schema`"));
    }
    let key = node_key(&id, schema, table);
    let want = p.get("expanded").and_then(Value::as_bool).unwrap_or(!s.db.expanded.contains(&key));
    if !want {
        s.db.expanded.remove(&key);
        return ok();
    }
    s.db.expanded.insert(key);
    let Some(ds) = s.db.source(&id) else { return ok() };
    let connected = matches!(ds.status, ConnStatus::Connected(_));
    match (schema, table) {
        (None, _) => {
            if !connected {
                ensure_connected(s, &id)?;
            } else if ds.tree.info.is_none() && !ds.tree.loading {
                s.db.meta(&id, MetaReq::Info).map_err(failed)?;
            }
        }
        (Some(sc), None) => {
            let node = ds.tree.schemas.get(sc);
            if node.is_none_or(|n| n.tables.is_none() && !n.loading) {
                s.db.meta(&id, MetaReq::Tables(sc.to_string())).map_err(failed)?;
            }
            if connected {
                crate::database::sql_nav::request_routines(&mut s.db, &id, sc, false);
            }
        }
        (Some(sc), Some(t)) => {
            let node = ds.table(sc, t).ok_or_else(|| bad(format!("no table {sc}.{t} loaded")))?;
            if node.details.is_none() && !node.loading {
                s.db.meta(&id, MetaReq::Details(sc.to_string(), t.to_string())).map_err(failed)?;
            }
        }
    }
    ok()
}

fn row_count(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = target_source(s, p)?;
    let schema = req_str(p, "schema")?;
    let table = req_str(p, "table")?;
    s.db.source(&id)
        .and_then(|d| d.table(schema, table))
        .ok_or_else(|| bad(format!("no table {schema}.{table} loaded")))?;
    s.db.meta(&id, MetaReq::Count(schema.to_string(), table.to_string())).map_err(failed)?;
    ok()
}

// ---------------------------------------------------------------- consoles

fn new_console(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = target_source(s, p)?;
    let root =
        s.db.project_root().map(Path::to_path_buf).ok_or_else(|| failed("open a project to create SQL consoles"))?;
    let dir = root.join(CONSOLES_DIR).join(&id);
    std::fs::create_dir_all(&dir).map_err(|e| failed(format!("{}: {e}", dir.display())))?;
    let mut path = dir.join("console.sql");
    if bool_or(p, "new", false) {
        let mut n = 1;
        while path.exists() && n < 10_000 {
            path = dir.join(format!("console_{n}.sql"));
            n += 1;
        }
    }
    if !path.exists() {
        std::fs::write(&path, "").map_err(|e| failed(format!("{}: {e}", path.display())))?;
    }
    s.open_file(&path)?;
    ensure_connected(s, &id)?;
    Ok(json!({ "path": path, "dataSource": id }))
}

fn set_data_source(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = req_str(p, "dataSource")?.to_string();
    if s.db.source(&id).is_none() {
        return Err(bad(format!("no data source `{id}`")));
    }
    let path = match crate::params::opt_path(p, "path") {
        Some(p) => crate::normalize_path(&p),
        None => {
            s.ws.active()
                .and_then(|(_, d)| d.path.clone())
                .ok_or_else(|| bad("no SQL file: pass `path` or open one"))?
        }
    };
    if let Some(root) = s.db.project_root()
        && path.starts_with(root.join(CONSOLES_DIR))
    {
        return Err(failed(
            "consoles belong to the data source of their folder; open a console of the other data source",
        ));
    }
    s.db.bindings.insert(path, id.clone());
    save(s)?;
    ensure_connected(s, &id)?;
    ok()
}

// ---------------------------------------------------------------- execution

/// The statements a console command acts on: `sql` (on the data source's query connection), or
/// the active SQL console's selection, statement at the caret, or every statement (`all`).
/// `None`: nothing to run.
pub(super) fn console_run(
    s: &mut Session,
    p: &Value,
    all: bool,
    action: ExecAction,
) -> Result<Option<PendingRun>, CommandError> {
    sync_project(s);
    if let Some(sql_text) = opt_str(p, "sql") {
        let source = target_source(s, p)?;
        let stmts: Vec<String> =
            split_statements(sql_text, dialect_of(s, Some(&source))).into_iter().map(|st| st.text).collect();
        let worker = format!("query:{source}");
        return Ok((!stmts.is_empty()).then(|| PendingRun {
            action,
            source,
            worker,
            title: "Query".into(),
            statements: stmts,
        }));
    }
    let Some((e, d)) = s.ws.active() else {
        return Err(CommandError::Disabled("open an SQL console (db.newConsole) or pass `sql`".into()));
    };
    if d.language != Language::Sql {
        return Err(CommandError::Disabled("the active editor is not an SQL file".into()));
    }
    let path = d.path.clone();
    let source = match opt_str(p, "dataSource") {
        Some(_) => target_source(s, p)?,
        None => {
            s.db.source_for_path(path.as_deref())
                .ok_or_else(|| failed("choose a data source for this file (the console toolbar or db.setDataSource)"))?
        }
    };
    let dialect = dialect_of(s, Some(&source));
    let text = d.doc.text();
    let sel = e.selections.primary();
    let stmts: Vec<String> = if all {
        split_statements(&text, dialect).into_iter().map(|st| st.text).collect()
    } else if !sel.is_empty() {
        let r = sel.range();
        split_statements(&d.doc.slice(r.start..r.end.min(d.doc.len_chars())), dialect)
            .into_iter()
            .map(|st| st.text)
            .collect()
    } else {
        statement_at(&text, dialect, sel.head).map(|st| vec![st.text]).unwrap_or_default()
    };
    if stmts.is_empty() {
        return Ok(None);
    }
    let title = path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "console".into());
    Ok(Some(PendingRun { action, source, worker: console_worker(path.as_deref(), d.id.0), title, statements: stmts }))
}

/// `{name: value}` from `params` (numbers and booleans as their SQL text).
pub(super) fn param_values(p: &Value) -> Result<Option<std::collections::BTreeMap<String, String>>, CommandError> {
    match p.get("params") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(m)) => Ok(Some(
            m.iter()
                .map(|(k, v)| {
                    let text = match v {
                        Value::String(t) => t.clone(),
                        Value::Null => "NULL".into(),
                        other => other.to_string(),
                    };
                    (k.clone(), text)
                })
                .collect(),
        )),
        Some(_) => Err(bad("`params` must be an object {name: value}")),
    }
}

/// Run (or explain) what a console command selects, through the parameter prompt and guards.
pub(super) fn run_console(s: &mut Session, p: &Value, all: bool, action: ExecAction) -> CmdResult {
    let Some(mut run) = console_run(s, p, all, action)? else {
        s.status = "Nothing to execute".into();
        return ok();
    };
    if matches!(action, ExecAction::Explain { .. }) {
        run.statements.truncate(1);
    }
    let from_console = opt_str(p, "sql").is_none();
    let source = run.source.clone();
    let n = run.statements.len();
    let values = param_values(p)?;
    let out = crate::database::exec::submit(s, run, values.as_ref(), bool_or(p, "confirm", false)).map_err(failed)?;
    if from_console {
        let _ = ensure_connected(s, &source);
    }
    if out.get("run").is_some() {
        s.ws.layout.show(ToolWindowId::Services);
        s.status = match action {
            ExecAction::Execute => format!("Executing {n} statement{}…", if n == 1 { "" } else { "s" }),
            ExecAction::Explain { analyze: false } => "Explaining…".into(),
            ExecAction::Explain { analyze: true } => "Explaining (analyze)…".into(),
        };
    }
    Ok(out)
}

fn run_sql(s: &mut Session, p: &Value, all: bool) -> CmdResult {
    run_console(s, p, all, ExecAction::Execute)
}

fn can_cancel(s: &Session) -> Result<(), String> {
    if s.db.running() || s.db.results.iter().any(|r| r.has_more || r.is_running()) {
        Ok(())
    } else {
        Err("no query is running".into())
    }
}

fn cancel(s: &mut Session, p: &Value) -> CmdResult {
    let result = opt_u64(p, "result")?;
    let n = s.db.cancel(result);
    s.status = if n == 0 { "Nothing to cancel".into() } else { "Cancelling…".into() };
    Ok(json!({ "cancelled": n }))
}

fn open_table(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let source = target_source(s, p)?;
    let schema = req_str(p, "schema")?.to_string();
    let table = req_str(p, "table")?.to_string();
    let kind = kind_of(s, &source);
    let order = opt_str(p, "orderBy").map(str::to_string);
    let desc = bool_or(p, "desc", false);
    let sort = order.as_ref().map(|col| {
        let column =
            s.db.source(&source)
                .and_then(|d| d.table(&schema, &table))
                .and_then(|t| t.details.as_ref())
                .and_then(|d| d.columns.iter().position(|c| &c.name == col))
                .unwrap_or(0);
        SortKey { column, desc }
    });
    let filter = ResultFilter {
        where_: opt_str(p, "where").unwrap_or_default().to_string(),
        order_by: opt_str(p, "order").unwrap_or_default().to_string(),
    };
    let statement = if filter.is_empty() {
        sql::select_table(kind, &schema, &table, order.as_deref().map(|c| (c, desc)), None)
    } else {
        let ord = if filter.order_by.trim().is_empty() {
            order.as_deref().map(|c| sql::order_by_column(kind, c, desc)).unwrap_or_default()
        } else {
            filter.order_by.clone()
        };
        sql::select_table_where(kind, &schema, &table, &filter.where_, &ord)
    };
    let sort = if filter.order_by.trim().is_empty() { sort } else { None };
    s.db.request_keys(&source, &schema, &table);
    let worker = format!("tables:{source}");
    let run =
        s.db.start_run(RunSpec {
            worker,
            source: source.clone(),
            owner: format!("table:{source}:{schema}.{table}"),
            title: table.clone(),
            statements: vec![statement.clone()],
            table: Some(TableRef { schema, table }),
            sort,
            filter,
            ..RunSpec::default()
        })
        .map_err(failed)?;
    s.ws.layout.show(ToolWindowId::Services);
    Ok(json!({ "run": run, "sql": statement }))
}

fn result_param(s: &Session, p: &Value) -> Result<ResultSet, CommandError> {
    let id = match opt_u64(p, "result")? {
        Some(id) => id,
        None => s.db.active_result.ok_or_else(|| bad("missing `result`"))?,
    };
    s.db.result(id).cloned().ok_or_else(|| bad(format!("no result {id}")))
}

fn sort_result(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let column = req_usize(p, "column")?;
    if column >= r.columns.len() {
        return Err(bad(format!("no column {column}")));
    }
    if r.plan.is_some() {
        return Err(CommandError::Disabled("a plan cannot be sorted".into()));
    }
    let sort = match p.get("desc").and_then(Value::as_bool) {
        Some(desc) => Some(SortKey { column, desc }),
        None => match r.sort {
            Some(k) if k.column == column && !k.desc => Some(SortKey { column, desc: true }),
            Some(k) if k.column == column => None,
            _ => Some(SortKey { column, desc: false }),
        },
    };
    // Sorting by a header replaces a typed ORDER BY.
    let filter = ResultFilter { order_by: String::new(), ..r.filter.clone() };
    grid::requery(s, &r, filter, sort)
}

fn fetch_more(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    s.db.fetch_more(r.id).map_err(failed)?;
    ok()
}

fn close_result(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    if r.has_more {
        s.db.cancel(Some(r.id));
    }
    s.db.results.retain(|x| x.id != r.id);
    if s.db.active_result == Some(r.id) {
        s.db.active_result = s.db.results.last().map(|x| x.id);
    }
    ok()
}

fn export_csv(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let path = req_path(p, "path")?;
    if r.plan.is_some() {
        return Err(CommandError::Disabled("a plan has no rows; copy it with db.copyResult".into()));
    }
    let mut out = Vec::new();
    codecraft_db::write_csv(&mut out, &r.columns, &r.rows).map_err(|e| failed(e.to_string()))?;
    std::fs::write(&path, out).map_err(|e| failed(format!("{}: {e}", path.display())))?;
    let note = if r.has_more || r.truncated { " (the rows fetched so far)" } else { "" };
    s.status = format!("Exported {} rows to {}{note}", r.rows.len(), path.display());
    Ok(json!({ "rows": r.rows.len(), "path": path }))
}

fn get_result(s: &mut Session, p: &Value) -> CmdResult {
    let r = result_param(s, p)?;
    let offset = opt_usize(p, "offset")?.unwrap_or(0);
    let limit = opt_usize(p, "limit")?.unwrap_or(100).min(10_000);
    let rows: Vec<_> = r.rows.iter().skip(offset).take(limit).collect();
    Ok(json!({
        "id": r.id,
        "title": r.title,
        "dataSource": r.source,
        "sql": r.sql,
        "columns": r.columns,
        "rows": rows,
        "totalRows": r.rows.len(),
        "status": r.status,
        "statusLine": r.status_line(),
        "error": r.error,
        "hasMore": r.has_more,
        "editable": s.db.editable(r.id).is_ok(),
        "pendingRows": r.edits.pending_rows(),
        "filter": r.filter,
        "transposed": r.transposed,
        "plan": r.plan,
    }))
}

fn list_data_sources(s: &mut Session, _: &Value) -> CmdResult {
    sync_project(s);
    let sources: Vec<Value> =
        s.db.sources
            .iter()
            .map(|d| {
                json!({
                    "id": d.config.id,
                    "name": d.config.name,
                    "kind": d.config.kind,
                    "summary": d.config.summary(),
                    "status": d.status,
                    "color": d.config.color_name(),
                    "readOnly": d.config.read_only,
                    "confirmWrites": d.config.confirm_writes,
                    "manualCommit": d.config.manual_commit,
                    "openTransactions": s.db.open_transactions(Some(&d.config.id)),
                    "schemas": d.tree.schemas.iter().map(|(name, n)| json!({
                        "name": name,
                        "tables": n.tables.as_ref().map(|t| t.iter().map(|t| t.info.name.clone()).collect::<Vec<_>>()),
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
    let results: Vec<Value> =
        s.db.results
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "title": r.title,
                    "dataSource": r.source,
                    "status": r.status,
                    "rows": r.rows.len(),
                    "plan": r.plan.is_some(),
                })
            })
            .collect();
    Ok(json!({ "dataSources": sources, "results": results }))
}

// ---------------------------------------------------------------- completion

const KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "ON",
    "USING",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "ALTER",
    "DROP",
    "ADD",
    "COLUMN",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "UNIQUE",
    "DEFAULT",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "EXISTS",
    "DISTINCT",
    "AS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "UNION",
    "ALL",
    "ASC",
    "DESC",
    "WITH",
    "RETURNING",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "TRUE",
    "FALSE",
];

const CLAUSE_WORDS: &[&str] = &[
    "WHERE",
    "JOIN",
    "LEFT",
    "RIGHT",
    "INNER",
    "OUTER",
    "FULL",
    "CROSS",
    "ON",
    "USING",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "SET",
    "VALUES",
    "NATURAL",
    "UNION",
    "SELECT",
    "WINDOW",
    "RETURNING",
    "FETCH",
    "FOR",
    "AS",
    "EXCEPT",
    "INTERSECT",
    "DEFAULT",
];

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String, bool),
    Dot,
    Comma,
    Other,
}

/// Identifiers (quoted ones unquoted, flagged), dots and commas; literals and comments skipped.
fn tokens(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        match c {
            '-' if chars.get(i + 1) == Some(&'-') => {
                while chars.get(i).is_some_and(|c| *c != '\n') {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars.get(i) == Some(&'*') && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            '\'' => {
                i += 1;
                while i < chars.len() && chars.get(i) != Some(&'\'') {
                    i += 1;
                }
                i += 1;
                out.push(Tok::Other);
            }
            '"' | '`' | '[' => {
                let close = if c == '[' { ']' } else { c };
                let start = i + 1;
                i += 1;
                while i < chars.len() && chars.get(i) != Some(&close) {
                    i += 1;
                }
                out.push(Tok::Word(chars.get(start..i.min(chars.len())).unwrap_or(&[]).iter().collect(), true));
                i += 1;
            }
            '.' => {
                out.push(Tok::Dot);
                i += 1;
            }
            ',' => {
                out.push(Tok::Comma);
                i += 1;
            }
            c if is_ident(c) => {
                let start = i;
                while chars.get(i).is_some_and(|c| is_ident(*c)) {
                    i += 1;
                }
                out.push(Tok::Word(chars.get(start..i).unwrap_or(&[]).iter().collect(), false));
            }
            c if c.is_whitespace() => i += 1,
            _ => {
                out.push(Tok::Other);
                i += 1;
            }
        }
    }
    out
}

/// A table referenced by a statement: `(schema, table, alias)`.
type TableRefTok = (Option<String>, String, Option<String>);

/// Tables after FROM / JOIN / UPDATE / INTO, with aliases.
fn table_refs(sql: &str) -> Vec<TableRefTok> {
    let toks = tokens(sql);
    let is_kw = |t: Option<&Tok>, set: &[&str]| matches!(t, Some(Tok::Word(w, false)) if set.iter().any(|k| k.eq_ignore_ascii_case(w)));
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let from = is_kw(toks.get(i), &["FROM"]);
        if !(from || is_kw(toks.get(i), &["JOIN", "UPDATE", "INTO", "TABLE"])) {
            i += 1;
            continue;
        }
        i += 1;
        while let Some(Tok::Word(first, quoted)) = toks.get(i) {
            if !quoted && (is_kw(toks.get(i), CLAUSE_WORDS) || first.eq_ignore_ascii_case("ONLY")) {
                break;
            }
            let (schema, table) = if toks.get(i + 1) == Some(&Tok::Dot) {
                match toks.get(i + 2) {
                    Some(Tok::Word(t, _)) => {
                        i += 3;
                        (Some(first.clone()), t.clone())
                    }
                    _ => break,
                }
            } else {
                i += 1;
                (None, first.clone())
            };
            if is_kw(toks.get(i), &["AS"]) {
                i += 1;
            }
            let alias = match toks.get(i) {
                Some(Tok::Word(a, q)) if *q || !is_kw(toks.get(i), CLAUSE_WORDS) => {
                    i += 1;
                    Some(a.clone())
                }
                _ => None,
            };
            out.push((schema, table, alias));
            if !(from && toks.get(i) == Some(&Tok::Comma)) {
                break;
            }
            i += 1;
        }
    }
    out
}

fn needs_quotes(kind: DbKind, name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_alphabetic() || c == '_');
    let rest_ok = name.chars().all(|c| c.is_alphanumeric() || c == '_');
    !first_ok || !rest_ok || kind == DbKind::Postgres && name.chars().any(char::is_uppercase)
}

fn ident_text(kind: DbKind, name: &str) -> String {
    if needs_quotes(kind, name) { sql::quote_ident(kind, name) } else { name.to_string() }
}

fn item(label: &str, insert: String, kind: &str, detail: Option<String>, rank: u8, anchor: usize) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        detail,
        kind: kind.to_string(),
        filter_text: label.to_string(),
        sort_text: format!("{rank}{}", label.to_lowercase()),
        insert_text: insert,
        cursor: None,
        replace_start: Some(anchor),
        additional_edits: Vec::new(),
        documentation: None,
        deprecated: false,
        resolved: true,
        ..CompletionItem::default()
    }
}

fn complete(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let Some((e, d)) = s.ws.active() else { return Err(CommandError::Disabled("no editor is open".into())) };
    if d.language != Language::Sql {
        return s.execute("code.codeCompletion.basic", p);
    }
    let editor = e.id;
    let source =
        s.db.source_for_path(d.path.as_deref())
            .ok_or_else(|| failed("choose a data source for this file to complete table names"))?;
    let kind = kind_of(s, &source);
    let caret = e.selections.primary().head.min(d.doc.len_chars());
    let text = d.doc.text();
    let chars: Vec<char> = text.chars().collect();
    let mut anchor = caret;
    while anchor > 0 && chars.get(anchor - 1).is_some_and(|c| is_ident(*c)) {
        anchor -= 1;
    }
    let prefix: String = chars.get(anchor..caret).unwrap_or(&[]).iter().collect();
    // `qualifier.` before the prefix (quoted or not).
    let qualifier = (anchor > 0 && chars.get(anchor - 1) == Some(&'.')).then(|| {
        let end = anchor - 1;
        match chars.get(end.wrapping_sub(1)) {
            Some(q @ ('"' | '`')) => {
                let close = end - 1;
                let mut start = close;
                while start > 0 && chars.get(start - 1) != Some(q) {
                    start -= 1;
                }
                chars.get(start..close).unwrap_or(&[]).iter().collect::<String>()
            }
            _ => {
                let mut start = end;
                while start > 0 && chars.get(start - 1).is_some_and(|c| is_ident(*c)) {
                    start -= 1;
                }
                chars.get(start..end).unwrap_or(&[]).iter().collect::<String>()
            }
        }
    });
    let stmt = statement_at(&text, kind.dialect(), caret).map(|st| st.text).unwrap_or_default();
    let refs = table_refs(&stmt);

    let Some(ds) = s.db.source(&source) else { return ok() };
    // An attached file completes in its schema (track DC).
    let file_schema =
        d.path.as_deref().and_then(|p| s.db.nav.schema_for_path(p)).filter(|sc| ds.tree.schemas.contains_key(sc));
    let default_schema = file_schema.or_else(|| ds.tree.info.as_ref().and_then(|i| i.default_schema.clone()));
    let find_table = |schema: Option<&str>, name: &str| -> Option<(String, String)> {
        let schemas: Vec<&String> = match schema {
            Some(sc) => ds.tree.schemas.keys().filter(|k| k.eq_ignore_ascii_case(sc)).collect(),
            None => {
                let mut v: Vec<&String> = ds.tree.schemas.keys().collect();
                v.sort_by_key(|k| Some(*k) != default_schema.as_ref());
                v
            }
        };
        schemas.into_iter().find_map(|sc| {
            ds.tree
                .schemas
                .get(sc)?
                .tables
                .as_ref()?
                .iter()
                .find(|t| t.info.name.eq_ignore_ascii_case(name))
                .map(|t| (sc.clone(), t.info.name.clone()))
        })
    };
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut load: Vec<MetaReq> = Vec::new();
    let add_columns = |schema: &str, table: &str, items: &mut Vec<CompletionItem>, load: &mut Vec<MetaReq>| {
        let Some(t) = ds.table(schema, table) else { return };
        match &t.details {
            Some(det) => {
                for c in &det.columns {
                    let detail = Some(format!("{table}.{}", c.data_type));
                    items.push(item(&c.name, ident_text(kind, &c.name), "field", detail, 0, anchor));
                }
            }
            None if !t.loading => load.push(MetaReq::Details(schema.to_string(), table.to_string())),
            None => {}
        }
    };
    let schema_tables = |schema: &str, items: &mut Vec<CompletionItem>, load: &mut Vec<MetaReq>| {
        let Some(n) = ds.tree.schemas.get(schema) else { return };
        match &n.tables {
            Some(ts) => {
                for t in ts {
                    // Kinds the completion popup has icons for: tables are types, schemas modules.
                    let k = if t.info.kind == codecraft_db::TableKind::Table { "struct" } else { "type" };
                    items.push(item(
                        &t.info.name,
                        ident_text(kind, &t.info.name),
                        k,
                        Some(schema.to_string()),
                        1,
                        anchor,
                    ));
                }
            }
            None if !n.loading => load.push(MetaReq::Tables(schema.to_string())),
            None => {}
        }
    };
    if let Some(q) = &qualifier {
        let by_alias = refs.iter().find(|(_, t, a)| {
            a.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(q)) || a.is_none() && t.eq_ignore_ascii_case(q)
        });
        let target = match by_alias {
            Some((sc, t, _)) => find_table(sc.as_deref(), t),
            None => find_table(None, q),
        };
        match target {
            Some((sc, t)) => add_columns(&sc, &t, &mut items, &mut load),
            None => {
                if let Some(sc) = ds.tree.schemas.keys().find(|k| k.eq_ignore_ascii_case(q)).cloned() {
                    schema_tables(&sc, &mut items, &mut load);
                }
            }
        }
    } else {
        for (sc, t, _) in &refs {
            if let Some((sc, t)) = find_table(sc.as_deref(), t) {
                add_columns(&sc, &t, &mut items, &mut load);
            }
        }
        if let Some(sc) = &default_schema {
            schema_tables(sc, &mut items, &mut load);
        }
        for sc in ds.tree.schemas.keys() {
            items.push(item(sc, ident_text(kind, sc), "mod", Some("schema".into()), 2, anchor));
        }
        let lower = prefix.chars().next().is_some_and(char::is_lowercase);
        for kw in KEYWORDS {
            let text = if lower { kw.to_lowercase() } else { kw.to_string() };
            items.push(item(&text, text.clone(), "keyword", None, 3, anchor));
        }
    }
    // Columns of joined tables may repeat; keep the first of each label and detail.
    let mut seen = std::collections::HashSet::new();
    items.retain(|i| seen.insert((i.label.clone(), i.detail.clone())));
    let connected = matches!(ds.status, ConnStatus::Connected(_));
    if connected {
        for r in load {
            let _ = s.db.meta(&source, r);
        }
    } else {
        let _ = ensure_connected(s, &source);
    }
    if items.is_empty() {
        s.status = if connected { "No suggestions".into() } else { "Connecting to load the schema…".into() };
        return ok();
    }
    let n = items.len();
    s.popup = Some(Popup::Completion(CompletionPopup {
        editor,
        anchor,
        items,
        visible: Vec::new(),
        selected: 0,
        pending: false,
        ..CompletionPopup::default()
    }));
    s.refresh_completion_filter();
    Ok(json!({ "items": n }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_refs_with_aliases() {
        let r = table_refs("select * from public.users u join orders as o on o.uid = u.id, items where 1");
        assert_eq!(
            r,
            vec![(Some("public".into()), "users".into(), Some("u".into())), (None, "orders".into(), Some("o".into())),]
        );
        let r = table_refs("select * from a, \"B c\" x where x.y = 1");
        assert_eq!(r, vec![(None, "a".into(), None), (None, "B c".into(), Some("x".into()))]);
        assert_eq!(table_refs("update t set a = 1"), vec![(None, "t".into(), None)]);
        assert_eq!(table_refs("insert into t(a) values (1)"), vec![(None, "t".into(), None)]);
        assert!(table_refs("select 'from x' -- from y\n").is_empty());
        assert!(table_refs("select * from").is_empty());
    }

    #[test]
    fn quoting_rules() {
        assert!(!needs_quotes(DbKind::Postgres, "users"));
        assert!(needs_quotes(DbKind::Postgres, "Users"));
        assert!(!needs_quotes(DbKind::Sqlite, "Users"));
        assert!(needs_quotes(DbKind::Mysql, "a b"));
        assert_eq!(ident_text(DbKind::Mysql, "a b"), "`a b`");
    }
}
