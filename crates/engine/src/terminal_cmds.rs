//! Terminal commands: tabs, input (`terminal.sendText`/`sendKey` for agents and tests), copy and
//! paste, clear, scrolling, reading the buffer, Run in Terminal and Open Claude Code.
//!
//! Inside the focused terminal the UI maps Cmd+C/V/K/A/T/W and Shift+Cmd+[/] to these commands
//! (a shortcut context the registry cannot express, so the specs carry no shortcuts); the tool
//! window itself is `view.toolWindows.terminal` (Alt+F12).

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, spec};
use crate::params::{bool_or, opt_path, opt_str, req_str, req_usize};
use crate::terminal::{self, NewTab};
use crate::{CommandError, Effect, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "terminal.new",
            "New Terminal Tab",
            None,
            "{cwd?: path, command?: string, args?: [string], title?: string} — a login shell in the project root; \
             `command` runs a command line in the shell instead, or the program itself when `args` is given",
            Context::Global,
            always,
            new_tab,
        ),
        spec("terminal.close", "Close Terminal Tab", None, "{tab?: int}", Context::Global, has_tab, |s, p| {
            let i = tab_index(s, p)?;
            terminal::close(s, i);
            ok()
        }),
        spec("terminal.next", "Next Terminal Tab", None, "", Context::Global, has_tab, |s, _| {
            cycle(s, 1);
            ok()
        }),
        spec("terminal.previous", "Previous Terminal Tab", None, "", Context::Global, has_tab, |s, _| {
            cycle(s, -1);
            ok()
        }),
        spec("terminal.activate", "Select Terminal Tab", None, "{tab: int}", Context::Global, has_tab, |s, p| {
            let i = tab_index(s, p)?;
            s.terminal.active = i;
            if let Some(t) = s.terminal.tabs.get_mut(i) {
                t.bell = false;
            }
            s.ws.layout.show(codecraft_workspace::ToolWindowId::Terminal);
            ok()
        }),
        spec(
            "terminal.rename",
            "Rename Terminal Tab",
            None,
            "{tab?: int, title?: string} — the tab keeps this name even when the program sets a title; \
             a blank `title` restores the automatic name; no `title` edits the name in place (UI)",
            Context::Global,
            has_tab,
            |s, p| {
                let i = tab_index(s, p)?;
                let id = s.terminal.tabs.get(i).map(|t| t.id);
                match p.get("title") {
                    None | Some(Value::Null) => {
                        s.terminal.rename_requested = id;
                        s.ws.layout.show(codecraft_workspace::ToolWindowId::Terminal);
                    }
                    Some(Value::String(title)) => terminal::rename(s, i, title),
                    Some(_) => return Err(CommandError::BadParams("`title` must be a string".into())),
                }
                let active = s.terminal.active;
                Ok(s.terminal.tabs.get(i).map(|t| t.info(i == active)).unwrap_or(Value::Null))
            },
        ),
        spec(
            "terminal.sendText",
            "Send Text to Terminal",
            None,
            "{text: string, tab?: int} — written to the terminal as typed (use \\r for Enter)",
            Context::Global,
            has_tab,
            |s, p| {
                let text = req_str(p, "text")?.to_string();
                let i = live_tab_index(s, p)?;
                if let Some(t) = s.terminal.tabs.get(i) {
                    t.term.send_text(&text);
                }
                ok()
            },
        ),
        spec(
            "terminal.sendKey",
            "Send Key to Terminal",
            None,
            "{key: string, tab?: int} — e.g. `Enter`, `Ctrl+C`, `Up`, `Shift+Tab`, `Alt+b`, `F5`",
            Context::Global,
            has_tab,
            |s, p| {
                let name = req_str(p, "key")?;
                let (key, mods) = codecraft_term::parse_key(name)
                    .ok_or_else(|| CommandError::BadParams(format!("unknown key `{name}`")))?;
                let i = live_tab_index(s, p)?;
                let sent = s.terminal.tabs.get(i).is_some_and(|t| t.term.send_key(key, mods));
                if !sent {
                    return Err(CommandError::BadParams(format!("`{name}` has no terminal encoding")));
                }
                ok()
            },
        ),
        spec(
            "terminal.paste",
            "Paste into Terminal",
            None,
            "{text?: string, tab?: int} — the text (default: CodeCraft's clipboard), bracketed if the program asked",
            Context::Global,
            has_tab,
            |s, p| {
                let text = match opt_str(p, "text") {
                    Some(t) => t.to_string(),
                    None => s.clipboard.as_ref().map(|(t, _)| t.clone()).unwrap_or_default(),
                };
                let i = live_tab_index(s, p)?;
                if !text.is_empty()
                    && let Some(t) = s.terminal.tabs.get(i)
                {
                    t.term.paste(&text);
                }
                ok()
            },
        ),
        spec(
            "terminal.copy",
            "Copy Terminal Selection",
            None,
            "{tab?: int} → {text} — copies the selection (nothing if none)",
            Context::Global,
            has_tab,
            |s, p| {
                let i = tab_index(s, p)?;
                let text = s.terminal.tabs.get(i).and_then(|t| t.term.selection_text());
                if let Some(text) = &text {
                    s.clipboard = Some((text.clone(), false));
                    s.effects.push(Effect::CopyToClipboard(text.clone()));
                }
                Ok(json!({ "text": text }))
            },
        ),
        spec("terminal.selectAll", "Select All in Terminal", None, "{tab?: int}", Context::Global, has_tab, |s, p| {
            let i = tab_index(s, p)?;
            if let Some(t) = s.terminal.tabs.get_mut(i) {
                t.term.select_all();
                t.refresh();
            }
            ok()
        }),
        spec(
            "terminal.clear",
            "Clear Terminal",
            None,
            "{tab?: int} — clear the scrollback and screen, keeping the prompt line (Cmd+K in the terminal)",
            Context::Global,
            has_tab,
            |s, p| {
                let i = tab_index(s, p)?;
                if let Some(t) = s.terminal.tabs.get_mut(i) {
                    t.term.clear();
                    t.refresh();
                }
                ok()
            },
        ),
        spec(
            "terminal.scroll",
            "Scroll Terminal",
            None,
            "{lines?: int (positive = up), pages?: int, to?: \"top\"|\"bottom\", offset?: int, tab?: int}",
            Context::Global,
            has_tab,
            scroll,
        ),
        spec(
            "terminal.resize",
            "Resize Terminal",
            None,
            "{cols: int, rows: int, cellWidth?: int, cellHeight?: int, tab?: int}",
            Context::Global,
            has_tab,
            |s, p| {
                let to_u16 = |v: usize| u16::try_from(v).unwrap_or(u16::MAX);
                let cols = to_u16(req_usize(p, "cols")?);
                let rows = to_u16(req_usize(p, "rows")?);
                let cw = to_u16(crate::params::opt_usize(p, "cellWidth")?.unwrap_or(8));
                let ch = to_u16(crate::params::opt_usize(p, "cellHeight")?.unwrap_or(16));
                let i = tab_index(s, p)?;
                if let Some(t) = s.terminal.tabs.get_mut(i) {
                    t.term.resize(cols, rows, (cw, ch));
                    t.refresh();
                }
                ok()
            },
        ),
        spec(
            "terminal.read",
            "Read Terminal",
            None,
            "{tab?: int, scrollback?: bool} → {text, title, cols, rows, cursor, alive, exitCode, …}",
            Context::Global,
            has_tab,
            read,
        ),
        spec(
            "terminal.list",
            "List Terminal Tabs",
            None,
            "→ [{tab, title, cwd, alive, …}]",
            Context::Global,
            always,
            |s, _| {
                let active = s.terminal.active;
                Ok(Value::Array(s.terminal.tabs.iter().enumerate().map(|(i, t)| t.info(i == active)).collect()))
            },
        ),
        spec(
            "terminal.runInTerminal",
            "Run in Terminal",
            None,
            "{command: string, cwd?: path} — a new shell tab that runs the command and stays open",
            Context::Global,
            always,
            |s, p| {
                let command = req_str(p, "command")?.to_string();
                if command.trim().is_empty() {
                    return Err(CommandError::BadParams("`command` is empty".into()));
                }
                let title = command.chars().take(40).collect::<String>();
                let id =
                    terminal::open(s, NewTab { cwd: opt_path(p, "cwd"), title: Some(title), ..NewTab::default() })?;
                if let Some(t) = s.terminal.tabs.iter().find(|t| t.id == id) {
                    // The shell reads it once it is ready (the PTY buffers it).
                    t.term.send_text(&format!("{command}\r"));
                }
                Ok(json!({ "tab": id }))
            },
        ),
        spec(
            "terminal.openClaude",
            "Open Claude Code in Terminal",
            None,
            "{cwd?: path} — a terminal tab running `claude` in the project root; it connects to CodeCraft's IDE server",
            Context::Global,
            always,
            open_claude,
        ),
        spec(
            "terminal.toggleAltAsMeta",
            "Use Option as Meta Key",
            None,
            "{enabled?: bool} — Option/Alt sends ESC-prefixed keys (Alt+B, Alt+F) instead of composing characters",
            Context::Global,
            always,
            |s, p| {
                s.terminal.alt_is_meta = bool_or(p, "enabled", !s.terminal.alt_is_meta);
                Ok(json!({ "enabled": s.terminal.alt_is_meta }))
            },
        ),
        spec(
            "terminal.setShell",
            "Set Terminal Shell",
            None,
            "{path?: string} — the shell for new tabs (default `$SHELL`); not persisted",
            Context::Global,
            always,
            |s, p| {
                match opt_str(p, "path").map(str::trim).filter(|v| !v.is_empty()) {
                    Some(path) => {
                        let meta =
                            std::fs::metadata(path).map_err(|e| CommandError::BadParams(format!("{path}: {e}")))?;
                        if !meta.is_file() {
                            return Err(CommandError::BadParams(format!("{path} is not a program")));
                        }
                        s.terminal.shell = Some(path.to_string());
                    }
                    None => s.terminal.shell = None,
                }
                ok()
            },
        ),
    ]
}

fn has_tab(s: &Session) -> Result<(), String> {
    if s.terminal.tabs.is_empty() { Err("no terminal is open".into()) } else { Ok(()) }
}

/// The `tab` param (an id) or the active tab.
fn tab_index(s: &Session, p: &Value) -> Result<usize, CommandError> {
    match p.get("tab") {
        None | Some(Value::Null) => {
            if s.terminal.tabs.get(s.terminal.active).is_some() {
                Ok(s.terminal.active)
            } else {
                Err(CommandError::Failed("no terminal is open".into()))
            }
        }
        Some(v) => {
            let id = v.as_u64().ok_or_else(|| CommandError::BadParams("`tab` must be a terminal tab id".into()))?;
            s.terminal.index_of(id).ok_or_else(|| CommandError::BadParams(format!("no terminal tab {id}")))
        }
    }
}

fn live_tab_index(s: &Session, p: &Value) -> Result<usize, CommandError> {
    let i = tab_index(s, p)?;
    match s.terminal.tabs.get(i) {
        Some(t) if t.alive() => Ok(i),
        Some(_) => Err(CommandError::Failed("the terminal's process has exited".into())),
        None => Err(CommandError::Failed("no terminal is open".into())),
    }
}

fn cycle(s: &mut Session, delta: isize) {
    let n = s.terminal.tabs.len();
    if n == 0 {
        return;
    }
    s.terminal.active = (s.terminal.active as isize + delta).rem_euclid(n as isize) as usize;
    if let Some(t) = s.terminal.active_tab_mut() {
        t.bell = false;
    }
}

fn new_tab(s: &mut Session, p: &Value) -> CmdResult {
    let args = match p.get("args") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|v| v.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| CommandError::BadParams("`args` must be strings".into()))?,
        ),
        Some(_) => return Err(CommandError::BadParams("`args` must be an array of strings".into())),
    };
    let command = opt_str(p, "command").map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
    if args.is_some() && command.is_none() {
        return Err(CommandError::BadParams("`args` needs `command`".into()));
    }
    let id = terminal::open(
        s,
        NewTab {
            cwd: opt_path(p, "cwd"),
            command,
            args,
            title: opt_str(p, "title").map(str::to_string),
            renamed: true,
            env: Vec::new(),
        },
    )?;
    Ok(json!({ "tab": id }))
}

fn scroll(s: &mut Session, p: &Value) -> CmdResult {
    let i = tab_index(s, p)?;
    let Some(t) = s.terminal.tabs.get_mut(i) else { return ok() };
    let int = |key: &str| -> Result<Option<i32>, CommandError> {
        match p.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_i64()
                .map(|n| Some(n.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32))
                .ok_or_else(|| CommandError::BadParams(format!("`{key}` must be an integer"))),
        }
    };
    if let Some(to) = opt_str(p, "to") {
        match to {
            "top" => t.term.scroll(codecraft_term::Scroll::Top),
            "bottom" => t.term.scroll(codecraft_term::Scroll::Bottom),
            other => return Err(CommandError::BadParams(format!("unknown `to`: {other}"))),
        }
    } else if let Some(offset) = crate::params::opt_usize(p, "offset")? {
        t.term.scroll_to(offset);
    } else if let Some(pages) = int("pages")? {
        let page = if pages > 0 { codecraft_term::Scroll::PageUp } else { codecraft_term::Scroll::PageDown };
        for _ in 0..pages.unsigned_abs().min(1000) {
            t.term.scroll(page);
        }
    } else if let Some(lines) = int("lines")? {
        t.term.scroll(codecraft_term::Scroll::Lines(lines));
    } else {
        return Err(CommandError::BadParams("give `lines`, `pages`, `offset` or `to`".into()));
    }
    t.refresh();
    Ok(json!({ "offset": t.screen.display_offset }))
}

fn read(s: &mut Session, p: &Value) -> CmdResult {
    let i = tab_index(s, p)?;
    let active = s.terminal.active;
    let Some(t) = s.terminal.tabs.get_mut(i) else { return ok() };
    t.refresh();
    let text = if bool_or(p, "scrollback", false) { t.term.text(true) } else { t.screen.text() };
    let mut info = t.info(i == active);
    if let Some(obj) = info.as_object_mut() {
        obj.insert("text".into(), json!(text));
        obj.insert("cols".into(), json!(t.screen.cols));
        obj.insert("rows".into(), json!(t.screen.rows));
        obj.insert("cursor".into(), json!(t.screen.cursor.map(|c| json!({ "row": c.row, "col": c.col }))));
        obj.insert("altScreen".into(), json!(t.screen.modes.alt_screen));
        obj.insert("displayOffset".into(), json!(t.screen.display_offset));
        obj.insert("history".into(), json!(t.screen.history));
    }
    Ok(info)
}

/// `claude` from the enriched `PATH` (the usual per-user install locations included).
fn claude_program(s: &Session) -> Result<PathBuf, CommandError> {
    if let Some(p) = &s.terminal.claude_program {
        return if p.is_file() {
            Ok(p.clone())
        } else {
            Err(CommandError::Failed(format!("Claude Code not found at {}", p.display())))
        };
    }
    // Sessions without background services (tests, the headless CLI) never start the real CLI.
    if !s.lsp_enabled {
        return Err(CommandError::Failed("Claude Code is not started in a session without background services".into()));
    }
    codecraft_agent::backend::find_in_path("claude", &codecraft_agent::backend::enriched_path()).ok_or_else(|| {
        CommandError::Failed(
            "Claude Code (`claude`) was not found; install it with `npm install -g @anthropic-ai/claude-code`".into(),
        )
    })
}

fn open_claude(s: &mut Session, p: &Value) -> CmdResult {
    let program = claude_program(s)?;
    let path = codecraft_agent::backend::enriched_path().to_string_lossy().into_owned();
    let id = terminal::open(
        s,
        NewTab {
            cwd: opt_path(p, "cwd"),
            command: Some(program.display().to_string()),
            args: Some(Vec::new()),
            title: Some("Claude Code".into()),
            renamed: false,
            env: vec![("PATH".into(), path)],
        },
    )?;
    Ok(json!({ "tab": id, "program": program, "ideServer": s.ide.port() }))
}
