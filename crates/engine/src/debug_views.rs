//! CLion-style debugger views: Memory (hex + ASCII, go to an address or `&expr`, edit bytes,
//! paging), Disassembly (instructions around the program counter with source lines mixed in;
//! stepping by instruction while it is shown), Registers (the adapter's register scope, changed
//! values highlighted between stops) and watchpoints (break when a variable is written or read).
//! OWNED BY the debugger track.
//!
//! Requests go through the session's adapter like every other debugger request (see
//! [`crate::debug::view_request`]); answers come back through `Session::pump` into
//! [`on_response`]. Adapters that lack a request (Xcode's lldb-dap has no `readMemory`,
//! `writeMemory` or `dataBreakpointInfo`, and misaligns disassembly before the program counter)
//! get the equivalent LLDB console command instead (`codecraft_dap::lldb`).

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_dap::{self as dap, lldb, memory};
use codecraft_workspace::ToolWindowId;

use crate::commands::{CommandSpec, Context, always, ok, spec};
use crate::debug::{self, DebugConsoleKind, DebugTab};
use crate::params::{opt_str, req_str};
use crate::{CommandError, Session};

pub use codecraft_dap::lldb::WatchAccess;

/// Bytes shown per page of the Memory view.
pub const MEMORY_PAGE: usize = 256;
/// Bytes per row of the Memory view.
pub const BYTES_PER_ROW: usize = 16;
/// Bytes one `debug.memory.write` may change.
const MAX_WRITE: usize = 4096;
/// Instructions disassembled before and in all around the program counter.
const DISASSEMBLY_BEFORE: i64 = 24;
const DISASSEMBLY_COUNT: usize = 80;
const MAX_DISASSEMBLY_ROWS: usize = 4000;
const MAX_REGISTERS: usize = 2000;
const MAX_HISTORY: usize = 500;

// ---------------------------------------------------------------- data model

/// The Memory view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryView {
    /// What was asked for: an address (`0x…`, decimal) or an expression (`&x`, `ptr`).
    pub input: String,
    /// The first address shown.
    pub address: Option<u64>,
    /// The page's bytes; `None` where memory could not be read.
    pub bytes: Vec<Option<u8>>,
    /// Offsets into `bytes` that differ from the previous read of the same page.
    pub changed: BTreeSet<usize>,
    pub loading: bool,
    pub error: Option<String>,
    /// The bytes came from the LLDB console (`memory read`), not `readMemory`.
    pub via_console: bool,
}

impl MemoryView {
    /// `(address, bytes)` per row of [`BYTES_PER_ROW`].
    pub fn rows(&self) -> Vec<(u64, &[Option<u8>])> {
        let Some(start) = self.address else { return Vec::new() };
        self.bytes
            .chunks(BYTES_PER_ROW)
            .enumerate()
            .map(|(i, chunk)| (start.saturating_add((i * BYTES_PER_ROW) as u64), chunk))
            .collect()
    }

    /// The byte at `address`, if it is on the page and was readable.
    pub fn byte_at(&self, address: u64) -> Option<u8> {
        let offset = usize::try_from(address.checked_sub(self.address?)?).ok()?;
        self.bytes.get(offset).copied().flatten()
    }
}

/// A row of the Disassembly view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum DisassemblyRow {
    /// A function label (`app::main`, ``t`main``).
    Symbol { name: String },
    /// A source line the following instructions were compiled from. `line` is zero-based.
    #[serde(rename_all = "camelCase")]
    Source { path: Option<PathBuf>, line: Option<usize>, text: String },
    #[serde(rename_all = "camelCase")]
    Instruction {
        address: u64,
        /// `e8 1b 40 b9`.
        bytes: Option<String>,
        text: String,
        /// `<+60>`: the offset in its function.
        offset: Option<String>,
    },
}

/// The Disassembly view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisassemblyView {
    pub rows: Vec<DisassemblyRow>,
    /// The selected frame's program counter (the highlighted instruction).
    pub current: Option<u64>,
    pub loading: bool,
    pub error: Option<String>,
    /// Disassembled by the LLDB console (`disassemble --frame`).
    pub via_console: bool,
}

impl DisassemblyView {
    /// Index of the current instruction's row.
    pub fn current_row(&self) -> Option<usize> {
        let current = self.current?;
        self.rows.iter().position(|r| matches!(r, DisassemblyRow::Instruction { address, .. } if *address == current))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Register {
    pub name: String,
    pub value: String,
    /// Differs from the previous stop.
    pub changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterGroup {
    pub name: String,
    pub registers: Vec<Register>,
}

/// The Registers view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistersView {
    pub groups: Vec<RegisterGroup>,
    pub loading: bool,
    pub error: Option<String>,
    /// Values at the previous stop (top frame), by `group/name`.
    #[serde(skip)]
    previous: HashMap<String, String>,
    /// Values at this stop (top frame).
    #[serde(skip)]
    current: HashMap<String, String>,
    /// Group references still being fetched → their index in `groups`.
    #[serde(skip)]
    waiting: std::collections::BTreeMap<i64, usize>,
}

/// A watchpoint: stop when a variable's memory is written (or read).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Watchpoint {
    pub id: u64,
    /// The variable as shown (`counter`, `point.x`).
    pub name: String,
    /// The expression that locates it (the variable's evaluate name).
    pub expression: String,
    pub access: WatchAccess,
    pub enabled: bool,
    /// What the adapter watches (`4 bytes at 16FDFCCF8 x`).
    pub description: Option<String>,
    /// Set in the adapter; `None` while being set or outside a session.
    pub verified: Option<bool>,
    pub message: Option<String>,
    /// The adapter's data id (`dataBreakpointInfo`).
    pub data_id: Option<String>,
    /// LLDB's watchpoint number when set through the console.
    pub lldb_id: Option<u32>,
}

/// State of the debugger views (on [`crate::debug::DebugState::views`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugViews {
    pub memory: MemoryView,
    pub disassembly: DisassemblyView,
    pub registers: RegistersView,
    pub watchpoints: Vec<Watchpoint>,
    /// Debug console input, oldest first.
    pub console_history: Vec<String>,
    #[serde(skip)]
    next_watchpoint: u64,
    /// The previous read of the memory page (for changed bytes).
    #[serde(skip)]
    previous_page: Option<(u64, Vec<Option<u8>>)>,
}

impl DebugViews {
    /// A new session: views empty, watchpoints gone (they name addresses of the old process).
    pub(crate) fn reset(&mut self) {
        let history = std::mem::take(&mut self.console_history);
        let input = std::mem::take(&mut self.memory.input);
        let next = self.next_watchpoint;
        *self = Self { console_history: history, next_watchpoint: next, ..Self::default() };
        self.memory.input = input;
    }

    /// The program stopped: registers compare against this stop from now on.
    pub(crate) fn on_stop(&mut self) {
        let r = &mut self.registers;
        if !r.current.is_empty() {
            r.previous = std::mem::take(&mut r.current);
        }
    }

    /// The program runs: nothing is current until it stops.
    pub(crate) fn on_resume(&mut self) {
        self.disassembly.current = None;
        self.registers.waiting.clear();
        self.registers.loading = false;
    }

    pub(crate) fn on_session_end(&mut self) {
        self.on_resume();
        self.memory.loading = false;
        self.disassembly.loading = false;
        for w in &mut self.watchpoints {
            w.verified = None;
            w.data_id = None;
            w.lldb_id = None;
        }
    }

    pub(crate) fn remember_command(&mut self, command: &str) {
        if self.console_history.last().map(String::as_str) != Some(command) {
            self.console_history.push(command.to_owned());
        }
        if self.console_history.len() > MAX_HISTORY {
            let excess = self.console_history.len() - MAX_HISTORY;
            self.console_history.drain(..excess);
        }
    }
}

/// What a view request is for.
#[derive(Clone, Debug)]
pub(crate) enum ViewRequest {
    /// Evaluating a Memory view location; `address_of`: this is the `&(…)` retry.
    MemoryEval {
        expression: String,
        address_of: bool,
    },
    MemoryRead {
        address: u64,
        console: bool,
    },
    MemoryWrite {
        console: bool,
    },
    Disassemble {
        ip: u64,
        console: bool,
    },
    /// The Registers scope's children (groups, or registers directly).
    RegisterScope {
        name: String,
    },
    RegisterGroup {
        reference: i64,
        top_frame: bool,
    },
    WatchInfo {
        id: u64,
    },
    SetWatchpoints {
        ids: Vec<u64>,
    },
    WatchCommand {
        id: u64,
        action: WatchAction,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchAction {
    Set,
    Delete,
    Enable(bool),
}

// ---------------------------------------------------------------- hooks from debug.rs

/// The scopes of the selected frame arrived: fetch the registers if they are shown.
pub(crate) fn on_scopes(s: &mut Session) {
    if s.debug.tab == DebugTab::Registers {
        request_registers(s);
    }
}

/// A frame became current: refresh the views that depend on it.
pub(crate) fn on_frame_selected(s: &mut Session) {
    if s.debug.tab == DebugTab::Disassembly {
        request_disassembly(s);
    }
}

/// The program stopped: read the shown memory page again (it doesn't depend on the frame).
pub(crate) fn after_stop(s: &mut Session) {
    if s.debug.tab == DebugTab::Memory
        && let Some(address) = s.debug.views.memory.address
        && let Err(e) = read_memory(s, address)
    {
        s.debug.views.memory.error = Some(e.to_string());
    }
}

// ---------------------------------------------------------------- memory

fn stopped(s: &Session) -> Result<(), CommandError> {
    if s.debug.is_stopped() {
        Ok(())
    } else {
        Err(CommandError::Disabled("the program is not paused in the debugger".into()))
    }
}

fn adapter(s: &Session) -> Result<debug::LiveAdapter, CommandError> {
    debug::live_adapter(s).ok_or_else(|| CommandError::Disabled("the debugger is not running".into()))
}

fn frame_id(s: &Session) -> Option<i64> {
    s.debug.session.as_ref().and_then(|x| x.selected_frame()).map(|f| f.id)
}

fn not_responding() -> CommandError {
    CommandError::Failed("the debugger is not responding".into())
}

/// Show the memory at an address or expression.
pub(crate) fn goto_memory(s: &mut Session, input: &str) -> Result<(), CommandError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(CommandError::BadParams("give an address or an expression".into()));
    }
    stopped(s)?;
    s.debug.views.memory.input = input.to_owned();
    s.debug.views.memory.error = None;
    if let Some(address) = memory::parse_address(input) {
        return read_memory(s, address);
    }
    let frame = frame_id(s);
    let expression = input.to_owned();
    let request = ViewRequest::MemoryEval { expression: expression.clone(), address_of: false };
    if !debug::view_request(s, request, |c| c.evaluate(&expression, frame, "watch")) {
        return Err(not_responding());
    }
    s.debug.views.memory.loading = true;
    Ok(())
}

/// Read a page at `address` (the Memory view's new start).
pub(crate) fn read_memory(s: &mut Session, address: u64) -> Result<(), CommandError> {
    let live = adapter(s)?;
    let console = !live.capabilities.supports_read_memory_request;
    if console && !live.is_lldb() {
        return Err(CommandError::Failed(format!("{} cannot read memory", live.name())));
    }
    let sent = if console {
        let command = lldb::memory_read_command(address, MEMORY_PAGE);
        let frame = frame_id(s);
        debug::view_request(s, ViewRequest::MemoryRead { address, console }, |c| c.evaluate(&command, frame, "repl"))
    } else {
        let reference = memory::reference(address);
        debug::view_request(s, ViewRequest::MemoryRead { address, console }, |c| {
            c.read_memory(&reference, 0, MEMORY_PAGE)
        })
    };
    if !sent {
        return Err(not_responding());
    }
    let view = &mut s.debug.views.memory;
    if view.address != Some(address) {
        view.bytes.clear();
        view.changed.clear();
    }
    view.address = Some(address);
    view.loading = true;
    view.error = None;
    Ok(())
}

/// Move the Memory view by `delta` pages.
pub(crate) fn page_memory(s: &mut Session, delta: i64) -> Result<(), CommandError> {
    let address = s.debug.views.memory.address.ok_or_else(|| CommandError::Disabled("no memory is shown".into()))?;
    let step = (MEMORY_PAGE as u64).saturating_mul(delta.unsigned_abs());
    let next = if delta < 0 { address.saturating_sub(step) } else { address.saturating_add(step) };
    read_memory(s, next)
}

/// Write bytes at `address`, then read the page and the variables again.
pub(crate) fn write_memory(s: &mut Session, address: u64, bytes: &[u8]) -> Result<(), CommandError> {
    stopped(s)?;
    if bytes.is_empty() || bytes.len() > MAX_WRITE {
        return Err(CommandError::BadParams(format!("write between 1 and {MAX_WRITE} bytes")));
    }
    let live = adapter(s)?;
    let console = !live.capabilities.supports_write_memory_request;
    if console && !live.is_lldb() {
        return Err(CommandError::Failed(format!("{} cannot write memory", live.name())));
    }
    let sent = if console {
        let command = lldb::memory_write_command(address, bytes);
        let frame = frame_id(s);
        debug::view_request(s, ViewRequest::MemoryWrite { console }, |c| c.evaluate(&command, frame, "repl"))
    } else {
        let reference = memory::reference(address);
        debug::view_request(s, ViewRequest::MemoryWrite { console }, |c| c.write_memory(&reference, 0, bytes))
    };
    if !sent {
        return Err(not_responding());
    }
    Ok(())
}

fn on_memory_eval(s: &mut Session, expression: String, address_of: bool, result: Result<Value, String>) {
    let body = match result {
        Ok(body) => dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default(),
        Err(e) => {
            memory_failed(s, e);
            return;
        }
    };
    let address = body
        .memory_reference
        .as_deref()
        .and_then(memory::parse_address)
        .or_else(|| memory::leading_address(&body.result));
    match address {
        Some(address) => {
            if let Err(e) = read_memory(s, address) {
                memory_failed(s, e.to_string());
            }
        }
        None if !address_of && !expression.starts_with('&') => {
            // A value, not a pointer: show where it lives.
            let retry = format!("&({expression})");
            let frame = frame_id(s);
            let request = ViewRequest::MemoryEval { expression: retry.clone(), address_of: true };
            if !debug::view_request(s, request, |c| c.evaluate(&retry, frame, "watch")) {
                memory_failed(s, "the debugger is not responding".into());
            }
        }
        None => {
            let shown = if body.result.is_empty() { "no value".to_owned() } else { body.result };
            memory_failed(s, format!("{expression} is not an address ({shown})"));
        }
    }
}

fn memory_failed(s: &mut Session, error: String) {
    let view = &mut s.debug.views.memory;
    view.loading = false;
    view.error = Some(error);
}

fn on_memory_read(s: &mut Session, address: u64, console: bool, result: Result<Value, String>) {
    if s.debug.views.memory.address != Some(address) {
        return; // the view moved on
    }
    let bytes: Result<Vec<Option<u8>>, String> = result.and_then(|body| {
        if console {
            let text = dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default().result;
            if let Some(e) = lldb::error_of(&text) {
                return Err(e);
            }
            let mut bytes: Vec<Option<u8>> = lldb::memory_bytes(&text, address).into_iter().map(Some).collect();
            bytes.truncate(MEMORY_PAGE);
            bytes.resize(MEMORY_PAGE, None);
            Ok(bytes)
        } else {
            let body: dap::ReadMemoryBody = dap::types::parse(&body)?;
            let start = memory::parse_address(&body.address).unwrap_or(address);
            let data = body.data.as_deref().map(memory::base64_decode).unwrap_or(Some(Vec::new()));
            let data = data.ok_or_else(|| "the debugger sent malformed memory".to_owned())?;
            // The adapter may start later than asked (unreadable bytes first).
            let skip = usize::try_from(start.saturating_sub(address)).unwrap_or(MEMORY_PAGE).min(MEMORY_PAGE);
            let mut bytes = vec![None; skip];
            bytes.extend(data.into_iter().map(Some));
            bytes.truncate(MEMORY_PAGE);
            bytes.resize(MEMORY_PAGE, None);
            Ok(bytes)
        }
    });
    let views = &mut s.debug.views;
    views.memory.loading = false;
    views.memory.via_console = console;
    match bytes {
        Ok(bytes) => {
            views.memory.changed = match &views.previous_page {
                Some((at, old)) if *at == address => bytes
                    .iter()
                    .zip(old)
                    .enumerate()
                    .filter(|(_, (new, old))| new.is_some() && old.is_some() && new != old)
                    .map(|(i, _)| i)
                    .collect(),
                _ => BTreeSet::new(),
            };
            if bytes.iter().all(Option::is_none) {
                views.memory.error = Some(format!("memory at 0x{address:x} cannot be read"));
            } else {
                views.memory.error = None;
            }
            views.previous_page = Some((address, bytes.clone()));
            views.memory.bytes = bytes;
        }
        Err(e) => {
            views.memory.bytes.clear();
            views.memory.changed.clear();
            views.memory.error = Some(e);
        }
    }
}

fn on_memory_write(s: &mut Session, console: bool, result: Result<Value, String>) {
    let error = match result {
        Ok(body) if console => {
            lldb::error_of(&dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default().result)
        }
        Ok(_) => None,
        Err(e) => Some(e),
    };
    if let Some(e) = error {
        s.debug.push_console_line(DebugConsoleKind::Error, format!("Could not write memory: {e}"));
        s.status = format!("Could not write memory: {e}");
        s.debug.views.memory.error = Some(e);
        return;
    }
    s.status = "Memory written".into();
    if let Some(address) = s.debug.views.memory.address {
        let _ = read_memory(s, address);
    }
    debug::reload_frame(s);
}

// ---------------------------------------------------------------- disassembly

/// Disassemble around the selected frame's program counter.
pub(crate) fn request_disassembly(s: &mut Session) {
    let ip = s
        .debug
        .session
        .as_ref()
        .and_then(|x| x.selected_frame())
        .and_then(|f| f.instruction_pointer.as_deref())
        .and_then(memory::parse_address);
    let stopped = s.debug.is_stopped();
    let view = &mut s.debug.views.disassembly;
    let Some(ip) = ip else {
        view.current = None;
        view.error = Some(if stopped {
            "The selected frame has no instruction address.".into()
        } else {
            "The disassembly is shown while the program is paused.".into()
        });
        return;
    };
    let Some(live) = debug::live_adapter(s) else { return };
    let console = !live.capabilities.supports_disassemble_request;
    if console && !live.is_lldb() {
        s.debug.views.disassembly.error = Some(format!("{} cannot disassemble", live.name()));
        return;
    }
    if send_disassemble(s, ip, console) {
        let view = &mut s.debug.views.disassembly;
        view.current = Some(ip);
        view.loading = true;
        view.error = None;
    }
}

fn send_disassemble(s: &mut Session, ip: u64, console: bool) -> bool {
    let request = ViewRequest::Disassemble { ip, console };
    if console {
        let frame = frame_id(s);
        debug::view_request(s, request, |c| c.evaluate(lldb::DISASSEMBLE_FRAME_COMMAND, frame, "repl"))
    } else {
        let reference = memory::reference(ip);
        debug::view_request(s, request, |c| c.disassemble(&reference, -DISASSEMBLY_BEFORE, DISASSEMBLY_COUNT))
    }
}

fn on_disassemble(s: &mut Session, current: bool, ip: u64, console: bool, result: Result<Value, String>) {
    if !current {
        return;
    }
    let rows = match result {
        Ok(body) if console => {
            let text = dap::types::parse::<dap::EvaluateBody>(&body).unwrap_or_default().result;
            match lldb::error_of(&text) {
                Some(e) => Err(e),
                None => Ok(console_rows(&text)),
            }
        }
        Ok(body) => {
            let body: dap::DisassembleBody = dap::types::parse(&body).unwrap_or_default();
            let has_ip = body.instructions.iter().any(|i| memory::parse_address(&i.address) == Some(ip));
            let lldb = debug::live_adapter(s).is_some_and(|l| l.is_lldb());
            if !has_ip && lldb {
                // Older lldb-dap decodes from a misaligned address before the program counter:
                // ask its console for the whole function instead.
                if send_disassemble(s, ip, true) {
                    return;
                }
            }
            Ok(dap_rows(s, &body.instructions))
        }
        Err(e) => Err(e),
    };
    let view = &mut s.debug.views.disassembly;
    view.loading = false;
    view.via_console = console;
    view.current = Some(ip);
    match rows {
        Ok(mut rows) => {
            rows.truncate(MAX_DISASSEMBLY_ROWS);
            view.rows = rows;
            view.error = None;
        }
        Err(e) => {
            view.rows.clear();
            view.error = Some(e);
        }
    }
}

/// Rows from DAP instructions: a symbol row when the function changes, the source line when the
/// line changes.
fn dap_rows(s: &Session, instructions: &[dap::DisassembledInstruction]) -> Vec<DisassemblyRow> {
    let mut rows = Vec::new();
    let mut last_symbol: Option<&str> = None;
    let mut last_line: Option<(PathBuf, i64)> = None;
    let mut texts: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
    for ins in instructions {
        if ins.presentation_hint.as_deref() == Some("invalid") {
            continue;
        }
        let Some(address) = memory::parse_address(&ins.address) else { continue };
        if let Some(symbol) = ins.symbol.as_deref().filter(|x| !x.is_empty())
            && last_symbol != Some(symbol)
        {
            rows.push(DisassemblyRow::Symbol { name: symbol.to_owned() });
            last_symbol = Some(symbol);
        }
        let location = ins.location.as_ref().and_then(|l| l.path.clone()).zip(ins.line.filter(|l| *l > 0));
        if let Some((path, line)) = location
            && last_line.as_ref() != Some(&(path.clone(), line))
        {
            let zero_based = usize::try_from(line.saturating_sub(1)).unwrap_or(0);
            let lines = texts.entry(path.clone()).or_insert_with(|| source_lines(s, &path));
            let text = lines.as_ref().and_then(|l| l.get(zero_based)).cloned().unwrap_or_default();
            rows.push(DisassemblyRow::Source { path: Some(path.clone()), line: Some(zero_based), text });
            last_line = Some((path, line));
        }
        rows.push(DisassemblyRow::Instruction {
            address,
            bytes: ins.instruction_bytes.clone().filter(|b| !b.is_empty()),
            text: ins.instruction.trim().to_owned(),
            offset: None,
        });
    }
    rows
}

fn source_lines(s: &Session, path: &std::path::Path) -> Option<Vec<String>> {
    let text = match s.ws.doc_by_path(path).and_then(|d| s.ws.doc(d)) {
        Some(d) => d.doc.text(),
        None => codecraft_fs::read_text_file(path).ok()?,
    };
    Some(text.lines().map(str::to_owned).collect())
}

fn console_rows(text: &str) -> Vec<DisassemblyRow> {
    lldb::parse_disassembly(text)
        .into_iter()
        .map(|line| match line {
            lldb::DisassemblyLine::Symbol(name) => DisassemblyRow::Symbol { name },
            lldb::DisassemblyLine::Source(text) => DisassemblyRow::Source { path: None, line: None, text },
            lldb::DisassemblyLine::Instruction { address, offset, text, .. } => {
                DisassemblyRow::Instruction { address, bytes: None, text, offset }
            }
        })
        .collect()
}

// ---------------------------------------------------------------- registers

/// Fetch the selected frame's registers (the adapter's Registers scope).
pub(crate) fn request_registers(s: &mut Session) {
    let scope = s.debug.session.as_ref().and_then(|x| {
        x.scopes
            .iter()
            .find(|sc| {
                sc.presentation_hint.as_deref() == Some("registers") || sc.name.to_lowercase().contains("register")
            })
            .cloned()
    });
    let view = &mut s.debug.views.registers;
    let Some(scope) = scope else {
        view.loading = false;
        let collecting = s.debug.session.as_ref().is_some_and(|x| x.scopes.is_empty());
        view.error = (!collecting).then(|| "The debugger shows no registers for this frame.".to_owned());
        return;
    };
    view.waiting.clear();
    let reference = scope.reference;
    let request = ViewRequest::RegisterScope { name: scope.name };
    if debug::view_request(s, request, |c| c.variables(reference)) {
        let view = &mut s.debug.views.registers;
        view.loading = true;
        view.error = None;
    }
}

fn top_frame(s: &Session) -> bool {
    s.debug.session.as_ref().is_some_and(|x| x.frame == 0)
}

fn on_register_scope(s: &mut Session, current: bool, name: String, result: Result<Value, String>) {
    if !current {
        return;
    }
    let vars = match result {
        Ok(body) => dap::types::parse::<dap::VariablesBody>(&body).unwrap_or_default().variables,
        Err(e) => {
            let view = &mut s.debug.views.registers;
            view.loading = false;
            view.error = Some(e);
            return;
        }
    };
    let top = top_frame(s);
    let (groups, loose): (Vec<_>, Vec<_>) = vars.into_iter().partition(|v| v.variables_reference > 0);
    let mut new_groups = Vec::new();
    if !loose.is_empty() {
        new_groups.push(make_group(&mut s.debug.views.registers, name, loose, top));
    }
    let first = new_groups.len();
    for g in &groups {
        new_groups.push(RegisterGroup { name: g.name.clone(), registers: Vec::new() });
    }
    s.debug.views.registers.groups = new_groups;
    for (i, g) in groups.into_iter().enumerate() {
        let reference = g.variables_reference;
        if debug::view_request(s, ViewRequest::RegisterGroup { reference, top_frame: top }, |c| c.variables(reference))
        {
            s.debug.views.registers.waiting.insert(reference, first + i);
        }
    }
    let view = &mut s.debug.views.registers;
    view.loading = !view.waiting.is_empty();
}

fn make_group(view: &mut RegistersView, name: String, vars: Vec<dap::Variable>, top: bool) -> RegisterGroup {
    let registers = vars
        .into_iter()
        .take(MAX_REGISTERS)
        .map(|v| {
            let key = format!("{name}/{}", v.name);
            let changed = view.previous.get(&key).is_some_and(|old| *old != v.value);
            if top {
                view.current.insert(key, v.value.clone());
            }
            Register { name: v.name, value: v.value, changed }
        })
        .collect();
    RegisterGroup { name, registers }
}

fn on_register_group(s: &mut Session, current: bool, reference: i64, top: bool, result: Result<Value, String>) {
    let view = &mut s.debug.views.registers;
    let Some(index) = view.waiting.remove(&reference).filter(|_| current) else { return };
    view.loading = !view.waiting.is_empty();
    let vars = match result {
        Ok(body) => dap::types::parse::<dap::VariablesBody>(&body).unwrap_or_default().variables,
        Err(e) => {
            view.error = Some(e);
            return;
        }
    };
    let name = view.groups.get(index).map(|g| g.name.clone()).unwrap_or_default();
    let group = make_group(view, name, vars, top);
    if let Some(slot) = view.groups.get_mut(index) {
        *slot = group;
    }
}

// ---------------------------------------------------------------- watchpoints

/// Add a watchpoint on a variable (the child `name` of `container`, or an expression).
pub(crate) fn add_watchpoint(
    s: &mut Session,
    name: &str,
    container: Option<i64>,
    expression: Option<&str>,
    access: WatchAccess,
) -> Result<u64, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::BadParams("the variable name is empty".into()));
    }
    stopped(s)?;
    let live = adapter(s)?;
    let expression = expression.map(str::trim).filter(|e| !e.is_empty()).unwrap_or(name).to_owned();
    let console = !live.capabilities.supports_data_breakpoints;
    if console && !live.is_lldb() {
        return Err(CommandError::Failed(format!("{} does not support watchpoints", live.name())));
    }
    let views = &mut s.debug.views;
    views.next_watchpoint = views.next_watchpoint.max(1);
    let id = views.next_watchpoint;
    views.next_watchpoint = id.saturating_add(1);
    views.watchpoints.push(Watchpoint {
        id,
        name: name.to_owned(),
        expression: expression.clone(),
        access,
        enabled: true,
        description: None,
        verified: None,
        message: None,
        data_id: None,
        lldb_id: None,
    });
    let frame = frame_id(s);
    let sent = if console {
        let command = lldb::watchpoint_set_command(&expression, access);
        let request = ViewRequest::WatchCommand { id, action: WatchAction::Set };
        debug::view_request(s, request, |c| c.evaluate(&command, frame, "repl"))
    } else {
        // With a container the adapter resolves the child; without, the expression itself.
        let (reference, lookup) = match container.filter(|r| *r > 0) {
            Some(r) => (Some(r), name.to_owned()),
            None => (None, expression.clone()),
        };
        debug::view_request(s, ViewRequest::WatchInfo { id }, |c| c.data_breakpoint_info(reference, &lookup, frame))
    };
    if !sent {
        s.debug.views.watchpoints.retain(|w| w.id != id);
        return Err(not_responding());
    }
    show_breakpoints(s);
    Ok(id)
}

fn show_breakpoints(s: &mut Session) {
    s.debug.tab = DebugTab::Breakpoints;
    s.ws.layout.show(ToolWindowId::Debug);
}

/// Remove a watchpoint (and from the adapter, when a session runs).
pub(crate) fn remove_watchpoint(s: &mut Session, id: u64) -> bool {
    let Some(index) = s.debug.views.watchpoints.iter().position(|w| w.id == id) else { return false };
    let w = s.debug.views.watchpoints.remove(index);
    if let Some(lldb_id) = w.lldb_id {
        let command = lldb::watchpoint_delete_command(lldb_id);
        let frame = frame_id(s);
        let request = ViewRequest::WatchCommand { id, action: WatchAction::Delete };
        debug::view_request(s, request, |c| c.evaluate(&command, frame, "repl"));
    } else if w.data_id.is_some() {
        send_watchpoints(s);
    }
    true
}

pub(crate) fn set_watchpoint_enabled(s: &mut Session, id: u64, enabled: bool) -> Result<(), CommandError> {
    let w = s
        .debug
        .views
        .watchpoints
        .iter_mut()
        .find(|w| w.id == id)
        .ok_or_else(|| CommandError::BadParams(format!("no watchpoint {id}")))?;
    w.enabled = enabled;
    let (lldb_id, data) = (w.lldb_id, w.data_id.is_some());
    if let Some(lldb_id) = lldb_id {
        let command = lldb::watchpoint_enable_command(lldb_id, enabled);
        let frame = frame_id(s);
        let request = ViewRequest::WatchCommand { id, action: WatchAction::Enable(enabled) };
        debug::view_request(s, request, |c| c.evaluate(&command, frame, "repl"));
    } else if data {
        send_watchpoints(s);
    }
    Ok(())
}

/// Send every enabled watchpoint with a data id (`setDataBreakpoints` replaces them all).
fn send_watchpoints(s: &mut Session) {
    let (ids, list): (Vec<u64>, Vec<dap::DataBreakpoint>) = s
        .debug
        .views
        .watchpoints
        .iter()
        .filter(|w| w.enabled)
        .filter_map(|w| {
            let data_id = w.data_id.clone()?;
            Some((
                w.id,
                dap::DataBreakpoint { data_id, access_type: Some(w.access.dap().to_owned()), ..Default::default() },
            ))
        })
        .unzip();
    debug::view_request(s, ViewRequest::SetWatchpoints { ids }, |c| c.set_data_breakpoints(&list));
}

fn watch_failed(s: &mut Session, id: u64, error: String) {
    let name = s.debug.views.watchpoints.iter().find(|w| w.id == id).map(|w| w.name.clone()).unwrap_or_default();
    s.debug.views.watchpoints.retain(|w| w.id != id);
    let message = format!("Cannot watch {name}: {error}");
    s.debug.push_console_line(DebugConsoleKind::Error, message.clone());
    s.status = message;
}

fn on_watch_info(s: &mut Session, id: u64, result: Result<Value, String>) {
    let info: dap::DataBreakpointInfoBody = match result.and_then(|b| dap::types::parse(&b)) {
        Ok(info) => info,
        Err(e) => return watch_failed(s, id, e),
    };
    let Some(data_id) = info.data_id.filter(|d| !d.is_empty()) else {
        let why =
            if info.description.is_empty() { "the debugger cannot watch it".to_owned() } else { info.description };
        return watch_failed(s, id, why);
    };
    let Some(w) = s.debug.views.watchpoints.iter_mut().find(|w| w.id == id) else { return };
    w.data_id = Some(data_id);
    w.description = Some(info.description).filter(|d| !d.is_empty());
    if let Some(types) = info.access_types.filter(|t| !t.is_empty())
        && !types.iter().any(|t| t == w.access.dap())
        && let Some(access) = types.iter().find_map(|t| WatchAccess::parse(t))
    {
        w.message = Some(format!("the debugger can only break on {}", access.dap()));
        w.access = access;
    }
    send_watchpoints(s);
}

fn on_set_watchpoints(s: &mut Session, ids: Vec<u64>, result: Result<Value, String>) {
    let body: dap::BreakpointsBody = match result {
        Ok(body) => dap::types::parse(&body).unwrap_or_default(),
        Err(e) => {
            s.debug.push_console_line(DebugConsoleKind::Error, format!("Could not set watchpoints: {e}"));
            for w in s.debug.views.watchpoints.iter_mut().filter(|w| ids.contains(&w.id)) {
                w.verified = Some(false);
                w.message = Some(e.clone());
            }
            return;
        }
    };
    for (id, theirs) in ids.iter().zip(body.breakpoints) {
        if let Some(w) = s.debug.views.watchpoints.iter_mut().find(|w| w.id == *id) {
            w.verified = Some(theirs.verified);
            if theirs.message.is_some() || theirs.verified {
                w.message = theirs.message;
            }
        }
    }
}

fn on_watch_command(s: &mut Session, id: u64, action: WatchAction, result: Result<Value, String>) {
    let text = result.map(|b| dap::types::parse::<dap::EvaluateBody>(&b).unwrap_or_default().result);
    let error = match &text {
        Ok(t) => lldb::error_of(t),
        Err(e) => Some(e.clone()),
    };
    match action {
        WatchAction::Set => match (error, text.ok().as_deref().and_then(lldb::parse_watchpoint_created)) {
            (None, Some(lldb_id)) => {
                if let Some(w) = s.debug.views.watchpoints.iter_mut().find(|w| w.id == id) {
                    w.lldb_id = Some(lldb_id);
                    w.verified = Some(true);
                    w.description = Some(format!("LLDB watchpoint {lldb_id}"));
                }
            }
            (Some(e), _) => watch_failed(s, id, e),
            (None, None) => watch_failed(s, id, "LLDB did not create a watchpoint".into()),
        },
        WatchAction::Delete | WatchAction::Enable(_) => {
            if let Some(e) = error {
                s.debug.push_console_line(DebugConsoleKind::Error, format!("Watchpoint: {e}"));
            }
        }
    }
}

// ---------------------------------------------------------------- responses

pub(crate) fn on_response(s: &mut Session, current: bool, request: ViewRequest, result: Result<Value, String>) {
    match request {
        ViewRequest::MemoryEval { expression, address_of } => on_memory_eval(s, expression, address_of, result),
        ViewRequest::MemoryRead { address, console } => on_memory_read(s, address, console, result),
        ViewRequest::MemoryWrite { console } => on_memory_write(s, console, result),
        ViewRequest::Disassemble { ip, console } => on_disassemble(s, current, ip, console, result),
        ViewRequest::RegisterScope { name } => on_register_scope(s, current, name, result),
        ViewRequest::RegisterGroup { reference, top_frame } => {
            on_register_group(s, current, reference, top_frame, result);
        }
        ViewRequest::WatchInfo { id } => on_watch_info(s, id, result),
        ViewRequest::SetWatchpoints { ids } => on_set_watchpoints(s, ids, result),
        ViewRequest::WatchCommand { id, action } => on_watch_command(s, id, action, result),
    }
}

// ---------------------------------------------------------------- commands

fn paused(s: &Session) -> Result<(), String> {
    if s.debug.is_stopped() { Ok(()) } else { Err("the program is not paused in the debugger".into()) }
}

fn show(s: &mut Session, tab: DebugTab) {
    s.debug.tab = tab;
    s.debug.choosing = false;
    s.ws.layout.show(ToolWindowId::Debug);
}

/// Show a Debug tab; the Memory, Disassembly and Registers views refresh while paused.
pub(crate) fn show_view(s: &mut Session, tab: DebugTab) {
    let was = s.debug.tab;
    show(s, tab);
    if !s.debug.is_stopped() || was == tab {
        return;
    }
    match tab {
        DebugTab::Disassembly => request_disassembly(s),
        DebugTab::Registers => request_registers(s),
        DebugTab::Memory => {
            if let Some(address) = s.debug.views.memory.address
                && let Err(e) = read_memory(s, address)
            {
                s.debug.views.memory.error = Some(e.to_string());
            }
        }
        _ => {}
    }
}

/// An address param: a number or a string (`0x…`, decimal).
fn address_param(p: &Value, key: &str) -> Result<u64, CommandError> {
    match p.get(key) {
        Some(Value::Number(n)) => {
            n.as_u64().ok_or_else(|| CommandError::BadParams(format!("`{key}` must be an address")))
        }
        Some(Value::String(text)) => memory::parse_address(text)
            .ok_or_else(|| CommandError::BadParams(format!("`{text}` is not an address (0x… or decimal)"))),
        _ => Err(CommandError::BadParams(format!("missing address param `{key}`"))),
    }
}

fn id_param(p: &Value) -> Result<u64, CommandError> {
    p.get("id").and_then(Value::as_u64).ok_or_else(|| CommandError::BadParams("missing integer param `id`".into()))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ----- memory
        spec(
            "debug.memory.show",
            "Show Memory View",
            None,
            "{address?: string} — open the Memory view, at an address (0x…, decimal) or expression (`&x`, `ptr`)",
            Context::Global,
            always,
            |s, p| {
                match opt_str(p, "address") {
                    Some(address) => {
                        show(s, DebugTab::Memory);
                        goto_memory(s, address)?;
                    }
                    None => show_view(s, DebugTab::Memory),
                }
                ok()
            },
        ),
        spec(
            "debug.memory.goto",
            "Go to Address",
            None,
            "{address: string} — an address (0x…, decimal) or an expression: a pointer shows what it points \
             to, `&x` or any other value shows where it lives",
            Context::Global,
            paused,
            |s, p| {
                show(s, DebugTab::Memory);
                goto_memory(s, req_str(p, "address")?)?;
                ok()
            },
        ),
        spec(
            "debug.memory.page",
            "Memory Page",
            None,
            "{delta: int} — move the Memory view by pages (-1 back, 1 forward)",
            Context::Global,
            paused,
            |s, p| {
                let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1);
                page_memory(s, delta)?;
                ok()
            },
        ),
        spec("debug.memory.refresh", "Refresh Memory View", None, "", Context::Global, paused, |s, _| {
            let address =
                s.debug.views.memory.address.ok_or_else(|| CommandError::Disabled("no memory is shown".into()))?;
            read_memory(s, address)?;
            ok()
        }),
        spec(
            "debug.memory.write",
            "Write Memory",
            None,
            "{address: string|int, bytes: string} — write hex bytes (`2a 00 ff`) at an address",
            Context::Global,
            paused,
            |s, p| {
                let address = address_param(p, "address")?;
                let bytes = memory::parse_hex_bytes(req_str(p, "bytes")?, MAX_WRITE)
                    .ok_or_else(|| CommandError::BadParams("`bytes` must be hex bytes like `2a 00 ff`".into()))?;
                write_memory(s, address, &bytes)?;
                Ok(json!({ "address": memory::reference(address), "count": bytes.len() }))
            },
        ),
        // ----- disassembly
        spec(
            "debug.disassembly.show",
            "Show Disassembly",
            None,
            "Open the Disassembly view at the selected frame (Step Over/Into then go by instruction)",
            Context::Global,
            always,
            |s, _| {
                show(s, DebugTab::Disassembly);
                if s.debug.is_stopped() {
                    request_disassembly(s);
                }
                ok()
            },
        ),
        spec("debug.disassembly.refresh", "Refresh Disassembly", None, "", Context::Global, paused, |s, _| {
            request_disassembly(s);
            ok()
        }),
        spec(
            "debug.disassembly.stepOverInstruction",
            "Step Over Instruction",
            None,
            "Execute one machine instruction, stepping over calls",
            Context::Global,
            paused,
            |s, _| {
                debug::step_instruction(s, true)?;
                ok()
            },
        ),
        spec(
            "debug.disassembly.stepIntoInstruction",
            "Step Into Instruction",
            None,
            "Execute one machine instruction, following calls",
            Context::Global,
            paused,
            |s, _| {
                debug::step_instruction(s, false)?;
                ok()
            },
        ),
        // ----- registers
        spec("debug.registers.show", "Show Registers", None, "", Context::Global, always, |s, _| {
            show(s, DebugTab::Registers);
            if s.debug.is_stopped() {
                request_registers(s);
            }
            ok()
        }),
        spec("debug.registers.refresh", "Refresh Registers", None, "", Context::Global, paused, |s, _| {
            request_registers(s);
            ok()
        }),
        // ----- watchpoints
        spec(
            "debug.watchpoint.add",
            "Add Watchpoint",
            None,
            "{name: string, reference?: int, evaluateName?: string, access?: write|read|readWrite} — break \
             when the variable `name` (a child of variables `reference`, else an expression) is written \
             (`write`, the default), read, or either",
            Context::Global,
            paused,
            |s, p| {
                let access = match opt_str(p, "access") {
                    None => WatchAccess::Write,
                    Some(a) => WatchAccess::parse(a).ok_or_else(|| {
                        CommandError::BadParams(format!("unknown access `{a}` (write, read, readWrite)"))
                    })?,
                };
                let reference = p.get("reference").and_then(Value::as_i64);
                let id = add_watchpoint(s, req_str(p, "name")?, reference, opt_str(p, "evaluateName"), access)?;
                Ok(json!({ "id": id }))
            },
        ),
        spec("debug.watchpoint.remove", "Remove Watchpoint", None, "{id: int}", Context::Global, always, |s, p| {
            Ok(json!({ "removed": remove_watchpoint(s, id_param(p)?) }))
        }),
        spec(
            "debug.watchpoint.setEnabled",
            "Enable Watchpoint",
            None,
            "{id: int, enabled?: bool} — enable or disable (toggles without `enabled`)",
            Context::Global,
            always,
            |s, p| {
                let id = id_param(p)?;
                let current = s.debug.views.watchpoints.iter().find(|w| w.id == id).map(|w| w.enabled);
                let enabled = p.get("enabled").and_then(Value::as_bool).unwrap_or(!current.unwrap_or(true));
                set_watchpoint_enabled(s, id, enabled)?;
                Ok(json!({ "enabled": enabled }))
            },
        ),
        spec("debug.watchpoint.removeAll", "Remove All Watchpoints", None, "", Context::Global, always, |s, _| {
            let ids: Vec<u64> = s.debug.views.watchpoints.iter().map(|w| w.id).collect();
            let count = ids.len();
            for id in ids {
                remove_watchpoint(s, id);
            }
            Ok(json!({ "removed": count }))
        }),
        // ----- console
        spec(
            "debug.console.history",
            "Debug Console History",
            None,
            "→ {history: [string]} — expressions and commands typed into the debug console, oldest first",
            Context::Global,
            always,
            |s, _| Ok(json!({ "history": s.debug.views.console_history })),
        ),
        spec("debug.console.clearHistory", "Clear Debug Console History", None, "", Context::Global, always, |s, _| {
            s.debug.views.console_history.clear();
            ok()
        }),
    ]
}

/// Shared by the result of commands that render the views: the Memory view as text rows.
pub fn hex_row(bytes: &[Option<u8>]) -> String {
    bytes.iter().map(|b| b.map_or_else(|| "??".to_owned(), |b| format!("{b:02x}"))).collect::<Vec<_>>().join(" ")
}

/// The printable-ASCII column of a Memory view row (`.` for the rest).
pub fn ascii_row(bytes: &[Option<u8>]) -> String {
    bytes
        .iter()
        .map(|b| match b {
            Some(b) if b.is_ascii_graphic() || *b == b' ' => char::from(*b),
            Some(_) => '.',
            None => '?',
        })
        .collect()
}

#[cfg(test)]
mod tests;
