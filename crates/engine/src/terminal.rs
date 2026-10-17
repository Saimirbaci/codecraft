//! Terminal sessions (PTY + emulator) behind the Terminal tool window. OWNED BY the terminal track.
//!
//! Each tab is a [`codecraft_term::Terminal`]: a login shell (or a program such as `claude`) on a
//! PTY, started in the project root with the IDE server's environment, so `claude` run there
//! connects to CodeCraft. Output is parsed on background threads; [`pump`] takes a fresh
//! [`Screen`] snapshot for each tab whose output changed. Nothing is persisted across restarts.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::Serialize;
use serde_json::{Value, json};

use codecraft_term::{ExitInfo, Palette, Screen, SharedPalette, SharedWaker, TermConfig, TermEvent, Terminal};
use codecraft_workspace::ToolWindowId;

use crate::{CommandError, Effect, Session};

/// Open tabs are capped so a runaway script or agent cannot fork shells without bound.
pub const MAX_TABS: usize = 32;

/// Inherited variables a CodeCraft terminal must not pass on: another terminal's or IDE's
/// identity (CodeCraft sets its own), and Claude Code's nesting marker (a CodeCraft terminal is
/// a fresh top-level terminal, not a child of the Claude Code session that may have started us).
const ENV_REMOVE: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    codecraft_mcp::ENV_PORT,
    codecraft_mcp::ENV_ENABLE,
    "FORCE_CODE_TERMINAL",
    "TERM_SESSION_ID",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "TMUX",
    "TMUX_PANE",
    "VSCODE_INJECTION",
    "VSCODE_GIT_IPC_HANDLE",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum TabStatus {
    Running,
    Exited(ExitInfo),
}

pub struct TerminalTab {
    /// Stable id (the `tab` param of `terminal.*` commands).
    pub id: u64,
    /// "Local", "Claude Code", the command, or a name given with `terminal.rename`.
    pub name: String,
    /// The name was given by the user (`terminal.rename`, `terminal.new {title}`): it is shown
    /// even when the program sets a title, so a renamed tab keeps its name.
    pub renamed: bool,
    /// The title the program set (OSC 0/2), shown instead of an automatic `name` while set.
    pub osc_title: Option<String>,
    /// The start directory, updated by OSC 7 reports.
    pub cwd: PathBuf,
    /// The command line it runs (`None` for the login shell).
    pub command: Option<String>,
    pub status: TabStatus,
    /// The latest snapshot of the visible grid (refreshed by `pump`).
    pub screen: Screen,
    /// Set by the bell; cleared when the tab is shown.
    pub bell: bool,
    pub term: Terminal,
}

impl TerminalTab {
    pub fn title(&self) -> &str {
        if self.renamed {
            return &self.name;
        }
        self.osc_title.as_deref().map(str::trim).filter(|t| !t.is_empty()).unwrap_or(&self.name)
    }

    pub fn alive(&self) -> bool {
        self.status == TabStatus::Running
    }

    /// Re-read the screen from the emulator now (after scrolling or selecting).
    pub fn refresh(&mut self) {
        self.screen = self.term.screen();
    }

    pub(crate) fn info(&self, active: bool) -> Value {
        let (code, signal) = match &self.status {
            TabStatus::Running => (None, None),
            TabStatus::Exited(i) => (i.code, i.signal.clone()),
        };
        json!({
            "tab": self.id,
            "title": self.title(),
            "renamed": self.renamed,
            "cwd": self.cwd,
            "command": self.command,
            "alive": self.alive(),
            "exitCode": code,
            "signal": signal,
            "active": active,
        })
    }
}

pub struct TerminalState {
    pub tabs: Vec<TerminalTab>,
    /// Index of the active tab.
    pub active: usize,
    /// Alt/Option sends Meta (ESC prefix) instead of composing characters.
    pub alt_is_meta: bool,
    /// The shell (RustRover's Settings › Terminal › Shell path); default `$SHELL`.
    pub shell: Option<String>,
    /// The program `terminal.openClaude` runs; default `claude` on the enriched `PATH`.
    pub claude_program: Option<PathBuf>,
    pub scrollback: usize,
    /// The UI should focus the terminal (a tab was just opened).
    pub focus_requested: bool,
    /// The UI should start editing this tab's name in place (`terminal.rename` without `title`).
    pub rename_requested: Option<u64>,
    next_id: u64,
    waker: SharedWaker,
    palette: SharedPalette,
    /// Tabs being terminated in the background.
    closing: Vec<Receiver<()>>,
}

impl Default for TerminalState {
    fn default() -> Self {
        Self {
            tabs: Vec::new(),
            active: 0,
            alt_is_meta: false,
            shell: None,
            claude_program: None,
            scrollback: 10_000,
            focus_requested: false,
            rename_requested: None,
            next_id: 1,
            waker: SharedWaker::default(),
            palette: SharedPalette::default(),
            closing: Vec::new(),
        }
    }
}

impl TerminalState {
    /// Called from background threads when a terminal's screen changed (the UI repaints).
    pub fn set_waker(&self, waker: codecraft_term::Waker) {
        *self.waker.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(waker);
    }

    /// The theme's colours, for programs that query them (OSC 4/10/11).
    pub fn set_palette(&self, palette: Palette) {
        *self.palette.write().unwrap_or_else(std::sync::PoisonError::into_inner) = palette;
    }

    pub fn active_tab(&self) -> Option<&TerminalTab> {
        self.tabs.get(self.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut TerminalTab> {
        self.tabs.get_mut(self.active)
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Whether processes are still being terminated.
    pub fn closing(&self) -> bool {
        !self.closing.is_empty()
    }

    /// A path printed in a tab (`src/main.rs`, `~/x.rs`, `/abs/y.rs`) resolved against the tab's
    /// directory, then `root`; only existing files.
    pub fn resolve_path(&self, tab: usize, raw: &str, root: Option<&Path>) -> Option<PathBuf> {
        let expanded = match raw.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => PathBuf::from(raw),
        };
        if expanded.is_absolute() {
            return expanded.is_file().then(|| crate::normalize_path(&expanded));
        }
        let cwd = self.tabs.get(tab).map(|t| t.cwd.clone());
        [cwd, root.map(Path::to_path_buf)]
            .into_iter()
            .flatten()
            .map(|base| base.join(&expanded))
            .find(|p| p.is_file())
            .map(|p| crate::normalize_path(&p))
    }

    fn close_index(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() {
            return false;
        }
        let mut tab = self.tabs.remove(index);
        self.closing.push(tab.term.terminate());
        if self.active > index || self.active >= self.tabs.len() {
            self.active = self.active.saturating_sub(1);
        }
        true
    }
}

/// What `terminal.new` starts.
#[derive(Clone, Debug, Default)]
pub(crate) struct NewTab {
    pub cwd: Option<PathBuf>,
    /// A program to run directly (with `args`), or a command line for the shell (`args: None`).
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub title: Option<String>,
    /// `title` is the user's name for the tab (kept when the program sets a title).
    pub renamed: bool,
    /// Extra environment (e.g. an enriched `PATH`).
    pub env: Vec<(String, String)>,
}

fn default_cwd(s: &Session) -> PathBuf {
    s.project_root().or_else(|| std::env::var_os("HOME").map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("/"))
}

/// The shell: the configured one; in sessions without background services (tests, the CLI),
/// plain `/bin/sh` instead of the user's login shell and its startup files.
fn shell(s: &Session) -> Option<String> {
    s.terminal.shell.clone().or_else(|| (!s.lsp_enabled).then(|| "/bin/sh".to_string()))
}

/// Open a Terminal tab running `program args…` directly (e.g. `ssh host`, `docker exec -it c sh`).
/// Returns the tab id. Used by the Docker and SSH tracks (M11).
pub fn open_command(
    s: &mut Session,
    title: &str,
    program: &str,
    args: &[String],
    cwd: Option<PathBuf>,
) -> Result<u64, CommandError> {
    open(
        s,
        NewTab {
            cwd,
            command: Some(program.to_string()),
            args: Some(args.to_vec()),
            title: Some(title.to_string()),
            renamed: false,
            env: Vec::new(),
        },
    )
}

/// Open a tab; returns its id. Shows the Terminal tool window and asks the UI to focus it.
pub(crate) fn open(s: &mut Session, new: NewTab) -> Result<u64, CommandError> {
    if s.terminal.tabs.len() >= MAX_TABS {
        return Err(CommandError::Failed(format!("too many terminal tabs (at most {MAX_TABS})")));
    }
    let cwd = new.cwd.clone().unwrap_or_else(|| default_cwd(s));
    if !cwd.is_dir() {
        return Err(CommandError::Failed(format!("not a directory: {}", cwd.display())));
    }
    let shell = shell(s);
    let (program, args) = match (&new.command, &new.args) {
        (None, _) => (None, Vec::new()),
        (Some(program), Some(args)) => (Some(program.clone()), args.clone()),
        // A command line runs in the user's login shell, so their PATH and aliases apply.
        (Some(line), None) => {
            let sh = codecraft_term::default_shell(shell.as_deref());
            (Some(sh), vec!["-l".into(), "-i".into(), "-c".into(), line.clone()])
        }
    };
    let mut env = s.ide.env_for_child();
    env.extend(new.env.iter().cloned());
    let cfg = TermConfig {
        program,
        args,
        shell,
        cwd: Some(cwd.clone()),
        env,
        env_remove: ENV_REMOVE.iter().map(|k| (*k).to_string()).collect(),
        cols: 80,
        rows: 24,
        scrollback: s.terminal.scrollback,
    };
    let term = Terminal::spawn(cfg, s.terminal.waker.clone(), s.terminal.palette.clone())
        .map_err(|e| CommandError::Failed(e.to_string()))?;
    let id = s.terminal.next_id;
    s.terminal.next_id += 1;
    let title = new.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let renamed = new.renamed && title.is_some();
    let name = unique_name(s, title, new.command.as_deref());
    let screen = term.screen();
    s.terminal.tabs.push(TerminalTab {
        id,
        name,
        renamed,
        osc_title: None,
        cwd,
        command: new.command.clone(),
        status: TabStatus::Running,
        screen,
        bell: false,
        term,
    });
    s.terminal.active = s.terminal.tabs.len() - 1;
    s.terminal.focus_requested = true;
    s.ws.layout.show(ToolWindowId::Terminal);
    Ok(id)
}

/// Rename a tab; a blank `title` goes back to the automatic name (and the program's title).
pub(crate) fn rename(s: &mut Session, index: usize, title: &str) {
    let title: String = title.trim().chars().take(100).collect();
    let Some(tab) = s.terminal.tabs.get_mut(index) else { return };
    if !title.is_empty() {
        tab.name = title;
        tab.renamed = true;
        return;
    }
    tab.name.clear();
    tab.renamed = false;
    let command = tab.command.clone();
    let name = unique_name(s, None, command.as_deref());
    if let Some(tab) = s.terminal.tabs.get_mut(index) {
        tab.name = name;
    }
}

/// RustRover names shell tabs "Local", "Local (2)", …; commands are named after the program.
fn unique_name(s: &Session, title: Option<&str>, command: Option<&str>) -> String {
    let base = match (title, command) {
        (Some(t), _) => t.chars().take(100).collect(),
        (None, None) => "Local".to_string(),
        (None, Some(c)) => {
            let first = c.split_whitespace().next().unwrap_or(c);
            Path::new(first).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| first.to_string())
        }
    };
    let taken = |n: &str| s.terminal.tabs.iter().any(|t| t.name == n);
    if !taken(&base) {
        return base;
    }
    (2..).map(|i| format!("{base} ({i})")).find(|n| !taken(n)).unwrap_or(base)
}

/// Close a tab (killing its processes in the background). Closing the last tab hides the tool
/// window, like RustRover.
pub(crate) fn close(s: &mut Session, index: usize) -> bool {
    let closed = s.terminal.close_index(index);
    if closed && s.terminal.tabs.is_empty() {
        s.ws.layout.hide(ToolWindowId::Terminal);
        s.effects.push(Effect::FocusEditor);
    }
    closed
}

/// Drain background events; true if anything changed.
pub(crate) fn pump(s: &mut Session) -> bool {
    let mut changed = false;
    let mut effects = Vec::new();
    for tab in &mut s.terminal.tabs {
        tab.term.tick();
        for event in tab.term.poll_events() {
            changed = true;
            match event {
                TermEvent::Title(t) => tab.osc_title = Some(t.chars().take(200).collect()),
                TermEvent::ResetTitle => tab.osc_title = None,
                TermEvent::Bell => tab.bell = true,
                TermEvent::Clipboard(text) => effects.push(Effect::CopyToClipboard(text)),
                TermEvent::Cwd(dir) => {
                    if dir.is_dir() {
                        tab.cwd = dir;
                    }
                }
                TermEvent::Exited(info) => tab.status = TabStatus::Exited(info),
            }
        }
        if tab.term.take_dirty() {
            tab.refresh();
            changed = true;
        }
    }
    s.effects.extend(effects);
    let before = s.terminal.closing.len();
    s.terminal.closing.retain(|rx| matches!(rx.try_recv(), Err(crossbeam_channel::TryRecvError::Empty)));
    changed | (before != s.terminal.closing.len())
}

/// Stop background work (window closing): terminate every tab and wait (bounded) until their
/// processes are gone.
pub(crate) fn shutdown(s: &mut Session) {
    while !s.terminal.tabs.is_empty() {
        s.terminal.close_index(0);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    for rx in std::mem::take(&mut s.terminal.closing) {
        let left = deadline.saturating_duration_since(Instant::now());
        let _ = rx.recv_timeout(left);
    }
}

/// Wait (bounded) for background terminations; for tests.
pub fn wait_closed(s: &mut Session, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    for rx in std::mem::take(&mut s.terminal.closing) {
        let left = deadline.saturating_duration_since(Instant::now());
        if !matches!(rx.recv_timeout(left), Err(crossbeam_channel::RecvTimeoutError::Disconnected)) {
            return false;
        }
    }
    true
}
