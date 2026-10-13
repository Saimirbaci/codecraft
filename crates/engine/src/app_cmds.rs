//! Application-level commands: Help and project windows. Each project window has its own
//! `Session`; the shell owns the windows, so these commands ask it through effects.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::commands::{CommandSpec, Context, always, ok, simple, spec};
use crate::params::req_path;
use crate::{Effect, Popup, Session};

/// The user guide shown by Help › Help.
pub const HELP: &str = include_str!("help.md");

/// Open project windows, as the shell last reported them (app-wide, not per session).
static PROJECT_WINDOWS: AtomicUsize = AtomicUsize::new(1);

/// The shell reports how many project windows are open (Next/Previous need two).
pub fn set_project_window_count(n: usize) {
    PROJECT_WINDOWS.store(n, Ordering::Relaxed);
}

pub fn project_window_count() -> usize {
    PROJECT_WINDOWS.load(Ordering::Relaxed)
}

fn several_windows(_: &Session) -> Result<(), String> {
    if project_window_count() > 1 { Ok(()) } else { Err("only one project window is open".into()) }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        simple("help.help", "Help", always, |s, _| {
            s.popup = Some(Popup::Message { title: "CodeCraft Help".into(), text: HELP.into() });
            ok()
        }),
        spec(
            "file.openInNewWindow",
            "Open in New Window",
            None,
            "{path: string} — open a project directory (or a file) in a new project window",
            Context::Global,
            always,
            |s, p| {
                let path = req_path(p, "path")?;
                s.effects.push(Effect::OpenProjectWindow(path));
                ok()
            },
        ),
        simple("window.nextProjectWindow", "Next Project Window", several_windows, |s, _| {
            s.effects.push(Effect::SwitchProjectWindow(1));
            ok()
        }),
        simple("window.previousProjectWindow", "Previous Project Window", several_windows, |s, _| {
            s.effects.push(Effect::SwitchProjectWindow(-1));
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    #[test]
    fn help_opens_the_guide() {
        let mut s = Session::new();
        s.execute("help.help", &Value::Null).unwrap();
        let Some(Popup::Message { title, text }) = &s.popup else { panic!("{:?}", s.popup) };
        assert_eq!(title, "CodeCraft Help");
        assert!(text.contains("## Getting around"));
    }

    #[test]
    fn project_window_commands_ask_the_shell() {
        let mut s = Session::new();
        s.execute("file.openInNewWindow", &json!({ "path": "/tmp/x" })).unwrap();
        assert_eq!(s.effects, vec![Effect::OpenProjectWindow("/tmp/x".into())]);
        assert!(s.execute("file.openInNewWindow", &Value::Null).is_err());
        // The window count is process-wide; this is the only test that changes it.
        set_project_window_count(1);
        assert!(s.execute("window.nextProjectWindow", &Value::Null).is_err());
        set_project_window_count(2);
        s.effects.clear();
        s.execute("window.nextProjectWindow", &Value::Null).unwrap();
        s.execute("window.previousProjectWindow", &Value::Null).unwrap();
        assert_eq!(s.effects, vec![Effect::SwitchProjectWindow(1), Effect::SwitchProjectWindow(-1)]);
        set_project_window_count(1);
    }
}
