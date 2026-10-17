//! CodeCraft Terminal: PTY processes and a VT terminal emulator (grid, scrollback, colours) (L0, no workspace deps).
//!
//! A [`Terminal`] is one PTY session: a child process (a login shell or a program) on a
//! pseudo-terminal from `portable-pty`, and an `alacritty_terminal` emulator fed by a reader
//! thread. The UI thread never blocks: input goes to a writer thread, output is parsed on the
//! reader thread, the exit is observed on a waiter thread, and the owner takes cheap
//! [`Screen`] snapshots when [`Terminal::take_dirty`] says something changed.
//!
//! Why these crates: `alacritty_terminal`'s emulator is complete and battle-tested (256 and
//! truecolor, SGR attributes, cursor shapes, alternate screen, scroll regions, bracketed paste,
//! wide chars, mouse modes, OSC titles and hyperlinks, selection over scrollback, device status
//! replies), far beyond `vt100`. Its own tty layer is not used because it exits the process
//! when a resize ioctl fails and installs a global SIGCHLD handler; `portable-pty` returns
//! errors instead.
//!
//! Closing a terminal (or dropping it) terminates the child, its descendants and their process
//! groups (see `kill.rs`), so nothing is left behind.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]

mod cwd;
pub mod input;
#[cfg(unix)]
mod kill;
pub mod links;
mod screen;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as AlacrittyEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll as GridScroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config as TermOptions, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, CursorShape as AnsiCursorShape, CursorStyle, Handler, Rgb};
use crossbeam_channel::{Receiver, Sender, unbounded};
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use serde::Serialize;

pub use cwd::process_cwds;
pub use input::{Key, Mods, MouseAction, MouseButton, encode_key, encode_mouse, parse_key};
pub use links::{Link, LinkTarget, find_links};
pub use screen::{Cell, CellFlags, Color, CursorShape, Modes, Screen, ScreenCursor, ScreenSelection, xterm_rgb};

/// Smallest and largest grid sizes accepted (the emulator needs at least 2 columns).
pub const MIN_COLS: u16 = 2;
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;

/// How long closed processes get to exit after SIGHUP before SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(300);

#[derive(Debug, thiserror::Error)]
pub enum TermError {
    #[error("cannot open a pseudo-terminal: {0}")]
    Pty(String),
    #[error("cannot start `{program}`: {message}")]
    Spawn { program: String, message: String },
    #[error("cannot start the terminal's threads: {0}")]
    Thread(#[from] std::io::Error),
}

/// Called from background threads when the screen changed (the UI requests a repaint).
pub type Waker = Arc<dyn Fn() + Send + Sync>;
/// A waker slot shared by every terminal; the UI fills it once.
pub type SharedWaker = Arc<Mutex<Option<Waker>>>;

/// RGB colours for OSC 10/11/4 queries (`vim` and others ask for the background to pick a
/// light or dark scheme). The UI keeps it in sync with the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub cursor: (u8, u8, u8),
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for Palette {
    fn default() -> Self {
        let mut ansi = [(0, 0, 0); 16];
        for (i, c) in ansi.iter_mut().enumerate() {
            *c = xterm_rgb(i as u8);
        }
        Self { fg: (0xbc, 0xbe, 0xc4), bg: (0x1e, 0x1f, 0x22), cursor: (0xce, 0xd0, 0xd6), ansi }
    }
}

pub type SharedPalette = Arc<RwLock<Palette>>;

/// What to run and where.
#[derive(Clone, Debug, Default)]
pub struct TermConfig {
    /// A program to run instead of the login shell (resolved on `PATH` from `env` if relative).
    pub program: Option<String>,
    pub args: Vec<String>,
    /// The login shell when `program` is `None` (default: [`default_shell`]).
    pub shell: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Extra environment (overrides the inherited one).
    pub env: Vec<(String, String)>,
    /// Inherited variables to drop.
    pub env_remove: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    /// Scrollback lines.
    pub scrollback: usize,
}

/// How the child process ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExitInfo {
    /// The exit code, if it exited normally.
    pub code: Option<i32>,
    /// The terminating signal (`SIGKILL`, …), if killed by one.
    pub signal: Option<String>,
}

impl ExitInfo {
    pub fn describe(&self) -> String {
        match (&self.code, &self.signal) {
            (Some(c), _) => format!("exit code {c}"),
            (None, Some(s)) => format!("terminated by {s}"),
            (None, None) => "exited".to_string(),
        }
    }
}

/// Things the owner may want to react to (drained with [`Terminal::poll_events`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TermEvent {
    /// OSC 0/2 window title.
    Title(String),
    ResetTitle,
    Bell,
    /// OSC 52 clipboard store (copy requested by the program).
    Clipboard(String),
    /// OSC 7 working directory report.
    Cwd(PathBuf),
    /// The child process ended.
    Exited(ExitInfo),
}

/// Viewport scrolling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scroll {
    /// Positive: up into the scrollback; negative: down.
    Lines(i32),
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// Mouse selection granularity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    /// Double click: words.
    Word,
    /// Triple click: lines.
    Line,
    /// Alt-drag: a rectangle.
    Block,
}

/// The shell to start: `preferred`, then `$SHELL`, `/bin/zsh`, `/bin/bash`, `/bin/sh`
/// (the first executable one).
pub fn default_shell(preferred: Option<&str>) -> String {
    let env_shell = std::env::var("SHELL").ok();
    [preferred, env_shell.as_deref(), Some("/bin/zsh"), Some("/bin/bash"), Some("/bin/sh")]
        .into_iter()
        .flatten()
        .find(|s| is_executable(Path::new(s)))
        .unwrap_or("/bin/sh")
        .to_string()
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else { return false };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// State shared between the terminal, its threads and the emulator's event listener.
struct Shared {
    events: Sender<TermEvent>,
    input: Sender<Vec<u8>>,
    palette: SharedPalette,
    waker: SharedWaker,
    dirty: AtomicBool,
    exited: AtomicBool,
    /// cols, rows, cell width, cell height (px) for size reports.
    size: [AtomicU32; 4],
}

impl Shared {
    fn wake(&self) {
        let waker = lock(&self.waker).clone();
        if let Some(w) = waker {
            w();
        }
    }

    fn window_size(&self) -> WindowSize {
        let get = |i: usize| self.size.get(i).map_or(0, |a| a.load(Ordering::Relaxed)).min(u32::from(u16::MAX)) as u16;
        WindowSize { num_cols: get(0), num_lines: get(1), cell_width: get(2), cell_height: get(3) }
    }

    fn color(&self, index: usize) -> Rgb {
        let p = *self.palette.read().unwrap_or_else(PoisonError::into_inner);
        let (r, g, b) = match index {
            0..=15 => p.ansi.get(index).copied().unwrap_or(p.fg),
            16..=255 => xterm_rgb(index as u8),
            256 => p.fg,
            257 => p.bg,
            _ => p.cursor,
        };
        Rgb { r, g, b }
    }
}

#[derive(Clone)]
struct Listener(Arc<Shared>);

impl EventListener for Listener {
    fn send_event(&self, event: AlacrittyEvent) {
        let s = &self.0;
        match event {
            AlacrittyEvent::PtyWrite(text) => {
                let _ = s.input.send(text.into_bytes());
            }
            AlacrittyEvent::Title(t) => {
                let _ = s.events.send(TermEvent::Title(t));
            }
            AlacrittyEvent::ResetTitle => {
                let _ = s.events.send(TermEvent::ResetTitle);
            }
            AlacrittyEvent::Bell => {
                let _ = s.events.send(TermEvent::Bell);
            }
            AlacrittyEvent::ClipboardStore(_, text) => {
                let _ = s.events.send(TermEvent::Clipboard(text));
            }
            AlacrittyEvent::ColorRequest(index, format) => {
                let _ = s.input.send(format(s.color(index)).into_bytes());
            }
            AlacrittyEvent::TextAreaSizeRequest(format) => {
                let _ = s.input.send(format(s.window_size()).into_bytes());
            }
            // Clipboard reads (OSC 52 paste) are refused: a program must not read the clipboard.
            _ => {}
        }
    }
}

struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The emulator and its parser (the parser lives here so a stalled synchronized update can be
/// flushed from the owner's thread).
struct Emu {
    term: Term<Listener>,
    parser: ansi::Processor,
}

/// A PTY session with its emulator.
pub struct Terminal {
    emu: Arc<Mutex<Emu>>,
    shared: Arc<Shared>,
    events: Receiver<TermEvent>,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pid: Option<u32>,
    cols: u16,
    rows: u16,
}

fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(MIN_COLS, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

impl Terminal {
    /// Start `cfg` on a new PTY. Output is parsed on a background thread; `waker` is called
    /// whenever the screen changes or an event is queued.
    pub fn spawn(cfg: TermConfig, waker: SharedWaker, palette: SharedPalette) -> Result<Self, TermError> {
        let (cols, rows) =
            clamp_size(if cfg.cols == 0 { 80 } else { cfg.cols }, if cfg.rows == 0 { 24 } else { cfg.rows });
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| TermError::Pty(e.to_string()))?;
        let (mut cmd, program) = match &cfg.program {
            None => {
                let shell = default_shell(cfg.shell.as_deref());
                let mut cmd = CommandBuilder::new_default_prog();
                cmd.env("SHELL", &shell);
                (cmd, shell)
            }
            Some(p) => {
                let mut cmd = CommandBuilder::new(p);
                cmd.args(&cfg.args);
                (cmd, p.clone())
            }
        };
        if let Some(cwd) = &cfg.cwd {
            cmd.cwd(cwd);
        }
        for key in &cfg.env_remove {
            cmd.env_remove(key);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "CodeCraft");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        if cmd.get_env("LANG").is_none_or(|v| v.is_empty()) {
            cmd.env("LANG", "en_US.UTF-8");
        }
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| TermError::Spawn { program: program.clone(), message: e.to_string() })?;
        // Our copy of the slave side must close, or the reader never sees EOF.
        drop(pair.slave);
        let pid = child.process_id();
        let master = pair.master;
        let reader = master.try_clone_reader().map_err(|e| TermError::Pty(e.to_string()))?;
        let writer = master.take_writer().map_err(|e| TermError::Pty(e.to_string()))?;

        let (events_tx, events) = unbounded();
        let (input_tx, input_rx) = unbounded::<Vec<u8>>();
        let shared = Arc::new(Shared {
            events: events_tx,
            input: input_tx,
            palette,
            waker,
            dirty: AtomicBool::new(true),
            exited: AtomicBool::new(false),
            size: [
                AtomicU32::new(u32::from(cols)),
                AtomicU32::new(u32::from(rows)),
                AtomicU32::new(8),
                AtomicU32::new(16),
            ],
        });
        let options = TermOptions {
            scrolling_history: cfg.scrollback.min(100_000),
            default_cursor_style: CursorStyle { shape: AnsiCursorShape::Block, blinking: true },
            ..TermOptions::default()
        };
        let size = GridSize { cols: usize::from(cols), rows: usize::from(rows) };
        let term = Term::new(options, &size, Listener(shared.clone()));
        let emu = Arc::new(Mutex::new(Emu { term, parser: ansi::Processor::new() }));

        let mut terminal = Self {
            emu: emu.clone(),
            shared: shared.clone(),
            events,
            master: Some(master),
            child: Some(child),
            pid,
            cols,
            rows,
        };
        // From here on, dropping `terminal` on an error kills the child.
        std::thread::Builder::new().name("term-writer".into()).spawn(move || writer_loop(writer, input_rx))?;
        let reader_shared = shared.clone();
        std::thread::Builder::new()
            .name("term-reader".into())
            .spawn(move || reader_loop(reader, emu, reader_shared))?;
        if let Some(pid) = pid {
            std::thread::Builder::new().name("term-waiter".into()).spawn(move || waiter_loop(pid, shared))?;
        } else if let Some(child) = terminal.child.as_mut() {
            // No pid (non-Unix): poll once now; the reader's EOF marks the end.
            let _ = child.try_wait();
        }
        terminal.shared.wake();
        Ok(terminal)
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Whether the child process has exited.
    pub fn exited(&self) -> bool {
        self.shared.exited.load(Ordering::SeqCst)
    }

    /// True once after output, scrolling or selection changed the screen.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::SeqCst)
    }

    pub fn poll_events(&self) -> Vec<TermEvent> {
        self.events.try_iter().collect()
    }

    fn emu(&self) -> MutexGuard<'_, Emu> {
        lock(&self.emu)
    }

    fn mark_dirty(&self) {
        self.shared.dirty.store(true, Ordering::SeqCst);
    }

    /// Raw bytes to the PTY (queued; never blocks).
    pub fn write(&self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            let _ = self.shared.input.send(bytes);
        }
    }

    /// Typed input: snaps the view to the bottom like a terminal does.
    fn input(&self, bytes: Vec<u8>) {
        {
            let mut emu = self.emu();
            if emu.term.grid().display_offset() != 0 {
                emu.term.scroll_display(GridScroll::Bottom);
                self.mark_dirty();
            }
        }
        self.write(bytes);
    }

    /// Text as if typed (UTF-8, unchanged).
    pub fn send_text(&self, text: &str) {
        self.input(text.as_bytes().to_vec());
    }

    /// A key press, encoded for the current cursor-key mode. False if it has no encoding.
    pub fn send_key(&self, key: Key, mods: Mods) -> bool {
        let app_cursor = self.emu().term.mode().contains(TermMode::APP_CURSOR);
        match encode_key(key, mods, app_cursor) {
            Some(bytes) => {
                self.input(bytes);
                true
            }
            None => false,
        }
    }

    /// Paste: newlines become CR, and the text is bracketed when the program asked for it
    /// (escape sequences inside pasted text are dropped so it cannot end the bracket).
    pub fn paste(&self, text: &str) {
        let bracketed = self.emu().term.mode().contains(TermMode::BRACKETED_PASTE);
        let mut body = text.replace("\r\n", "\r").replace('\n', "\r");
        let mut out = Vec::with_capacity(body.len() + 12);
        if bracketed {
            body.retain(|c| c != '\x1b');
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(body.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
        } else {
            out.extend_from_slice(body.as_bytes());
        }
        self.input(out);
    }

    /// Focus in/out reports for programs that enabled them (mode 1004).
    pub fn focus_changed(&self, focused: bool) {
        let reporting = {
            let mut emu = self.emu();
            emu.term.is_focused = focused;
            emu.term.mode().contains(TermMode::FOCUS_IN_OUT)
        };
        if reporting {
            self.write(if focused { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
        }
    }

    /// Report a mouse event if the program enabled tracking for it. False if not reported.
    pub fn mouse(&self, button: MouseButton, action: MouseAction, col: usize, row: usize, mods: Mods) -> bool {
        let mode = *self.emu().term.mode();
        let wanted = match (action, button) {
            (MouseAction::Motion, MouseButton::None) => mode.contains(TermMode::MOUSE_MOTION),
            (MouseAction::Motion, _) => mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG),
            _ => mode.intersects(TermMode::MOUSE_MODE),
        };
        if !wanted {
            return false;
        }
        match encode_mouse(button, action, col, row, mods, mode.contains(TermMode::SGR_MOUSE)) {
            Some(bytes) => {
                self.write(bytes);
                true
            }
            None => false,
        }
    }

    /// Resize the grid and the PTY (the child gets SIGWINCH). `cell_px` is for size reports.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_px: (u16, u16)) {
        let (cols, rows) = clamp_size(cols, rows);
        if let (Some(a), Some(b)) = (self.shared.size.get(2), self.shared.size.get(3)) {
            a.store(u32::from(cell_px.0), Ordering::Relaxed);
            b.store(u32::from(cell_px.1), Ordering::Relaxed);
        }
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        if let (Some(a), Some(b)) = (self.shared.size.first(), self.shared.size.get(1)) {
            a.store(u32::from(cols), Ordering::Relaxed);
            b.store(u32::from(rows), Ordering::Relaxed);
        }
        self.emu().term.resize(GridSize { cols: usize::from(cols), rows: usize::from(rows) });
        if let Some(m) = &self.master {
            let size = PtySize {
                rows,
                cols,
                pixel_width: cols.saturating_mul(cell_px.0),
                pixel_height: rows.saturating_mul(cell_px.1),
            };
            if let Err(e) = m.resize(size) {
                log::debug!("terminal resize: {e}");
            }
        }
        self.mark_dirty();
    }

    /// A copy of the visible screen.
    pub fn screen(&self) -> Screen {
        let emu = self.emu();
        screen::snapshot(&emu.term)
    }

    pub fn scroll(&self, scroll: Scroll) {
        let s = match scroll {
            Scroll::Lines(n) => GridScroll::Delta(n),
            Scroll::PageUp => GridScroll::PageUp,
            Scroll::PageDown => GridScroll::PageDown,
            Scroll::Top => GridScroll::Top,
            Scroll::Bottom => GridScroll::Bottom,
        };
        self.emu().term.scroll_display(s);
        self.mark_dirty();
    }

    /// Scroll so that `offset` lines of scrollback are above the viewport bottom (0 = live).
    pub fn scroll_to(&self, offset: usize) {
        let mut emu = self.emu();
        let current = emu.term.grid().display_offset() as i64;
        let delta = (offset as i64 - current).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        if delta != 0 {
            emu.term.scroll_display(GridScroll::Delta(delta));
            self.mark_dirty();
        }
    }

    /// Clear the scrollback and the screen above the cursor line (macOS Terminal's Cmd+K);
    /// the current line (the prompt) stays, moved to the top.
    pub fn clear(&self) {
        let mut emu = self.emu();
        let term = &mut emu.term;
        term.scroll_display(GridScroll::Bottom);
        term.selection = None;
        if !term.mode().contains(TermMode::ALT_SCREEN) {
            let cursor = term.grid().cursor.point;
            let line = usize::try_from(cursor.line.0).unwrap_or(0);
            if line > 0 {
                term.scroll_up(line);
                term.goto(0, cursor.column.0);
            }
        }
        term.grid_mut().clear_history();
        drop(emu);
        self.mark_dirty();
    }

    fn grid_point(term: &Term<Listener>, row: usize, col: usize) -> Point {
        let rows = term.grid().screen_lines();
        let cols = term.grid().columns();
        let offset = term.grid().display_offset() as i64;
        let line =
            (row.min(rows.saturating_sub(1)) as i64 - offset).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
        Point::new(Line(line), Column(col.min(cols.saturating_sub(1))))
    }

    /// Start a selection at a viewport cell (`right_half`: the pointer is on the cell's right half).
    pub fn start_selection(&self, kind: SelectionKind, row: usize, col: usize, right_half: bool) {
        let mut emu = self.emu();
        let point = Self::grid_point(&emu.term, row, col);
        let ty = match kind {
            SelectionKind::Simple => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
            SelectionKind::Block => SelectionType::Block,
        };
        let side = if right_half { Side::Right } else { Side::Left };
        emu.term.selection = Some(Selection::new(ty, point, side));
        drop(emu);
        self.mark_dirty();
    }

    /// Extend the selection to a viewport cell.
    pub fn update_selection(&self, row: usize, col: usize, right_half: bool) {
        let mut emu = self.emu();
        let point = Self::grid_point(&emu.term, row, col);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(sel) = emu.term.selection.as_mut() {
            sel.update(point, side);
        }
        drop(emu);
        self.mark_dirty();
    }

    pub fn clear_selection(&self) {
        self.emu().term.selection = None;
        self.mark_dirty();
    }

    /// Select the whole buffer (scrollback included).
    pub fn select_all(&self) {
        let mut emu = self.emu();
        let grid = emu.term.grid();
        let start = Point::new(grid.topmost_line(), Column(0));
        let end = Point::new(grid.bottommost_line(), grid.last_column());
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        emu.term.selection = Some(sel);
        drop(emu);
        self.mark_dirty();
    }

    /// The selected text, if anything non-empty is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.emu().term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// The buffer as text: the visible screen, or everything from the top of the scrollback.
    /// Trailing blanks are trimmed from each line and trailing empty lines are dropped.
    pub fn text(&self, scrollback: bool) -> String {
        let emu = self.emu();
        let grid = emu.term.grid();
        let top = if scrollback { grid.topmost_line() } else { Line(-(grid.display_offset() as i32)) };
        let bottom = if scrollback { grid.bottommost_line() } else { top + (grid.screen_lines() as i32 - 1) };
        let raw = emu.term.bounds_to_string(Point::new(top, Column(0)), Point::new(bottom, grid.last_column()));
        let lines: Vec<&str> = raw.lines().map(str::trim_end).collect();
        let end = lines.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
        lines.get(..end).unwrap_or(&[]).join("\n")
    }

    /// Flush a synchronized update (DEC mode 2026) whose timeout passed while no output arrived.
    pub fn tick(&self) {
        let mut emu = self.emu();
        let expired = emu.parser.sync_timeout().sync_timeout().is_some_and(|t| t <= Instant::now());
        if expired {
            let Emu { term, parser } = &mut *emu;
            parser.stop_sync(term);
            drop(emu);
            self.mark_dirty();
        }
    }

    /// Terminate the child and everything it started, in the background. The returned channel
    /// closes when that is done.
    pub fn terminate(&mut self) -> Receiver<()> {
        let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
        let master = self.master.take();
        let child = self.child.take();
        let pid = self.pid.take();
        let shared = self.shared.clone();
        if master.is_none() && child.is_none() {
            return done_rx;
        }
        let job = move || {
            let mut child = child;
            #[cfg(unix)]
            if let Some(pid) = pid {
                kill::kill_tree(pid, KILL_GRACE, &shared.exited);
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
                if let Some(c) = child.as_mut() {
                    let _ = c.kill();
                }
            }
            // Reap the child (it is dead or dying), then close the PTY.
            if let Some(c) = child.as_mut() {
                let _ = c.wait();
            }
            shared.exited.store(true, Ordering::SeqCst);
            drop(master);
            drop(done_tx);
        };
        if let Err(e) = std::thread::Builder::new().name("term-kill".into()).spawn(job) {
            log::warn!("cannot start the terminal kill thread: {e}");
        }
        done_rx
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn writer_loop(mut writer: Box<dyn Write + Send>, input: Receiver<Vec<u8>>) {
    for bytes in input {
        if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
            break;
        }
    }
}

fn reader_loop(mut reader: Box<dyn Read + Send>, emu: Arc<Mutex<Emu>>, shared: Arc<Shared>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let Some(bytes) = buf.get(..n) else { break };
                {
                    let mut guard = lock(&emu);
                    let Emu { term, parser } = &mut *guard;
                    parser.advance(term, bytes);
                }
                if let Some(cwd) = osc7_cwd(bytes) {
                    let _ = shared.events.send(TermEvent::Cwd(cwd));
                }
                shared.dirty.store(true, Ordering::SeqCst);
                shared.wake();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            // EIO once the child side is closed.
            Err(_) => break,
        }
    }
    shared.dirty.store(true, Ordering::SeqCst);
    shared.wake();
}

#[cfg(unix)]
fn waiter_loop(pid: u32, shared: Arc<Shared>) {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let info = match i32::try_from(pid).ok().and_then(Pid::from_raw) {
        None => ExitInfo { code: None, signal: None },
        Some(p) => loop {
            // NOWAIT leaves the child a zombie, so its pid (and process group id) cannot be
            // reused before `terminate` has signalled the tree; `terminate` reaps it.
            match waitid(WaitId::Pid(p), WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
                Ok(Some(st)) => {
                    break ExitInfo {
                        code: if st.exited() { st.exit_status() } else { None },
                        signal: if st.exited() { None } else { st.terminating_signal().map(signal_name) },
                    };
                }
                Err(rustix::io::Errno::INTR) | Ok(None) => continue,
                Err(_) => break ExitInfo { code: None, signal: None },
            }
        },
    };
    shared.exited.store(true, Ordering::SeqCst);
    let _ = shared.events.send(TermEvent::Exited(info));
    shared.dirty.store(true, Ordering::SeqCst);
    shared.wake();
}

#[cfg(not(unix))]
fn waiter_loop(_pid: u32, shared: Arc<Shared>) {
    // Without a non-reaping wait, the reader's EOF is the exit signal.
    shared.wake();
}

// Only the Unix waiter reports signals (dead code elsewhere).
#[cfg(unix)]
fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        3 => "SIGQUIT".into(),
        4 => "SIGILL".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        10 if cfg!(target_os = "macos") => "SIGBUS".into(),
        11 => "SIGSEGV".into(),
        13 => "SIGPIPE".into(),
        15 => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}

/// The directory from an OSC 7 report (`ESC ] 7 ; file://host/path BEL`) in a chunk of output.
fn osc7_cwd(bytes: &[u8]) -> Option<PathBuf> {
    const START: &[u8] = b"\x1b]7;";
    let at = bytes.windows(START.len()).rposition(|w| w == START)?;
    let rest = bytes.get(at + START.len()..)?;
    let end = rest.iter().position(|b| *b == 0x07 || *b == 0x1b)?;
    let uri = std::str::from_utf8(rest.get(..end)?).ok()?;
    let after_scheme = uri.strip_prefix("file://")?;
    let path = after_scheme.get(after_scheme.find('/')?..)?;
    let decoded = percent_decode(path)?;
    Some(PathBuf::from(decoded))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn osc7() {
        assert_eq!(osc7_cwd(b"x\x1b]7;file://host/Users/a%20b/src\x07y"), Some(PathBuf::from("/Users/a b/src")));
        assert_eq!(osc7_cwd(b"\x1b]7;file:///tmp\x1b\\"), Some(PathBuf::from("/tmp")));
        assert_eq!(osc7_cwd(b"\x1b]7;file://host"), None);
        assert_eq!(osc7_cwd(b"\x1b]7;http://x/y\x07"), None);
        assert_eq!(osc7_cwd(b"\x1b]7;file:///bad%zz\x07"), None);
    }

    #[test]
    fn shells() {
        let s = default_shell(Some("/definitely/not/a/shell"));
        assert!(Path::new(&s).is_absolute());
        assert_eq!(default_shell(Some("/bin/sh")), "/bin/sh");
    }
}
