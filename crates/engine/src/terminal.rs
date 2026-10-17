//! Terminal sessions (PTY + emulator) behind the Terminal tool window. OWNED BY the terminal track.
//!
//! Each tab is a [`codecraft_term::Terminal`]: a login shell (or a program such as `claude`) on a
//! PTY, started in the project root with the IDE server's environment, so `claude` run there
//! connects to CodeCraft. Output is parsed on background threads; [`pump`] takes a fresh
//! [`Screen`] snapshot for each tab whose output changed.
//!
//! Shell and Claude Code tabs are part of the project's saved session ([`snapshot`]/[`restore`]):
//! their names, working directories, own command histories and earlier output come back, each
//! with a fresh shell, when the project opens again, and a Claude Code session that ran in a tab
//! is resumed there ([`crate::terminal_shell`]). The
//! directory follows `cd` through OSC 7 reports and, for shells that don't send them, a
//! background look at the shell process every [`CWD_POLL`].

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::Serialize;
use serde_json::{Value, json};

use codecraft_term::{ExitInfo, Palette, Screen, SharedPalette, SharedWaker, TermConfig, TermEvent, Terminal};
use codecraft_workspace::{ClaudeSnapshot, MAX_TERMINALS, TerminalKind, TerminalSnapshot, ToolWindowId};

use crate::terminal_shell as shell_int;
use crate::{CommandError, Effect, Session};

/// Open tabs are capped so a runaway script or agent cannot fork shells without bound.
pub const MAX_TABS: usize = 32;
/// How often the shells' working directories are read while tabs are open.
pub const CWD_POLL: Duration = Duration::from_secs(5);

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
    /// Stable across restarts: the tab's own command history and saved output are kept under it.
    pub key: String,
    /// The tab runs Claude Code itself (Open Claude Code in Terminal).
    pub claude: bool,
    /// The Claude Code session the tab was started with (`--session-id`/`--resume`), until the
    /// session hook reports the current one.
    pub claude_session: Option<String>,
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
    /// A working-directory lookup running in the background, and when the last one started.
    cwd_poll: Option<Receiver<Vec<(u32, PathBuf)>>>,
    cwd_polled: Option<Instant>,
    /// Where tab histories, saved output and Claude Code session records go (default:
    /// `terminals/` in the config directory, in sessions with background services only).
    pub data_dir: Option<PathBuf>,
    /// Extra environment for every tab (tests point `HOME` at a temporary directory).
    pub env: Vec<(String, String)>,
    /// The shell integration directory written this session.
    pub(crate) installed: Option<PathBuf>,
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
            cwd_poll: None,
            cwd_polled: None,
            data_dir: None,
            env: Vec::new(),
            installed: None,
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
    /// Open without selecting the tab, showing the tool window or taking focus (session restore).
    pub background: bool,
    /// Extra environment (e.g. an enriched `PATH`).
    pub env: Vec<(String, String)>,
    /// The tab's key (a restored tab's); default: a new one.
    pub key: Option<String>,
    /// `command` is Claude Code, started with this session.
    pub claude: Option<String>,
    /// Text shown above the program's output (a restored tab's earlier output).
    pub preface: Option<String>,
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
            ..NewTab::default()
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
    let key = new.key.clone().filter(|k| shell_int::valid_id(k)).unwrap_or_else(shell_int::new_key);
    let integration = if new.command.is_none() || new.claude.is_some() { shell_int::install(s) } else { None };
    let mut extra_env = Vec::new();
    let mut shell_program = None;
    if let Some(integration) = &integration {
        if new.command.is_none() {
            let resolved = codecraft_term::default_shell(shell.as_deref());
            let launch = shell_int::shell_launch(s, integration, &resolved, &key);
            shell_program = launch.program;
            extra_env = launch.env;
        } else {
            extra_env = shell_int::common_env(s, integration, &key);
            // Claude Code is the tab's program: the hook records "the program" (no pid), and a
            // `claude` run inside it is not tracked.
            extra_env.push((shell_int::ENV_CLAUDE_PID.into(), "0".into()));
        }
    }
    let (program, args) = match (&new.command, &new.args) {
        (None, _) => match shell_program {
            Some((program, args)) => (Some(program), args),
            None => (None, Vec::new()),
        },
        (Some(program), Some(args)) => (Some(program.clone()), args.clone()),
        // A command line runs in the user's login shell, so their PATH and aliases apply.
        (Some(line), None) => {
            let sh = codecraft_term::default_shell(shell.as_deref());
            (Some(sh), vec!["-l".into(), "-i".into(), "-c".into(), line.clone()])
        }
    };
    let mut env = s.ide.env_for_child();
    env.extend(new.env.iter().cloned());
    env.extend(s.terminal.env.iter().cloned());
    if let Some(integration) = &integration {
        // The `claude` shim first on PATH (the integration's rc files put it first again after
        // the user's own startup files).
        let path = env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        let bin = integration.bin().to_string_lossy().into_owned();
        env.retain(|(k, _)| k != "PATH");
        env.push(("PATH".into(), if path.is_empty() { bin } else { format!("{bin}:{path}") }));
    }
    env.extend(extra_env.into_iter().filter(|(k, _)| k != "PATH"));
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
        preface: new.preface.clone(),
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
        key,
        claude: new.claude.is_some(),
        claude_session: new.claude.clone().filter(|c| !c.is_empty()),
        status: TabStatus::Running,
        screen,
        bell: false,
        term,
    });
    if !new.background {
        s.terminal.active = s.terminal.tabs.len() - 1;
        s.terminal.focus_requested = true;
        s.ws.layout.show(ToolWindowId::Terminal);
    }
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
    if let Some(key) = s.terminal.tabs.get(index).map(|t| t.key.clone()) {
        forget_files(s, &key);
    }
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
    poll_cwds(s);
    let before = s.terminal.closing.len();
    s.terminal.closing.retain(|rx| matches!(rx.try_recv(), Err(crossbeam_channel::TryRecvError::Empty)));
    changed | (before != s.terminal.closing.len())
}

/// Apply a finished working-directory lookup; start the next one every [`CWD_POLL`].
fn poll_cwds(s: &mut Session) {
    if let Some(rx) = &s.terminal.cwd_poll {
        match rx.try_recv() {
            Ok(found) => {
                apply_cwds(&mut s.terminal, &found);
                s.terminal.cwd_poll = None;
            }
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => s.terminal.cwd_poll = None,
        }
    }
    if s.terminal.cwd_polled.is_some_and(|t| t.elapsed() < CWD_POLL) {
        return;
    }
    let pids = shell_pids(&s.terminal);
    if pids.is_empty() {
        return;
    }
    s.terminal.cwd_polled = Some(Instant::now());
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("term-cwd".into()).spawn(move || {
        let _ = tx.send(codecraft_term::process_cwds(&pids));
    });
    match spawned {
        Ok(_) => s.terminal.cwd_poll = Some(rx),
        Err(e) => log::debug!("cannot start the terminal cwd lookup: {e}"),
    }
}

/// The processes of live shell tabs (the ones a session snapshot keeps).
fn shell_pids(state: &TerminalState) -> Vec<u32> {
    state.tabs.iter().filter(|t| t.command.is_none() && t.alive()).filter_map(|t| t.term.pid()).collect()
}

fn apply_cwds(state: &mut TerminalState, found: &[(u32, PathBuf)]) {
    for tab in &mut state.tabs {
        let Some(pid) = tab.term.pid() else { continue };
        if let Some((_, dir)) = found.iter().find(|(p, _)| *p == pid)
            && dir.is_dir()
        {
            tab.cwd = dir.clone();
        }
    }
}

/// Read the shells' working directories now (blocking briefly; before a final save).
pub(crate) fn refresh_cwds(s: &mut Session) {
    let found = codecraft_term::process_cwds(&shell_pids(&s.terminal));
    apply_cwds(&mut s.terminal, &found);
}

/// The live shell and Claude Code tabs for the session snapshot, and which of them is selected.
/// Tabs running another program (`ssh`, `docker exec`, a command) are not kept: they would run
/// again.
pub fn snapshot(s: &Session) -> (Vec<TerminalSnapshot>, Option<usize>) {
    let mut out = Vec::new();
    let mut active = None;
    for (i, tab) in s.terminal.tabs.iter().enumerate() {
        if (tab.command.is_some() && !tab.claude) || !tab.alive() {
            continue;
        }
        if i == s.terminal.active {
            active = Some(out.len());
        }
        out.push(TerminalSnapshot {
            key: Some(tab.key.clone()),
            name: tab.renamed.then(|| tab.name.clone()),
            cwd: tab.cwd.clone(),
            kind: if tab.claude { TerminalKind::ClaudeCode } else { TerminalKind::Shell },
            claude: claude_session(s, tab),
        });
    }
    (out, active)
}

/// The Claude Code session running in `tab`: the one its hook reported (for a shell tab, only
/// while that claude process still runs in the tab), else the one a Claude Code tab started with.
fn claude_session(s: &Session, tab: &TerminalTab) -> Option<ClaudeSnapshot> {
    let state = shell_int::claude_state_file(s, &tab.key).and_then(|p| shell_int::read_claude_state(&p));
    if tab.claude {
        return match state {
            Some(st) if st.pid.is_none() => Some(ClaudeSnapshot { session: st.session, transcript: st.transcript }),
            _ => tab.claude_session.clone().map(|session| ClaudeSnapshot { session, transcript: None }),
        };
    }
    let st = state?;
    let (shell, pid) = (tab.term.pid()?, st.pid?);
    codecraft_term::runs_under(shell, pid).then_some(ClaudeSnapshot { session: st.session, transcript: st.transcript })
}

/// A saved session that can be resumed: a plain id whose transcript (when known) still exists.
fn resumable(claude: Option<&ClaudeSnapshot>) -> Option<String> {
    let c = claude?;
    (shell_int::valid_id(&c.session) && c.transcript.as_ref().is_none_or(|t| t.is_file())).then(|| c.session.clone())
}

/// Before the final save: keep each shell tab's output (the end of its scrollback) for the next
/// start, and drop the files of tabs that are gone. Tabs running Claude Code keep none: the
/// resumed session shows its conversation itself.
pub(crate) fn save_tab_files(s: &mut Session) {
    let (saved, _) = snapshot(s);
    for tab in &s.terminal.tabs {
        let Some(path) = shell_int::output_file(s, &tab.key) else { continue };
        let keep = !tab.claude
            && tab.command.is_none()
            && saved.iter().any(|t| t.key.as_deref() == Some(tab.key.as_str()) && t.claude.is_none());
        let text = if keep { shell_int::output_tail(&tab.term.text(true)) } else { String::new() };
        if text.is_empty() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        if let Some(dir) = path.parent()
            && let Err(e) = std::fs::create_dir_all(dir)
        {
            log::warn!("cannot create {}: {e}", dir.display());
            continue;
        }
        if let Err(e) = codecraft_fs::write_text_file_atomic(&path, &text) {
            log::warn!("cannot save the output of terminal tab {}: {e}", tab.name);
        }
    }
    let keys: Vec<String> = s.terminal.tabs.iter().map(|t| t.key.clone()).collect();
    shell_int::prune(s, &keys);
}

/// Start a shell (or Claude Code) for each saved tab, with its name, in its directory (the
/// project root when the directory is gone), with its own history and earlier output; a Claude
/// Code session that ran in it is resumed (`claude --resume <id>`). Nothing happens when
/// terminals are already open. Returns how many tabs were opened.
pub(crate) fn restore(s: &mut Session, saved: &[TerminalSnapshot], active: Option<usize>) -> usize {
    if !s.terminal.tabs.is_empty() {
        return 0;
    }
    let mut opened = 0;
    let mut select = None;
    for (i, tab) in saved.iter().take(MAX_TERMINALS.min(MAX_TABS)).enumerate() {
        let name = tab.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(|n| n.chars().take(100).collect());
        let renamed = name.is_some();
        let cwd = Some(tab.cwd.clone()).filter(|d| d.is_absolute() && d.is_dir());
        let key = tab.key.clone().filter(|k| shell_int::valid_id(k));
        let resume = resumable(tab.claude.as_ref());
        let base = NewTab { cwd, title: name, renamed, background: true, key, ..NewTab::default() };
        let result = match tab.kind {
            TerminalKind::ClaudeCode => {
                crate::terminal_cmds::claude_tab(s, resume.as_deref(), base).and_then(|new| open(s, new))
            }
            TerminalKind::Shell => {
                let preface = match (&resume, base.key.as_deref().and_then(|k| shell_int::output_file(s, k))) {
                    (None, Some(path)) => {
                        std::fs::read_to_string(path).ok().and_then(|t| shell_int::output_preface(&t))
                    }
                    _ => None,
                };
                let opened = open(s, NewTab { preface, ..base });
                if let (Ok(id), Some(session)) = (&opened, &resume)
                    && let Some(t) = s.terminal.tabs.iter().find(|t| t.id == *id)
                {
                    // Typed into the shell (it reads it once ready), so the tab is back at its
                    // prompt when Claude Code exits.
                    t.term.send_text(&format!("claude --resume {session}\r"));
                }
                opened
            }
        };
        match result {
            Ok(_) => {
                if active == Some(i) {
                    select = Some(opened);
                }
                opened += 1;
            }
            Err(e) => log::warn!("cannot restore a terminal tab: {e}"),
        }
    }
    s.terminal.active = select.unwrap_or(0);
    opened
}

/// A tab the user closed: its history, saved output and session record go too.
fn forget_files(s: &Session, key: &str) {
    for path in [shell_int::history_file(s, key), shell_int::output_file(s, key), shell_int::claude_state_file(s, key)]
        .into_iter()
        .flatten()
    {
        let _ = std::fs::remove_file(path);
    }
}

/// The project is closing: its terminals go with it (the session saved them first).
pub(crate) fn close_all(s: &mut Session) {
    while !s.terminal.tabs.is_empty() {
        s.terminal.close_index(0);
    }
    s.terminal.active = 0;
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
