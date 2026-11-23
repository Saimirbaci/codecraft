//! Tool window bars (the stripes): which tool window buttons they show and in what order.
//!
//! The choices are [`Settings::tool_window_bars`](codecraft_workspace::Settings) (persisted with
//! the settings). Hiding a button does not close or disable the window: View › Tool Windows and
//! the shortcuts still open it. Everything here is a command, so agents configure the bars the
//! way the dialog does: `view.appearance.toolWindowBars.{customize,show,move,reset,close}`.

use serde_json::{Value, json};

use codecraft_workspace::{Side, ToolWindowId};

use crate::commands::{CmdResult, CommandError, CommandSpec, Context, always, spec};
use crate::{IMPLEMENTED_TOOL_WINDOWS, Session};

/// One group of buttons. The left bar has two: its own windows at the top, and the bottom tool
/// windows low on it (RustRover's New UI).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Left,
    Bottom,
    Right,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Left, Group::Bottom, Group::Right];

    pub fn key(self) -> &'static str {
        match self {
            Group::Left => "left",
            Group::Bottom => "bottom",
            Group::Right => "right",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Group::Left => "Left bar",
            Group::Bottom => "Bottom tool windows (low on the left bar)",
            Group::Right => "Right bar",
        }
    }

    /// Whether the window belongs to this group.
    fn holds(self, tw: ToolWindowId) -> bool {
        match self {
            Group::Left => tw.side() == Side::Left,
            Group::Bottom => tw.side() == Side::Bottom,
            Group::Right => tw.side() == Side::Right,
        }
    }

    /// The group's windows from top to bottom of the bar, before the user's order. The windows
    /// only some projects have (CMake, Zig, TypeScript) are always listed: the shell skips them
    /// where they do not apply.
    pub fn default_order(self) -> Vec<ToolWindowId> {
        let mut v: Vec<ToolWindowId> = IMPLEMENTED_TOOL_WINDOWS.into_iter().filter(|t| self.holds(*t)).collect();
        if self == Group::Bottom {
            for tw in [ToolWindowId::CMake, ToolWindowId::Zig, ToolWindowId::TypeScript] {
                v.insert(0, tw);
            }
        }
        v
    }

    fn of(tw: ToolWindowId) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.default_order().contains(&tw))
    }
}

/// The group's buttons from top to bottom as the user arranged them: hidden windows are left
/// out unless `include_hidden`.
pub fn buttons(s: &Session, group: Group, include_hidden: bool) -> Vec<ToolWindowId> {
    s.ws.settings.tool_window_bars.arrange(&group.default_order(), include_hidden)
}

fn tool(p: &Value) -> Result<ToolWindowId, CommandError> {
    let key = p
        .get("tool")
        .and_then(Value::as_str)
        .ok_or_else(|| CommandError::BadParams("`tool` is a tool window id such as \"project\"".into()))?;
    ToolWindowId::from_key(key).ok_or_else(|| CommandError::BadParams(format!("no tool window `{key}`")))
}

fn group_of(tw: ToolWindowId) -> Result<Group, CommandError> {
    Group::of(tw).ok_or_else(|| CommandError::BadParams(format!("`{}` has no button on a bar", tw.key())))
}

/// The bars as data, for the dialog and for agents.
pub fn state(s: &Session) -> Value {
    let bars = &s.ws.settings.tool_window_bars;
    json!({
        "dialog": s.appearance.bars_dialog,
        "customized": !bars.is_default(),
        "groups": Group::ALL.iter().map(|g| json!({
            "group": g.key(),
            "title": g.title(),
            "tools": buttons(s, *g, true).into_iter().map(|t| json!({
                "tool": t.key(),
                "title": t.title(),
                "shown": bars.is_shown(t),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

fn customize(s: &mut Session, _: &Value) -> CmdResult {
    s.appearance.bars_dialog = true;
    Ok(state(s))
}

fn close(s: &mut Session, _: &Value) -> CmdResult {
    s.appearance.bars_dialog = false;
    Ok(Value::Null)
}

fn show(s: &mut Session, p: &Value) -> CmdResult {
    let tw = tool(p)?;
    group_of(tw)?;
    let shown = p.get("on").and_then(Value::as_bool).unwrap_or(!s.ws.settings.tool_window_bars.is_shown(tw));
    s.ws.settings.tool_window_bars.set_shown(tw, shown);
    Ok(json!({ "tool": tw.key(), "on": shown }))
}

/// `{tool, by}` moves a button up (negative) or down; `{tool, before}` puts it before another
/// button of the same group (`null` = last).
fn move_button(s: &mut Session, p: &Value) -> CmdResult {
    let tw = tool(p)?;
    let group = group_of(tw)?;
    let order = group.default_order();
    let bars = &mut s.ws.settings.tool_window_bars;
    let moved = if let Some(by) = p.get("by") {
        let by = by
            .as_i64()
            .ok_or_else(|| CommandError::BadParams("`by` is a whole number of places".into()))?;
        bars.shift(&order, tw, by.clamp(-1000, 1000) as i32)
    } else if let Some(before) = p.get("before") {
        let before = if before.is_null() { None } else { Some(tool(&json!({ "tool": before }))?) };
        if before.is_some_and(|b| !order.contains(&b)) {
            return Err(CommandError::BadParams("`before` is on another bar".into()));
        }
        bars.move_before(&order, tw, before)
    } else {
        return Err(CommandError::BadParams("`by` (places) or `before` (a tool window) is needed".into()));
    };
    Ok(json!({ "tool": tw.key(), "moved": moved }))
}

fn reset(s: &mut Session, _: &Value) -> CmdResult {
    s.ws.settings.tool_window_bars = Default::default();
    Ok(state(s))
}

const PARAMS_SHOW: &str = "{tool: string, on?: bool} — show or hide the tool window's button (it still opens from View › Tool Windows)";
const PARAMS_MOVE: &str =
    "{tool: string, by?: int, before?: string|null} — move the button `by` places (negative = up) or before another button of its bar";

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "view.appearance.toolWindowBars.customize",
            "Customize Tool Window Bars…",
            None,
            "Opens the dialog that chooses the buttons of the left and right bars and their order",
            Context::Global,
            always,
            customize,
        ),
        spec("view.appearance.toolWindowBars.close", "Close Tool Window Bars Dialog", None, "", Context::Global, always, close),
        spec("view.appearance.toolWindowBars.show", "Show Tool Window Button", None, PARAMS_SHOW, Context::Global, always, show),
        spec("view.appearance.toolWindowBars.move", "Move Tool Window Button", None, PARAMS_MOVE, Context::Global, always, move_button),
        spec("view.appearance.toolWindowBars.reset", "Reset Tool Window Bars", None, "", Context::Global, always, reset),
        spec("view.appearance.toolWindowBars.state", "Tool Window Bars State", None, "", Context::Global, always, |s, _| Ok(state(s))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &mut Session, id: &str, p: Value) -> Value {
        s.execute(id, &p).unwrap()
    }

    fn keys(s: &Session, g: Group) -> Vec<&'static str> {
        buttons(s, g, false).into_iter().map(ToolWindowId::key).collect()
    }

    #[test]
    fn the_groups_cover_every_button_once() {
        for tw in IMPLEMENTED_TOOL_WINDOWS {
            let n = Group::ALL.into_iter().filter(|g| g.default_order().contains(&tw)).count();
            assert_eq!(n, 1, "{tw:?}");
        }
        assert_eq!(Group::Left.default_order()[0], ToolWindowId::Project);
    }

    #[test]
    fn hide_show_and_reorder_buttons_and_reset() {
        let mut s = Session::new();
        let before = keys(&s, Group::Right);
        run(&mut s, "view.appearance.toolWindowBars.show", json!({ "tool": "cargo", "on": false }));
        assert!(!keys(&s, Group::Right).contains(&"cargo"));
        // Hidden is not disabled: the window still toggles.
        run(&mut s, "view.toolWindows.cargo", Value::Null);
        assert!(s.ws.layout.is_visible(ToolWindowId::Cargo));
        // The dialog's list keeps it, unchecked.
        assert!(buttons(&s, Group::Right, true).contains(&ToolWindowId::Cargo));
        run(&mut s, "view.appearance.toolWindowBars.show", json!({ "tool": "cargo" }));
        assert_eq!(keys(&s, Group::Right), before);

        let last = *before.last().unwrap_or(&"");
        run(&mut s, "view.appearance.toolWindowBars.move", json!({ "tool": last, "by": -1 }));
        assert_eq!(keys(&s, Group::Right)[before.len() - 2], last);
        run(&mut s, "view.appearance.toolWindowBars.move", json!({ "tool": last, "before": before[0] }));
        assert_eq!(keys(&s, Group::Right)[0], last);
        // The other bars did not move.
        assert_eq!(keys(&s, Group::Left)[0], "project");

        let st = run(&mut s, "view.appearance.toolWindowBars.reset", Value::Null);
        assert_eq!(st["customized"], false);
        assert_eq!(keys(&s, Group::Right), before);
    }

    #[test]
    fn bad_params_are_errors_not_panics() {
        let mut s = Session::new();
        for (id, p) in [
            ("show", json!({})),
            ("show", json!({ "tool": "nope" })),
            ("move", json!({ "tool": "project" })),
            ("move", json!({ "tool": "project", "by": "x" })),
            ("move", json!({ "tool": "project", "before": "cargo" })),
            ("move", json!({ "tool": "project", "before": 3 })),
        ] {
            let r = s.execute(&format!("view.appearance.toolWindowBars.{id}"), &p);
            assert!(r.is_err(), "{id} {p}");
        }
        assert!(s.ws.settings.tool_window_bars.is_default());
        // A huge distance is clamped to the end, not an overflow.
        assert!(s.execute("view.appearance.toolWindowBars.move", &json!({ "tool": "project", "by": i64::MAX })).is_ok());
        assert!(s.execute("view.appearance.toolWindowBars.move", &json!({ "tool": "project", "by": i64::MIN })).is_ok());
    }

    #[test]
    fn the_dialog_opens_and_closes_and_state_lists_every_group() {
        let mut s = Session::new();
        let st = run(&mut s, "view.appearance.toolWindowBars.customize", Value::Null);
        assert_eq!(st["dialog"], true);
        assert_eq!(st["groups"].as_array().map(Vec::len), Some(3));
        run(&mut s, "view.appearance.toolWindowBars.close", Value::Null);
        assert!(!s.appearance.bars_dialog);
        assert_eq!(run(&mut s, "view.appearance.state", Value::Null)["bars"]["dialog"], false);
    }
}
