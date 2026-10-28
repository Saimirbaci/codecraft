//! Window › Active Tool Window (hide it, hide the side ones, jump back to the last one) and
//! Window › Notifications (close the latest or all of them). Hide All Windows is
//! `view_cmds.rs`.

use serde_json::{Value, json};

use codecraft_workspace::ToolWindowId;

use crate::Session;
use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};

fn has_visible(s: &Session) -> Result<(), String> {
    s.ws.layout.active().map(|_| ()).ok_or_else(|| "no tool window is open".into())
}

fn has_notifications(s: &Session) -> Result<(), String> {
    if s.ws.notifications.is_empty() { Err("there are no notifications".into()) } else { Ok(()) }
}

fn hide_active(s: &mut Session, _: &Value) -> CmdResult {
    let Some(tw) = s.ws.layout.active() else { return ok() };
    s.ws.layout.hide(tw);
    s.effects.push(crate::Effect::FocusEditor);
    Ok(json!({ "hidden": tw.key() }))
}

fn jump_to_last(s: &mut Session, _: &Value) -> CmdResult {
    let tw = s.ws.layout.last_active.unwrap_or(ToolWindowId::Project);
    s.ws.layout.show(tw);
    Ok(json!({ "shown": tw.key() }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "window.activeToolWindow.hideActiveToolWindow",
            "Hide Active Tool Window",
            None,
            "Hides the tool window shown last (else a visible one) → {hidden}",
            Context::Global,
            has_visible,
            hide_active,
        ),
        spec(
            "window.activeToolWindow.hideSideToolWindows",
            "Hide Side Tool Windows",
            None,
            "Hides the left and right tool windows",
            Context::Global,
            always,
            |s, _| {
                s.ws.layout.left = None;
                s.ws.layout.left_lower = None;
                s.ws.layout.right = None;
                ok()
            },
        ),
        spec(
            "window.activeToolWindow.jumpToLastToolWindow",
            "Jump to Last Tool Window",
            None,
            "Shows the tool window shown last (Project at first) → {shown}",
            Context::Global,
            always,
            jump_to_last,
        ),
        spec(
            "window.notifications.closeFirst",
            "Close First",
            None,
            "Closes the latest notification",
            Context::Global,
            has_notifications,
            |s, _| {
                s.ws.notifications.pop();
                ok()
            },
        ),
        spec("window.notifications.closeAll", "Close All", None, "", Context::Global, has_notifications, |s, _| {
            s.ws.notifications.clear();
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use codecraft_workspace::NotificationLevel;

    #[test]
    fn active_tool_window_commands() {
        let mut s = Session::new();
        s.execute("view.toolWindows.run", &Value::Null).unwrap();
        s.execute("view.toolWindows.cargo", &Value::Null).unwrap();
        assert_eq!(s.ws.layout.last_active, Some(ToolWindowId::Cargo));
        let v = s.execute("window.activeToolWindow.hideActiveToolWindow", &Value::Null).unwrap();
        assert_eq!(v["hidden"], "cargo");
        // Then the next visible one (bottom first).
        let v = s.execute("window.activeToolWindow.hideActiveToolWindow", &Value::Null).unwrap();
        assert_eq!(v["hidden"], "run");
        let v = s.execute("window.activeToolWindow.jumpToLastToolWindow", &Value::Null).unwrap();
        assert_eq!(v["shown"], "cargo");
        assert!(s.ws.layout.is_visible(ToolWindowId::Cargo));
        s.execute("window.activeToolWindow.hideSideToolWindows", &Value::Null).unwrap();
        assert!(s.ws.layout.left.is_none() && s.ws.layout.right.is_none());
        assert!(s.enabled("window.activeToolWindow.hideActiveToolWindow").is_err());
        // Hide All Windows (catalog id) and its old id.
        s.execute("view.toolWindows.project", &Value::Null).unwrap();
        s.execute("window.activeToolWindow.hideAllWindows", &Value::Null).unwrap();
        assert!(s.ws.layout.all_hidden);
        s.execute("view.hideAllToolWindows", &Value::Null).unwrap();
        assert!(!s.ws.layout.all_hidden);
    }

    #[test]
    fn notifications_close_first_and_all() {
        let mut s = Session::new();
        assert!(s.enabled("window.notifications.closeAll").is_err());
        for t in ["a", "b", "c"] {
            s.ws.notify(NotificationLevel::Info, t, "");
        }
        s.execute("window.notifications.closeFirst", &Value::Null).unwrap();
        let titles: Vec<&str> = s.ws.notifications.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(titles, ["a", "b"]);
        s.execute("window.notifications.closeAll", &Value::Null).unwrap();
        assert!(s.ws.notifications.is_empty());
    }
}
