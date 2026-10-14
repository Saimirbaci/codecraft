//! Track DD commands: the Redis key browser (`db.redis.*`): open/close, SCAN with pattern and
//! type, database selection, namespaces, values with paging, and edits under the read-only and
//! production guards. OWNED BY track DD.

use serde_json::{Value, json};

use codecraft_db::DbKind;

use super::{bad, failed, target_source};
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::database::redis_browser::{bytes_param, op_from_params};
use crate::database::sync_project;
use crate::params::{bool_or, opt_str};
use crate::{CommandError, Session};

pub(super) fn specs() -> Vec<CommandSpec> {
    let mut v = vec![
        spec(
            "db.redis.open",
            "Open Redis Key Browser",
            None,
            "{dataSource?, db?: number} — opens the key browser of a Redis data source and scans its keys",
            Context::Global,
            always,
            open,
        ),
        spec("db.redis.close", "Close Redis Key Browser", None, "{dataSource?}", Context::Global, always, |s, p| {
            let id = source(s, p)?;
            s.db.redis_close(Some(&id));
            ok()
        }),
        spec(
            "db.redis.state",
            "Redis Key Browser State",
            None,
            "{dataSource?, tree?: bool} — keys, cursor, namespaces (with `tree`), the selected key's value",
            Context::Global,
            always,
            state,
        ),
        spec(
            "db.redis.scan",
            "Scan Redis Keys",
            None,
            "{dataSource?, pattern?: glob (default *), type?: string|hash|list|set|zset|stream|ReJSON-RL|all, db?} — \
             restarts the scan (SCAN with MATCH/TYPE, never KEYS)",
            Context::Global,
            always,
            scan,
        ),
        spec(
            "db.redis.scanMore",
            "Load More Redis Keys",
            None,
            "{dataSource?} — continues the scan from its cursor",
            Context::Global,
            always,
            |s, p| {
                let id = browser(s, p)?;
                s.db.redis_scan(&id, None, None, false).map_err(failed)?;
                ok()
            },
        ),
        spec(
            "db.redis.selectDb",
            "Select Redis Database",
            None,
            "{dataSource?, db: number}",
            Context::Global,
            always,
            |s, p| {
                let id = source(s, p)?;
                let db = db_param(p)?.ok_or_else(|| bad("missing `db`"))?;
                s.db.redis_open(&id, Some(db)).map_err(failed)?;
                ok()
            },
        ),
        spec(
            "db.redis.toggleNamespace",
            "Expand Redis Namespace",
            None,
            "{dataSource?, prefix: e.g. `user:`, expanded?: bool}",
            Context::Global,
            always,
            |s, p| {
                let id = browser(s, p)?;
                let prefix = opt_str(p, "prefix").ok_or_else(|| bad("missing `prefix`"))?.to_string();
                let b = s.db.redis.browsers.get_mut(&id).ok_or_else(|| failed("the key browser is not open"))?;
                let open = p.get("expanded").and_then(Value::as_bool).unwrap_or(!b.expanded.contains(&prefix));
                if open {
                    b.expanded.insert(prefix);
                } else {
                    b.expanded.remove(&prefix);
                }
                b.revision += 1;
                Ok(json!({ "expanded": open }))
            },
        ),
        spec(
            "db.redis.openKey",
            "Show Redis Key",
            None,
            "{dataSource?, key | keyHex} — loads the key's type, TTL, size and first page of its value",
            Context::Global,
            always,
            |s, p| {
                let id = browser(s, p)?;
                let key = bytes_param(p, "key").map_err(bad)?.ok_or_else(|| bad("missing `key`"))?;
                s.db.redis_load(&id, key, false).map_err(failed)?;
                ok()
            },
        ),
        spec(
            "db.redis.moreValues",
            "Load More Values",
            None,
            "{dataSource?} — the next page of the shown value",
            Context::Global,
            always,
            |s, p| {
                let id = browser(s, p)?;
                let key =
                    s.db.redis
                        .get(&id)
                        .and_then(|b| b.value.as_ref().map(|v| v.key.clone()))
                        .ok_or_else(|| failed("no key is shown"))?;
                s.db.redis_load(&id, key, true).map_err(failed)?;
                ok()
            },
        ),
    ];
    for id in crate::database::redis_browser::EDIT_COMMANDS.iter().copied() {
        v.push(spec(id, label(id), None, params_doc(id), Context::Global, always, edit_runner(id)));
    }
    v
}

fn label(id: &str) -> &'static str {
    match id {
        "db.redis.setString" => "Set Redis String",
        "db.redis.hashSet" => "Set Hash Field",
        "db.redis.hashDelete" => "Delete Hash Field",
        "db.redis.listSet" => "Set List Item",
        "db.redis.listPush" => "Push List Item",
        "db.redis.listRemove" => "Remove List Item",
        "db.redis.setAdd" => "Add Set Member",
        "db.redis.setRemove" => "Remove Set Member",
        "db.redis.zsetAdd" => "Add Sorted Set Member",
        "db.redis.zsetRemove" => "Remove Sorted Set Member",
        "db.redis.streamAdd" => "Add Stream Entry",
        "db.redis.streamDelete" => "Delete Stream Entry",
        "db.redis.jsonSet" => "Set JSON Value",
        "db.redis.setTtl" => "Set Key TTL",
        "db.redis.deleteKeys" => "Delete Redis Keys",
        "db.redis.renameKey" => "Rename Redis Key",
        "db.redis.newKey" => "New Redis Key",
        _ => "Redis Edit",
    }
}

fn params_doc(id: &str) -> &'static str {
    match id {
        "db.redis.setString" => "{dataSource?, key|keyHex, value|valueHex, confirm?} — keeps the TTL",
        "db.redis.hashSet" => "{dataSource?, key, field, value, confirm?}",
        "db.redis.hashDelete" => "{dataSource?, key, field, confirm?}",
        "db.redis.listSet" => "{dataSource?, key, index, value, confirm?}",
        "db.redis.listPush" => "{dataSource?, key, value, head?: bool, confirm?}",
        "db.redis.listRemove" => "{dataSource?, key, index, confirm?} — removes the item at index",
        "db.redis.setAdd" | "db.redis.setRemove" => "{dataSource?, key, member, confirm?}",
        "db.redis.zsetAdd" => "{dataSource?, key, member, score, confirm?}",
        "db.redis.zsetRemove" => "{dataSource?, key, member, confirm?}",
        "db.redis.streamAdd" => "{dataSource?, key, fields: {field: value}, confirm?} — XADD with an automatic id",
        "db.redis.streamDelete" => "{dataSource?, key, id, confirm?}",
        "db.redis.jsonSet" => "{dataSource?, key, value: JSON, confirm?} — RedisJSON JSON.SET at $",
        "db.redis.setTtl" => "{dataSource?, key, seconds?: number (null removes the expiry), confirm?}",
        "db.redis.deleteKeys" => "{dataSource?, key? | keys: [..], confirm?} — UNLINK",
        "db.redis.renameKey" => "{dataSource?, key, newName, confirm?} — refuses to overwrite an existing key",
        "db.redis.newKey" => {
            "{dataSource?, key, type: string|hash|list|set|zset|stream|json, value?, field?, score?, confirm?}"
        }
        _ => "",
    }
}

fn edit_runner(id: &str) -> crate::commands::Run {
    match id {
        "db.redis.setString" => |s, p| edit(s, p, "db.redis.setString"),
        "db.redis.hashSet" => |s, p| edit(s, p, "db.redis.hashSet"),
        "db.redis.hashDelete" => |s, p| edit(s, p, "db.redis.hashDelete"),
        "db.redis.listSet" => |s, p| edit(s, p, "db.redis.listSet"),
        "db.redis.listPush" => |s, p| edit(s, p, "db.redis.listPush"),
        "db.redis.listRemove" => |s, p| edit(s, p, "db.redis.listRemove"),
        "db.redis.setAdd" => |s, p| edit(s, p, "db.redis.setAdd"),
        "db.redis.setRemove" => |s, p| edit(s, p, "db.redis.setRemove"),
        "db.redis.zsetAdd" => |s, p| edit(s, p, "db.redis.zsetAdd"),
        "db.redis.zsetRemove" => |s, p| edit(s, p, "db.redis.zsetRemove"),
        "db.redis.streamAdd" => |s, p| edit(s, p, "db.redis.streamAdd"),
        "db.redis.streamDelete" => |s, p| edit(s, p, "db.redis.streamDelete"),
        "db.redis.jsonSet" => |s, p| edit(s, p, "db.redis.jsonSet"),
        "db.redis.setTtl" => |s, p| edit(s, p, "db.redis.setTtl"),
        "db.redis.deleteKeys" => |s, p| edit(s, p, "db.redis.deleteKeys"),
        "db.redis.renameKey" => |s, p| edit(s, p, "db.redis.renameKey"),
        _ => |s, p| edit(s, p, "db.redis.newKey"),
    }
}

/// A Redis data source: the param, the shown browser's, or the selected one.
fn source(s: &Session, p: &Value) -> Result<String, CommandError> {
    let id = if opt_str(p, "dataSource").is_none()
        && let Some(a) = &s.db.redis.active
    {
        a.clone()
    } else {
        target_source(s, p)?
    };
    match s.db.source(&id).map(|d| d.config.kind) {
        Some(DbKind::Redis) => Ok(id),
        Some(k) => Err(bad(format!("`{id}` is a {} data source, not Redis", k.label()))),
        None => Err(bad(format!("no data source `{id}`"))),
    }
}

/// A data source whose browser is open.
fn browser(s: &Session, p: &Value) -> Result<String, CommandError> {
    let id = source(s, p)?;
    if s.db.redis.get(&id).is_none() {
        return Err(CommandError::Disabled("the key browser is not open (db.redis.open)".into()));
    }
    Ok(id)
}

fn db_param(p: &Value) -> Result<Option<u32>, CommandError> {
    match p.get("db") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            n.as_u64().and_then(|n| u32::try_from(n).ok()).map(Some).ok_or_else(|| bad("bad `db`"))
        }
        Some(Value::String(t)) => t.trim().trim_start_matches("db").parse().map(Some).map_err(|_| bad("bad `db`")),
        Some(_) => Err(bad("bad `db`")),
    }
}

fn open(s: &mut Session, p: &Value) -> CmdResult {
    sync_project(s);
    let id = match opt_str(p, "dataSource") {
        Some(_) => source(s, p)?,
        None => target_source(s, p).and_then(|id| source(s, &json!({ "dataSource": id })))?,
    };
    let db = db_param(p)?;
    s.db.redis_open(&id, db).map_err(failed)?;
    Ok(json!({ "dataSource": id }))
}

fn scan(s: &mut Session, p: &Value) -> CmdResult {
    let id = source(s, p)?;
    if s.db.redis.get(&id).is_none() || p.get("db").is_some() {
        s.db.redis_open(&id, db_param(p)?).map_err(failed)?;
    }
    let pattern = opt_str(p, "pattern").map(str::to_string);
    let kind = p.get("type").map(|t| t.as_str().map(str::to_string));
    s.db.redis_scan(&id, pattern, kind, true).map_err(failed)?;
    ok()
}

fn state(s: &mut Session, p: &Value) -> CmdResult {
    let id = browser(s, p)?;
    let b = s.db.redis.get(&id).ok_or_else(|| failed("the key browser is not open"))?;
    let mut v = serde_json::to_value(b).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("selected".into(), json!(b.selected_name()));
        o.insert("complete".into(), json!(b.cursor.is_none()));
        if bool_or(p, "tree", false) {
            o.insert("tree".into(), serde_json::to_value(b.tree(':')).unwrap_or(Value::Null));
        }
    }
    Ok(v)
}

/// An edit: read-only data sources refuse it; production ones need `confirm: true` (the UI
/// asks first and shows the commands).
fn edit(s: &mut Session, p: &Value, id: &str) -> CmdResult {
    let source = browser(s, p)?;
    let op = op_from_params(id, p).map_err(bad)?;
    let preview = op.preview();
    let ds = s.db.source(&source).ok_or_else(|| bad(format!("no data source `{source}`")))?;
    if ds.config.read_only {
        return Err(CommandError::Disabled(format!("{} is read-only", ds.config.name)));
    }
    if ds.config.confirm_writes && !bool_or(p, "confirm", false) {
        return Err(failed(format!(
            "{} is a production data source: confirm the change (pass `confirm: true`):\n{preview}",
            ds.config.name
        )));
    }
    s.db.redis_write(&source, op).map_err(failed)?;
    Ok(json!({ "commands": preview }))
}
