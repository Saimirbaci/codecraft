//! Commands for the IDE server (Claude Code IDE integration).

use serde_json::{Value, json};

use crate::Session;
use crate::commands::{CmdResult, CommandError, CommandSpec, always, has_editor, simple};
use crate::ide_server;

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("ide.status", "Claude Code: IDE Server Status", always, status),
        simple("ide.restart", "Claude Code: Restart IDE Server", always, restart),
        simple("ide.enable", "Claude Code: Enable IDE Integration", always, enable),
        simple("ide.disable", "Claude Code: Disable IDE Integration", always, disable),
        simple("ide.sendSelection", "Claude Code: Send Selection to Claude", connected_editor, send_selection),
    ]
}

fn connected_editor(s: &Session) -> Result<(), String> {
    has_editor(s)?;
    if s.ide.client_count() == 0 { Err("Claude Code is not connected".into()) } else { Ok(()) }
}

/// `{enabled, running, port, lockFile, root, clients, pendingDiffs, error, env}`.
fn status(s: &mut Session, _: &Value) -> CmdResult {
    let v = s.ide.status_json();
    s.status = match (s.ide.port(), s.ide.last_error()) {
        (Some(port), _) => format!("IDE server on port {port}, {} client(s) connected", s.ide.client_count()),
        (None, Some(e)) => format!("IDE server is not running: {e}"),
        (None, None) if !s.ide.is_enabled() => "IDE integration is disabled".into(),
        (None, None) => "IDE server is not running (open a project to start it)".into(),
    };
    Ok(v)
}

fn restart(s: &mut Session, _: &Value) -> CmdResult {
    ide_server::restart(s);
    status(s, &Value::Null)
}

fn enable(s: &mut Session, _: &Value) -> CmdResult {
    s.ide.set_enabled(true);
    ide_server::apply_settings(s);
    status(s, &Value::Null)
}

fn disable(s: &mut Session, _: &Value) -> CmdResult {
    s.ide.set_enabled(false);
    ide_server::apply_settings(s);
    status(s, &Value::Null)
}

fn send_selection(s: &mut Session, _: &Value) -> CmdResult {
    let sent = ide_server::send_at_mention(s).map_err(CommandError::Failed)?;
    s.status = "Sent the selection to Claude Code".into();
    Ok(json!({ "clients": sent }))
}
