//! Jupyter notebooks (`.ipynb`, nbformat 4): the pure model behind the notebook editor (M12
//! track PY). Parsing, serializing and edit operations; no processes, no UI.
//!
//! Fidelity: a cell keeps every field of the file it does not manage (cell `id`, `metadata`,
//! `attachments`, unknown keys), and the notebook keeps its `metadata` and unknown top-level
//! keys. Writing follows nbformat's own writer (`json.dumps(nb, sort_keys=True, indent=1,
//! ensure_ascii=False)` plus a final newline; multi-line strings of sources, streams and
//! `text/*` / `image/svg+xml` / `application/javascript` data stored as lists of lines), so a
//! notebook saved by Jupyter round-trips byte for byte.
//!
//! In memory, list-of-lines strings of outputs are joined (like nbformat's reader), so views can
//! borrow them. Output produced by the kernel is capped per cell ([`MAX_KERNEL_BYTES_PER_CELL`],
//! [`MAX_OUTPUTS_PER_CELL`], [`MAX_IMAGE_BYTES`]); outputs read from a file are kept whole (they
//! are the file's data), the views truncate what they show.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// Largest notebook file opened as a notebook (bigger ones open as text).
pub const MAX_NOTEBOOK_BYTES: usize = 256 << 20;
/// Stream text, results and errors a cell keeps from one run (the rest is dropped with a note).
pub const MAX_KERNEL_BYTES_PER_CELL: usize = 2 << 20;
/// Outputs a cell keeps from one run.
pub const MAX_OUTPUTS_PER_CELL: usize = 500;
/// Largest image (base64 bytes) kept from the kernel.
pub const MAX_IMAGE_BYTES: usize = 16 << 20;
/// Cells kept from a file (absurd files are cut off with an error instead).
pub const MAX_CELLS: usize = 100_000;

/// What the kernel output cap notes in the cell.
pub const TRUNCATED_NOTE: &str = "\n[Output truncated: the cell produced more output than the notebook keeps]\n";

static NEXT_UID: AtomicU64 = AtomicU64::new(1);

fn next_uid() -> u64 {
    NEXT_UID.fetch_add(1, Ordering::Relaxed)
}

/// A cell's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CellKind {
    Code,
    Markdown,
    Raw,
}

impl CellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Markdown => "markdown",
            Self::Raw => "raw",
        }
    }

    /// `code`, `markdown`/`md`, `raw` (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "code" | "python" => Some(Self::Code),
            "markdown" | "md" => Some(Self::Markdown),
            "raw" | "text" => Some(Self::Raw),
            _ => None,
        }
    }
}

/// One cell.
#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// Session-local identity (stable while cells move; never saved).
    pub uid: u64,
    pub kind: CellKind,
    pub source: String,
    /// `execution_count` of a code cell.
    pub execution_count: Option<u64>,
    /// Output objects (nbformat JSON, multi-line strings joined).
    pub outputs: Vec<Value>,
    /// Every other field of the cell as read (`id`, `metadata`, `attachments`, unknown keys).
    raw: Map<String, Value>,
    /// The file had no valid `cell_type` (it is written back as it was unless the type changes).
    kind_from_file: bool,
    /// Bytes of output the kernel added in the current run (for the caps).
    kernel_bytes: usize,
    /// The current run's output was truncated (the note was added).
    truncated: bool,
}

impl Cell {
    /// A new cell (with an `id` when `with_id`).
    pub fn new(kind: CellKind, source: &str, id: Option<String>) -> Self {
        let mut raw = Map::new();
        if let Some(id) = id {
            raw.insert("id".into(), Value::String(id));
        }
        raw.insert("metadata".into(), json!({}));
        Self {
            uid: next_uid(),
            kind,
            source: source.to_string(),
            execution_count: None,
            outputs: Vec::new(),
            raw,
            kind_from_file: true,
            kernel_bytes: 0,
            truncated: false,
        }
    }

    /// The nbformat 4.5 cell id.
    pub fn id(&self) -> Option<&str> {
        self.raw.get("id").and_then(Value::as_str)
    }

    pub fn metadata(&self) -> Option<&Value> {
        self.raw.get("metadata")
    }

    /// A field of the cell kept from the file (`attachments`, unknown keys).
    pub fn extra(&self, key: &str) -> Option<&Value> {
        self.raw.get(key)
    }

    /// Clear outputs and the execution count (before a run, or Clear Outputs).
    pub fn clear_outputs(&mut self) {
        self.outputs.clear();
        self.execution_count = None;
        self.kernel_bytes = 0;
        self.truncated = false;
    }

    /// Whether more kernel output may be added; adds the truncation note once when not.
    fn admit(&mut self, bytes: usize) -> bool {
        if self.truncated {
            return false;
        }
        let total = self.kernel_bytes.saturating_add(bytes);
        if total > MAX_KERNEL_BYTES_PER_CELL || self.outputs.len() >= MAX_OUTPUTS_PER_CELL {
            self.truncated = true;
            self.outputs.push(stream_output("stderr", TRUNCATED_NOTE));
            return false;
        }
        self.kernel_bytes = total;
        true
    }

    /// Append stream text from the kernel (merged into the previous output of the same stream).
    pub fn append_stream(&mut self, name: &str, text: &str) {
        if text.is_empty() || !self.admit(text.len()) {
            return;
        }
        let name = if name == "stderr" { "stderr" } else { "stdout" };
        if let Some(last) = self.outputs.last_mut()
            && last.get("output_type").and_then(Value::as_str) == Some("stream")
            && last.get("name").and_then(Value::as_str) == Some(name)
            && let Some(Value::String(t)) = last.get_mut("text")
        {
            t.push_str(text);
            return;
        }
        self.outputs.push(stream_output(name, text));
    }

    /// Add an output from the kernel (`execute_result`, `display_data`, `error`), within the caps.
    pub fn push_output(&mut self, mut output: Value) {
        if let Some(data) = output.get_mut("data").and_then(Value::as_object_mut) {
            for mime in ["image/png", "image/jpeg"] {
                if data.get(mime).and_then(Value::as_str).is_some_and(|s| s.len() > MAX_IMAGE_BYTES) {
                    data.remove(mime);
                    data.entry("text/plain").or_insert_with(|| Value::String("<image too large to keep>".into()));
                }
            }
        }
        let bytes = approx_size(&output);
        if self.admit(bytes) {
            self.outputs.push(output);
        }
    }
}

/// Rough size of a JSON value (string bytes), for the output caps.
fn approx_size(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => a.iter().map(approx_size).fold(0usize, usize::saturating_add),
        Value::Object(m) => m.iter().map(|(k, v)| k.len().saturating_add(approx_size(v))).fold(0, usize::saturating_add),
        _ => 8,
    }
}

/// A notebook.
#[derive(Clone, Debug, PartialEq)]
pub struct Notebook {
    pub cells: Vec<Cell>,
    pub nbformat: u64,
    pub nbformat_minor: u64,
    /// Notebook metadata (kernelspec, language_info, …), kept as read.
    pub metadata: Value,
    /// Unknown top-level keys.
    extra: Map<String, Value>,
}

impl Default for Notebook {
    fn default() -> Self {
        Self::empty()
    }
}

impl Notebook {
    /// A new notebook with one empty code cell (nbformat 4.5).
    pub fn empty() -> Self {
        let mut nb = Self {
            cells: Vec::new(),
            nbformat: 4,
            nbformat_minor: 5,
            metadata: json!({
                "kernelspec": { "display_name": "Python 3", "language": "python", "name": "python3" },
                "language_info": { "name": "python" }
            }),
            extra: Map::new(),
        };
        let id = nb.new_cell_id();
        nb.cells.push(Cell::new(CellKind::Code, "", id));
        nb
    }

    /// Parse a notebook file. Errors explain what is wrong (the file then opens as text).
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_NOTEBOOK_BYTES {
            return Err(format!("the notebook is too large ({} MB) to open as a notebook", text.len() >> 20));
        }
        if text.trim().is_empty() {
            return Ok(Self::empty());
        }
        let value: Value = serde_json::from_str(text).map_err(|e| format!("not a valid notebook (JSON): {e}"))?;
        let Value::Object(mut top) = value else { return Err("not a notebook: the file is not a JSON object".into()) };
        let nbformat = match top.remove("nbformat") {
            None => 4,
            Some(v) => v.as_u64().ok_or("not a notebook: `nbformat` is not a number")?,
        };
        if nbformat != 4 {
            return Err(format!("nbformat {nbformat} notebooks are not supported (only version 4)"));
        }
        let nbformat_minor = match top.remove("nbformat_minor") {
            None => 0,
            Some(v) => v.as_u64().ok_or("not a notebook: `nbformat_minor` is not a number")?,
        };
        let metadata = top.remove("metadata").unwrap_or_else(|| json!({}));
        let cells_value = top.remove("cells").unwrap_or(Value::Array(Vec::new()));
        let Value::Array(cells_json) = cells_value else { return Err("not a notebook: `cells` is not a list".into()) };
        if cells_json.len() > MAX_CELLS {
            return Err(format!("the notebook has too many cells ({})", cells_json.len()));
        }
        let mut cells = Vec::with_capacity(cells_json.len());
        for (i, c) in cells_json.into_iter().enumerate() {
            cells.push(parse_cell(c).map_err(|e| format!("cell {}: {e}", i + 1))?);
        }
        Ok(Self { cells, nbformat, nbformat_minor, metadata, extra: top })
    }

    /// The notebook as nbformat JSON text (sorted keys, one-space indent, final newline).
    pub fn to_json(&self) -> String {
        let mut top = self.extra.clone();
        top.insert("cells".into(), Value::Array(self.cells.iter().map(cell_json).collect()));
        top.insert("metadata".into(), self.metadata.clone());
        top.insert("nbformat".into(), json!(self.nbformat));
        top.insert("nbformat_minor".into(), json!(self.nbformat_minor));
        let mut out = String::with_capacity(4096);
        write_value(&mut out, &Value::Object(top), 0);
        out.push('\n');
        out
    }

    /// The kernel language from the metadata (`python` by default).
    pub fn language(&self) -> String {
        let m = &self.metadata;
        m.pointer("/kernelspec/language")
            .or_else(|| m.pointer("/language_info/name"))
            .and_then(Value::as_str)
            .unwrap_or("python")
            .to_ascii_lowercase()
    }

    /// The kernel's display name from the metadata.
    pub fn kernel_display_name(&self) -> Option<String> {
        self.metadata.pointer("/kernelspec/display_name").and_then(Value::as_str).map(str::to_owned)
    }

    /// Cells carry ids (nbformat 4.5+).
    pub fn uses_cell_ids(&self) -> bool {
        self.nbformat > 4 || self.nbformat_minor >= 5 || self.cells.iter().any(|c| c.id().is_some())
    }

    /// A fresh cell id (8 hex digits, unique in the notebook) when the notebook uses ids.
    pub fn new_cell_id(&self) -> Option<String> {
        if !self.uses_cell_ids() {
            return None;
        }
        let mut seed = next_uid()
            ^ std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9);
        loop {
            // splitmix64
            seed = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = seed;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            let id = format!("{:08x}", z & 0xFFFF_FFFF);
            if !self.cells.iter().any(|c| c.id() == Some(id.as_str())) {
                return Some(id);
            }
        }
    }

    /// The index of the cell with `uid`.
    pub fn index_of(&self, uid: u64) -> Option<usize> {
        self.cells.iter().position(|c| c.uid == uid)
    }

    pub fn cell_mut(&mut self, uid: u64) -> Option<&mut Cell> {
        self.cells.iter_mut().find(|c| c.uid == uid)
    }

    /// Insert a cell at `index` (clamped); returns its index.
    pub fn insert_cell(&mut self, index: usize, kind: CellKind, source: &str) -> usize {
        let id = self.new_cell_id();
        let at = index.min(self.cells.len());
        self.cells.insert(at, Cell::new(kind, source, id));
        at
    }

    /// Remove the cell at `index`.
    pub fn delete_cell(&mut self, index: usize) -> Result<Cell, String> {
        if index >= self.cells.len() {
            return Err(format!("no cell {index}"));
        }
        Ok(self.cells.remove(index))
    }

    /// Move the cell at `index` one place up or down; returns its new index.
    pub fn move_cell(&mut self, index: usize, up: bool) -> Result<usize, String> {
        if index >= self.cells.len() {
            return Err(format!("no cell {index}"));
        }
        let to = if up { index.checked_sub(1) } else { Some(index + 1).filter(|t| *t < self.cells.len()) };
        let Some(to) = to else { return Ok(index) };
        self.cells.swap(index, to);
        Ok(to)
    }

    /// Change a cell's type: code → markdown/raw drops outputs and the execution count (as
    /// Jupyter does).
    pub fn set_kind(&mut self, index: usize, kind: CellKind) -> Result<(), String> {
        let cell = self.cells.get_mut(index).ok_or_else(|| format!("no cell {index}"))?;
        if cell.kind == kind && cell.kind_from_file {
            return Ok(());
        }
        if kind != CellKind::Code {
            cell.clear_outputs();
        }
        cell.kind = kind;
        cell.kind_from_file = true;
        Ok(())
    }

    pub fn set_source(&mut self, index: usize, source: &str) -> Result<(), String> {
        let cell = self.cells.get_mut(index).ok_or_else(|| format!("no cell {index}"))?;
        cell.source = source.to_string();
        Ok(())
    }

    pub fn clear_outputs(&mut self, index: usize) -> Result<(), String> {
        self.cells.get_mut(index).ok_or_else(|| format!("no cell {index}"))?.clear_outputs();
        Ok(())
    }

    pub fn clear_all_outputs(&mut self) {
        for c in &mut self.cells {
            c.clear_outputs();
        }
    }
}

fn parse_cell(c: Value) -> Result<Cell, String> {
    let Value::Object(mut raw) = c else { return Err("not a JSON object".into()) };
    let (kind, kind_from_file) = match raw.get("cell_type").and_then(Value::as_str).and_then(|t| match t {
        "code" => Some(CellKind::Code),
        "markdown" => Some(CellKind::Markdown),
        "raw" => Some(CellKind::Raw),
        _ => None,
    }) {
        Some(k) => {
            raw.remove("cell_type");
            (k, true)
        }
        // Unknown or missing: shown as raw, written back as it was.
        None => (CellKind::Raw, false),
    };
    let source = match raw.remove("source") {
        None | Some(Value::Null) => String::new(),
        Some(v) => join_text(&v).ok_or("`source` is not text")?,
    };
    let mut execution_count = None;
    let mut outputs = Vec::new();
    if kind == CellKind::Code && kind_from_file {
        execution_count = match raw.remove("execution_count") {
            Some(Value::Number(n)) => n.as_u64(),
            _ => None,
        };
        outputs = match raw.remove("outputs") {
            Some(Value::Array(list)) => list.into_iter().map(normalize_output).collect(),
            _ => Vec::new(),
        };
    }
    Ok(Cell { uid: next_uid(), kind, source, execution_count, outputs, raw, kind_from_file, kernel_bytes: 0, truncated: false })
}

/// A string, or a list of strings joined (nbformat's multi-line text).
fn join_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for p in parts {
                out.push_str(p.as_str()?);
            }
            Some(out)
        }
        _ => None,
    }
}

/// Join the list-of-lines strings of an output (stream text, mime data) in memory.
fn normalize_output(mut o: Value) -> Value {
    if let Some(map) = o.as_object_mut() {
        if let Some(t) = map.get("text").and_then(join_text) {
            map.insert("text".into(), Value::String(t));
        }
        if let Some(data) = map.get_mut("data").and_then(Value::as_object_mut) {
            for (key, value) in data.iter_mut() {
                if splits(key)
                    && value.is_array()
                    && let Some(t) = join_text(value)
                {
                    *value = Value::String(t);
                }
            }
        }
    }
    o
}

/// Whether nbformat stores this mime type's string as a list of lines.
fn splits(mime: &str) -> bool {
    mime.starts_with("text/") || mime == "application/javascript" || mime == "image/svg+xml"
}

/// `s` as a list of lines, each keeping its `\n` (Python's `splitlines(True)` on `\n`).
fn split_lines(s: &str) -> Value {
    Value::Array(s.split_inclusive('\n').map(|l| Value::String(l.to_string())).collect())
}

fn cell_json(c: &Cell) -> Value {
    let mut m = c.raw.clone();
    if c.kind_from_file {
        m.insert("cell_type".into(), Value::String(c.kind.as_str().into()));
    }
    m.insert("source".into(), split_lines(&c.source));
    if c.kind == CellKind::Code && c.kind_from_file {
        m.insert("execution_count".into(), c.execution_count.map(Value::from).unwrap_or(Value::Null));
        m.insert("outputs".into(), Value::Array(c.outputs.iter().map(output_json).collect()));
    } else if c.kind_from_file {
        m.remove("execution_count");
        m.remove("outputs");
    }
    if c.kind_from_file {
        m.entry("metadata").or_insert_with(|| json!({}));
    }
    Value::Object(m)
}

fn output_json(o: &Value) -> Value {
    let mut o = o.clone();
    if let Some(map) = o.as_object_mut() {
        let stream = map.get("output_type").and_then(Value::as_str) == Some("stream");
        if stream && let Some(Value::String(t)) = map.get("text") {
            let v = split_lines(t);
            map.insert("text".into(), v);
        }
        if let Some(data) = map.get_mut("data").and_then(Value::as_object_mut) {
            for (key, value) in data.iter_mut() {
                if splits(key)
                    && let Value::String(s) = value
                {
                    *value = split_lines(s);
                }
            }
        }
    }
    o
}

/// Python's `json.dumps(v, sort_keys=True, indent=1, ensure_ascii=False)`.
fn write_value(out: &mut String, v: &Value, level: usize) {
    match v {
        Value::Object(m) if m.is_empty() => out.push_str("{}"),
        Value::Array(a) if a.is_empty() => out.push_str("[]"),
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push_str("{\n");
            for (i, k) in keys.iter().enumerate() {
                indent(out, level + 1);
                out.push_str(&serde_json::to_string(k).unwrap_or_else(|_| "\"\"".into()));
                out.push_str(": ");
                if let Some(v) = m.get(*k) {
                    write_value(out, v, level + 1);
                }
                if i + 1 < keys.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, level);
            out.push('}');
        }
        Value::Array(a) => {
            out.push_str("[\n");
            for (i, v) in a.iter().enumerate() {
                indent(out, level + 1);
                write_value(out, v, level + 1);
                if i + 1 < a.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            indent(out, level);
            out.push(']');
        }
        other => out.push_str(&serde_json::to_string(other).unwrap_or_else(|_| "null".into())),
    }
}

fn indent(out: &mut String, level: usize) {
    for _ in 0..level {
        out.push(' ');
    }
}

// ---------------------------------------------------------------- outputs

/// A stream output object.
pub fn stream_output(name: &str, text: &str) -> Value {
    json!({ "name": name, "output_type": "stream", "text": text })
}

/// An `execute_result` output object.
pub fn result_output(execution_count: Option<u64>, data: Map<String, Value>) -> Value {
    json!({ "data": data, "execution_count": execution_count, "metadata": {}, "output_type": "execute_result" })
}

/// A `display_data` output object.
pub fn display_output(data: Map<String, Value>) -> Value {
    json!({ "data": data, "metadata": {}, "output_type": "display_data" })
}

/// An `error` output object.
pub fn error_output(ename: &str, evalue: &str, traceback: Vec<String>) -> Value {
    json!({ "ename": ename, "evalue": evalue, "output_type": "error", "traceback": traceback })
}

/// A borrowed view of an output for display.
#[derive(Debug, PartialEq)]
pub enum OutputView<'a> {
    Stream { stderr: bool, text: &'a str },
    /// `execute_result` (with its count) or `display_data`.
    Data { execution_count: Option<u64>, data: &'a Map<String, Value> },
    Error { ename: &'a str, evalue: &'a str, traceback: Vec<&'a str> },
    /// An output type this version does not know.
    Other { output_type: &'a str },
}

/// How to show an output object.
pub fn output_view(o: &Value) -> OutputView<'_> {
    let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("");
    match o.get("output_type").and_then(Value::as_str).unwrap_or("") {
        "stream" => OutputView::Stream { stderr: s("name") == "stderr", text: s("text") },
        t @ ("execute_result" | "display_data" | "pyout") => {
            static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
            let data = o.get("data").and_then(Value::as_object).unwrap_or_else(|| EMPTY.get_or_init(Map::new));
            let count = if t == "display_data" { None } else { o.get("execution_count").and_then(Value::as_u64) };
            OutputView::Data { execution_count: count, data }
        }
        "error" | "pyerr" => OutputView::Error {
            ename: s("ename"),
            evalue: s("evalue"),
            traceback: o
                .get("traceback")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default(),
        },
        other => OutputView::Other { output_type: other },
    }
}

/// The text of a mime bundle entry (a string, a list of strings, or JSON shown as text).
pub fn mime_text(data: &Map<String, Value>, mime: &str) -> Option<String> {
    let v = data.get(mime)?;
    join_text(v).or_else(|| serde_json::to_string_pretty(v).ok())
}

/// The first `max_chars` chars of `text`, and whether it was cut.
pub fn clip(text: &str, max_chars: usize) -> (&str, bool) {
    match text.char_indices().nth(max_chars) {
        Some((i, _)) => (text.get(..i).unwrap_or(text), true),
        None => (text, false),
    }
}

/// Remove ANSI escape sequences (IPython colours its tracebacks).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI: parameters, then a final byte in @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: up to BEL or ESC \.
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Decode standard base64 (whitespace ignored, padding optional).
pub fn decode_base64(s: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some(u32::from(c - b'A')),
            b'a'..=b'z' => Some(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(c - b'0') + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    let mut done = false;
    for &b in s.as_bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            done = true;
            continue;
        }
        if done {
            return Err("data after base64 padding".into());
        }
        let v = val(b).ok_or_else(|| format!("invalid base64 character {:?}", char::from(b)))?;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xFF) as u8);
        }
        acc &= (1 << bits) - 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JUPYTER: &str = r##"{
 "cells": [
  {
   "cell_type": "markdown",
   "id": "a1b2c3d4",
   "metadata": {
    "tags": [
     "intro"
    ]
   },
   "source": [
    "# Title\n",
    "\n",
    "Some *text*."
   ]
  },
  {
   "cell_type": "code",
   "execution_count": 3,
   "id": "e5f6a7b8",
   "metadata": {
    "collapsed": false,
    "custom": {
     "x": 1
    }
   },
   "outputs": [
    {
     "name": "stdout",
     "output_type": "stream",
     "text": [
      "hello\n",
      "world\n"
     ]
    },
    {
     "data": {
      "image/png": "iVBORw0KGgo=",
      "text/html": [
       "<b>x</b>\n",
       "<i>y</i>"
      ],
      "text/plain": [
       "42"
      ]
     },
     "execution_count": 3,
     "metadata": {},
     "output_type": "execute_result"
    },
    {
     "ename": "ValueError",
     "evalue": "bad",
     "output_type": "error",
     "traceback": [
      "\u001b[0;31mValueError\u001b[0m: bad"
     ]
    }
   ],
   "source": [
    "print('hello')\n",
    "print('world')\n",
    "42"
   ]
  },
  {
   "cell_type": "code",
   "execution_count": null,
   "id": "00000001",
   "metadata": {},
   "outputs": [],
   "source": []
  }
 ],
 "metadata": {
  "kernelspec": {
   "display_name": "Python 3 (ipykernel)",
   "language": "python",
   "name": "python3"
  },
  "unknown_tool": {
   "keep": "me"
  }
 },
 "nbformat": 4,
 "nbformat_minor": 5,
 "x_extra": true
}
"##;

    #[test]
    fn jupyter_notebooks_round_trip_byte_for_byte() {
        let nb = Notebook::parse(JUPYTER).unwrap();
        assert_eq!(nb.cells.len(), 3);
        assert_eq!(nb.cells[0].kind, CellKind::Markdown);
        assert_eq!(nb.cells[0].source, "# Title\n\nSome *text*.");
        assert_eq!(nb.cells[0].id(), Some("a1b2c3d4"));
        assert_eq!(nb.cells[1].execution_count, Some(3));
        assert_eq!(nb.cells[1].outputs.len(), 3);
        assert_eq!(nb.language(), "python");
        assert_eq!(nb.kernel_display_name().as_deref(), Some("Python 3 (ipykernel)"));
        assert_eq!(nb.to_json(), JUPYTER);
        // Parse → write → parse is stable.
        assert_eq!(Notebook::parse(&nb.to_json()).unwrap().to_json(), JUPYTER);
    }

    #[test]
    fn output_views_join_lines_and_strip_ansi() {
        let nb = Notebook::parse(JUPYTER).unwrap();
        let outs = &nb.cells[1].outputs;
        assert_eq!(output_view(&outs[0]), OutputView::Stream { stderr: false, text: "hello\nworld\n" });
        let OutputView::Data { execution_count, data } = output_view(&outs[1]) else { panic!() };
        assert_eq!(execution_count, Some(3));
        assert_eq!(mime_text(data, "text/plain").as_deref(), Some("42"));
        assert_eq!(mime_text(data, "text/html").as_deref(), Some("<b>x</b>\n<i>y</i>"));
        assert_eq!(decode_base64(data["image/png"].as_str().unwrap()).unwrap(), b"\x89PNG\r\n\x1a\n");
        let OutputView::Error { ename, traceback, .. } = output_view(&outs[2]) else { panic!() };
        assert_eq!(ename, "ValueError");
        assert_eq!(strip_ansi(traceback[0]), "ValueError: bad");
        assert_eq!(output_view(&json!({"output_type": "weird"})), OutputView::Other { output_type: "weird" });
        assert!(matches!(output_view(&json!(42)), OutputView::Other { .. }));
    }

    #[test]
    fn edit_operations() {
        let mut nb = Notebook::parse(JUPYTER).unwrap();
        let at = nb.insert_cell(1, CellKind::Code, "x = 1");
        assert_eq!(at, 1);
        let id = nb.cells[1].id().unwrap().to_string();
        assert_eq!(id.len(), 8);
        assert!(nb.cells.iter().filter(|c| c.id() == Some(id.as_str())).count() == 1);
        assert_eq!(nb.insert_cell(999, CellKind::Markdown, "end"), 4);
        let uid = nb.cells[1].uid;
        assert_eq!(nb.move_cell(1, true).unwrap(), 0);
        assert_eq!(nb.index_of(uid), Some(0));
        assert_eq!(nb.move_cell(0, true).unwrap(), 0);
        assert_eq!(nb.move_cell(4, false).unwrap(), 4);
        assert!(nb.move_cell(5, false).is_err());
        // code → markdown drops outputs.
        let code = nb.cells.iter().position(|c| !c.outputs.is_empty()).unwrap();
        nb.set_kind(code, CellKind::Markdown).unwrap();
        assert!(nb.cells[code].outputs.is_empty() && nb.cells[code].execution_count.is_none());
        let json: Value = serde_json::from_str(&nb.to_json()).unwrap();
        let c = &json["cells"][code];
        assert_eq!(c["cell_type"], "markdown");
        assert!(c.get("outputs").is_none() && c.get("execution_count").is_none());
        assert_eq!(c["metadata"]["custom"]["x"], 1, "metadata survives a type change");
        nb.set_kind(code, CellKind::Code).unwrap();
        let json: Value = serde_json::from_str(&nb.to_json()).unwrap();
        assert_eq!(json["cells"][code]["outputs"], json!([]));
        assert_eq!(json["cells"][code]["execution_count"], Value::Null);
        nb.set_source(0, "y = 2\nz = 3\n").unwrap();
        let json: Value = serde_json::from_str(&nb.to_json()).unwrap();
        assert_eq!(json["cells"][0]["source"], json!(["y = 2\n", "z = 3\n"]));
        let removed = nb.delete_cell(0).unwrap();
        assert_eq!(removed.source, "y = 2\nz = 3\n");
        assert!(nb.delete_cell(99).is_err());
        assert!(nb.set_source(99, "").is_err());
        assert!(nb.set_kind(99, CellKind::Raw).is_err());
        assert!(nb.clear_outputs(99).is_err());
        nb.clear_all_outputs();
        assert!(nb.cells.iter().all(|c| c.outputs.is_empty()));
        // Unknown metadata and top-level keys survive edits.
        let json: Value = serde_json::from_str(&nb.to_json()).unwrap();
        assert_eq!(json["metadata"]["unknown_tool"]["keep"], "me");
        assert_eq!(json["x_extra"], true);
    }

    #[test]
    fn older_notebooks_get_no_cell_ids() {
        let text = r#"{"cells": [{"cell_type": "code", "metadata": {}, "source": "a", "outputs": [], "execution_count": 1}], "metadata": {}, "nbformat": 4, "nbformat_minor": 2}"#;
        let mut nb = Notebook::parse(text).unwrap();
        assert!(!nb.uses_cell_ids());
        nb.insert_cell(0, CellKind::Code, "");
        assert_eq!(nb.cells[0].id(), None);
        let json: Value = serde_json::from_str(&nb.to_json()).unwrap();
        assert!(json["cells"][0].get("id").is_none());
        assert_eq!(json["cells"][0]["source"], json!([]));
        assert_eq!(json["cells"][1]["source"], json!(["a"]));
    }

    #[test]
    fn hostile_notebooks_are_errors_or_tolerated() {
        for bad in ["{", "[]", "42", "null", r#"{"nbformat": 3}"#, r#"{"nbformat": "4"}"#, r#"{"cells": {}}"#,
            r#"{"cells": [1]}"#, r#"{"cells": [{"cell_type": "code", "source": 5}]}"#, r#"{"nbformat_minor": -1}"#]
        {
            assert!(Notebook::parse(bad).is_err(), "{bad}");
        }
        // Missing fields are tolerated and written back as they were where possible.
        let odd = r#"{"cells": [{"source": "x", "zzz": [1, 2]}, {"cell_type": "code"}, {"cell_type": "markdown", "source": ["a", "b"]},
            {"cell_type": "code", "source": "1", "execution_count": "x", "outputs": [{"output_type": "stream", "text": 7}, {"output_type": "display_data", "data": {"text/plain": ["a", 1]}}]}]}"#;
        let nb = Notebook::parse(odd).unwrap();
        assert_eq!(nb.cells.len(), 4);
        assert_eq!(nb.cells[0].kind, CellKind::Raw);
        assert_eq!(nb.cells[2].source, "ab");
        assert_eq!(nb.cells[3].execution_count, None);
        let json: Value = serde_json::from_str(&nb.to_json()).unwrap();
        assert!(json["cells"][0].get("cell_type").is_none(), "an unknown cell type is not invented");
        assert_eq!(json["cells"][0]["zzz"], json!([1, 2]));
        assert_eq!(json["nbformat"], 4);
        for o in &nb.cells[3].outputs {
            let _ = output_view(o);
        }
        // Empty text is a new notebook.
        let nb = Notebook::parse("  \n").unwrap();
        assert_eq!(nb.cells.len(), 1);
        assert!(Notebook::parse(&nb.to_json()).is_ok());
        // Unicode survives unescaped, controls escaped.
        let mut nb = Notebook::empty();
        nb.set_source(0, "s = 'é😀\u{1}\"\\'").unwrap();
        let text = nb.to_json();
        assert!(text.contains("é😀\\u0001\\\"\\\\"), "{text}");
        assert_eq!(Notebook::parse(&text).unwrap().cells[0].source, "s = 'é😀\u{1}\"\\'");
    }

    #[test]
    fn kernel_output_is_capped() {
        let mut c = Cell::new(CellKind::Code, "", None);
        c.append_stream("stdout", "a");
        c.append_stream("stdout", "b");
        c.append_stream("stderr", "e");
        c.append_stream("weird", "c");
        assert_eq!(c.outputs.len(), 3);
        assert_eq!(c.outputs[0]["text"], "ab");
        assert_eq!(c.outputs[2]["name"], "stdout");
        let chunk = "x".repeat(1 << 20);
        for _ in 0..5 {
            c.append_stream("stdout", &chunk);
        }
        assert!(c.kernel_bytes <= MAX_KERNEL_BYTES_PER_CELL);
        assert_eq!(c.outputs.last().unwrap()["text"], TRUNCATED_NOTE);
        let n = c.outputs.len();
        c.push_output(display_output(Map::new()));
        assert_eq!(c.outputs.len(), n, "nothing more after the note");
        c.clear_outputs();
        for _ in 0..MAX_OUTPUTS_PER_CELL + 10 {
            c.push_output(display_output(Map::new()));
        }
        assert_eq!(c.outputs.len(), MAX_OUTPUTS_PER_CELL + 1);
        c.clear_outputs();
        let mut data = Map::new();
        data.insert("image/png".into(), Value::String("A".repeat(MAX_IMAGE_BYTES + 1)));
        c.push_output(display_output(data));
        let OutputView::Data { data, .. } = output_view(&c.outputs[0]) else { panic!() };
        assert!(data.get("image/png").is_none());
        assert!(data.get("text/plain").is_some());
    }

    #[test]
    fn base64_and_clip() {
        assert_eq!(decode_base64("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(decode_base64("aGVs\nbG8").unwrap(), b"hello");
        assert_eq!(decode_base64("").unwrap(), b"");
        assert!(decode_base64("a*b").is_err());
        assert!(decode_base64("aA==aA").is_err());
        assert_eq!(clip("héllo", 2), ("hé", true));
        assert_eq!(clip("hé", 5), ("hé", false));
        assert_eq!(strip_ansi("\u{1b}]0;title\u{7}a\u{1b}[1;31mb\u{1b}"), "ab");
    }
}
