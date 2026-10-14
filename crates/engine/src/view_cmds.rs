//! View menu commands: tool windows, quick info popups, font size.

use serde_json::Value;

use codecraft_workspace::ToolWindowId;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_lsp_editor, ok, simple, spec};
use crate::lsp::GotoKind;
use crate::{Popup, Session};

/// Tool windows CodeCraft implements (the others stay disabled in the menu until they exist).
pub const IMPLEMENTED_TOOL_WINDOWS: [ToolWindowId; 28] = [
    ToolWindowId::Project,
    ToolWindowId::Find,
    ToolWindowId::Run,
    ToolWindowId::Problems,
    ToolWindowId::Structure,
    ToolWindowId::Build,
    ToolWindowId::Cargo,
    ToolWindowId::Notifications,
    ToolWindowId::Debug,
    ToolWindowId::Database,
    ToolWindowId::Agent,
    ToolWindowId::Services,
    ToolWindowId::Terminal,
    ToolWindowId::Commit,
    ToolWindowId::Git,
    ToolWindowId::Bookmarks,
    ToolWindowId::Todo,
    ToolWindowId::Coverage,
    ToolWindowId::Hierarchy,
    ToolWindowId::DatabaseChanges,
    ToolWindowId::InspectionResults,
    ToolWindowId::Profiler,
    ToolWindowId::PythonConsole,
    ToolWindowId::PythonPackages,
    ToolWindowId::FileTransfer,
    ToolWindowId::RemoteHost,
    ToolWindowId::RustRepl,
    ToolWindowId::PythonProcessOutput,
];

macro_rules! tool_window {
    ($id:literal, $label:literal, $tw:expr) => {
        simple($id, $label, always, |s, _| {
            s.ws.layout.toggle($tw);
            ok()
        })
    };
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    let mut v = vec![
        tool_window!("view.toolWindows.project", "Project", ToolWindowId::Project),
        tool_window!("view.toolWindows.find", "Find", ToolWindowId::Find),
        tool_window!("view.toolWindows.run", "Run", ToolWindowId::Run),
        tool_window!("view.toolWindows.problems", "Problems", ToolWindowId::Problems),
        tool_window!("view.toolWindows.structure", "Structure", ToolWindowId::Structure),
        tool_window!("view.toolWindows.build", "Build", ToolWindowId::Build),
        tool_window!("view.toolWindows.cargo", "Cargo", ToolWindowId::Cargo),
        tool_window!("view.toolWindows.notifications", "Notifications", ToolWindowId::Notifications),
        tool_window!("view.toolWindows.debug", "Debug", ToolWindowId::Debug),
        tool_window!("view.toolWindows.database", "Database", ToolWindowId::Database),
        tool_window!("view.toolWindows.agent", "Agent", ToolWindowId::Agent),
        tool_window!("view.toolWindows.terminal", "Terminal", ToolWindowId::Terminal),
        tool_window!("view.toolWindows.commit", "Commit", ToolWindowId::Commit),
        tool_window!("view.toolWindows.git", "Git", ToolWindowId::Git),
        tool_window!("view.toolWindows.bookmarks", "Bookmarks", ToolWindowId::Bookmarks),
        tool_window!("view.toolWindows.todo", "TODO", ToolWindowId::Todo),
        tool_window!("view.toolWindows.coverage", "Coverage", ToolWindowId::Coverage),
        tool_window!("view.toolWindows.hierarchy", "Hierarchy", ToolWindowId::Hierarchy),
        tool_window!("view.toolWindows.databaseChanges", "Database Changes", ToolWindowId::DatabaseChanges),
        tool_window!("view.toolWindows.inspectionResults", "Inspection Results", ToolWindowId::InspectionResults),
        tool_window!("view.toolWindows.profiler", "Profiler", ToolWindowId::Profiler),
        tool_window!("view.toolWindows.pythonConsole", "Python Console", ToolWindowId::PythonConsole),
        tool_window!("view.toolWindows.pythonPackages", "Python Packages", ToolWindowId::PythonPackages),
        tool_window!("view.toolWindows.fileTransfer", "File Transfer", ToolWindowId::FileTransfer),
        tool_window!("view.toolWindows.remoteHost", "Remote Host", ToolWindowId::RemoteHost),
        spec(
            "window.activeToolWindow.hideAllWindows",
            "Hide All Windows",
            None,
            "",
            Context::Global,
            always,
            |s, _| {
                s.ws.layout.all_hidden = !s.ws.layout.all_hidden;
                ok()
            },
        ),
        spec(
            "view.quickDocumentation",
            "Quick Documentation",
            Some("F1"),
            "",
            Context::Editor,
            has_lsp_editor,
            |s, _| {
                s.lsp_hover()?;
                ok()
            },
        ),
        spec("view.quickDefinition", "Quick Definition", None, "", Context::Editor, has_lsp_editor, |s, _| {
            s.lsp_goto(GotoKind::QuickDefinition)?;
            ok()
        }),
        spec("view.quickTypeDefinition", "Quick Type Definition", None, "", Context::Editor, has_lsp_editor, |s, _| {
            s.lsp_goto(GotoKind::QuickTypeDefinition)?;
            ok()
        }),
        spec("view.parameterInfo", "Parameter Info", None, "", Context::Editor, has_lsp_editor, |s, _| {
            s.lsp_signature_help()?;
            ok()
        }),
        spec("view.errorDescription", "Error Description", None, "", Context::Editor, has_editor, error_description),
        simple("view.increaseFontSizeInAllEditors", "Increase Font Size in All Editors", always, |s, _| font(s, 1.0)),
        simple("view.decreaseFontSizeInAllEditors", "Decrease Font Size in All Editors", always, |s, _| font(s, -1.0)),
        simple("view.resetFontSizeInAllEditors", "Reset Font Size in All Editors", always, |s, _| {
            s.ws.settings.editor_font_size = codecraft_workspace::Settings::default().editor_font_size;
            ok()
        }),
        spec("view.toggleSoftWrap", "Soft-Wrap", None, "", Context::Global, always, |s, _| {
            s.ws.settings.soft_wrap = !s.ws.settings.soft_wrap;
            ok()
        }),
        spec("view.toggleWhitespace", "Show Whitespaces", None, "", Context::Global, always, |s, _| {
            s.ws.settings.show_whitespace = !s.ws.settings.show_whitespace;
            ok()
        }),
        spec("view.toggleLineNumbers", "Show Line Numbers", None, "", Context::Global, always, |s, _| {
            s.ws.settings.show_line_numbers = !s.ws.settings.show_line_numbers;
            ok()
        }),
    ];
    v.shrink_to_fit();
    v
}

fn font(s: &mut Session, delta: f32) -> CmdResult {
    s.ws.settings.editor_font_size = (s.ws.settings.editor_font_size + delta).clamp(6.0, 72.0);
    ok()
}

/// Cmd+F1: the problem(s) at the caret in a popup.
fn error_description(s: &mut Session, _: &Value) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return ok() };
    let Some(path) = d.path.clone() else { return ok() };
    let offset = e.selections.primary().head;
    let pos = d.doc.offset_to_position(offset);
    let text: Vec<String> =
        s.ws.problems_for(&path)
            .into_iter()
            .filter(|p| p.start <= pos && pos <= p.end || p.start.line == pos.line)
            .map(|p| match &p.code {
                Some(c) => format!("**{}[{c}]**: {}", p.severity.label(), p.message),
                None => format!("**{}**: {}", p.severity.label(), p.message),
            })
            .collect();
    if text.is_empty() {
        s.status = "No problems at the caret".into();
    } else {
        s.popup = Some(Popup::Hover { editor: e.id, offset, text: text.join("\n\n") });
    }
    ok()
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_implemented_tool_window_has_a_toggle() {
        // Some toggles live with their track (Services: database_cmds.rs).
        let reg = crate::registry();
        for tw in super::IMPLEMENTED_TOOL_WINDOWS {
            let id = format!("view.toolWindows.{}", tw.key());
            assert!(reg.contains(&id), "{id}");
        }
    }
}
