//! Show Recursive Macro Expansion (M12 track RS): `rust-analyzer/expandMacro` at the caret (a
//! `macro_rules!` call, a derive or an attribute macro), shown in a read-only popup with the
//! macro's name. rust-analyzer's text is shown at once; rustfmt then formats it on a worker thread
//! (expressions and statements are formatted inside a wrapper function) and the popup updates.

use std::path::{Path, PathBuf};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_lsp::ResponseError;
use codecraft_syntax::Language;

use crate::lsp::Pending;
use crate::popup::ActionItem;
use crate::rust_tools::RustPending;
use crate::{CommandError, Session};

/// The expansion shown in the popup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroExpansion {
    /// The macro's name (`vec`, `Debug`, `tokio::main`).
    pub name: String,
    pub text: String,
    /// rustfmt formatted `text` (false while formatting or when rustfmt could not).
    pub formatted: bool,
    /// Where the macro call is.
    pub path: PathBuf,
    pub line: usize,
    pub request: u64,
}

#[derive(Default)]
pub struct MacroState {
    pub view: Option<MacroExpansion>,
    /// The request waiting for the server.
    pub pending: Option<u64>,
    next: u64,
    format_rx: Option<Receiver<(u64, Option<String>)>>,
}

impl MacroState {
    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.format_rx.is_some()
    }
}

/// `rust.showRecursiveMacroExpansion`: ask rust-analyzer for the expansion at the caret.
pub(crate) fn request(s: &mut Session) -> Result<Value, CommandError> {
    let language = s.ws.active().map(|(_, d)| d.language);
    if language != Some(Language::Rust) {
        return Err(CommandError::Disabled("macro expansion works in Rust files".into()));
    }
    let (_, path, _, pos) = s.caret_for_lsp()?;
    let line = pos.line as usize;
    s.rust.macros.next = s.rust.macros.next.wrapping_add(1);
    let request = s.rust.macros.next;
    let p = path.clone();
    s.lsp_send(
        |c| c.expand_macro(&p, pos.line, pos.character),
        Pending::Rust(RustPending::ExpandMacro { request, path, line }),
    )?;
    s.rust.macros.pending = Some(request);
    s.status = "Expanding macro…".into();
    Ok(json!({ "request": request }))
}

/// The server's answer.
pub(crate) fn on_response(
    s: &mut Session,
    request: u64,
    path: PathBuf,
    line: usize,
    result: Result<Value, ResponseError>,
) {
    if s.rust.macros.pending != Some(request) {
        return;
    }
    s.rust.macros.pending = None;
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            if e.code != -32800 && e.code != -32801 {
                s.status = format!("Macro expansion failed: {}", e.message);
            }
            return;
        }
    };
    let Some(expanded) = codecraft_lsp::decode_expanded_macro(value) else {
        s.status = "No macro call at the caret".into();
        return;
    };
    s.status = format!("Expansion of {}", expanded.name);
    let text = expanded.expansion.clone();
    s.rust.macros.view = Some(MacroExpansion {
        name: expanded.name,
        text: text.clone(),
        formatted: false,
        path: path.clone(),
        line,
        request,
    });
    let edition =
        s.ws.project
            .as_ref()
            .and_then(|p| p.cargo.as_ref())
            .and_then(|m| m.package_for_file(&path))
            .map(|p| p.edition.clone())
            .unwrap_or_else(|| "2021".into());
    let cwd = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let spawned = std::thread::Builder::new().name("codecraft-macro-format".into()).spawn(move || {
        let _ = tx.send((request, format_expansion(&text, &edition, &cwd)));
    });
    if spawned.is_ok() {
        s.rust.macros.format_rx = Some(rx);
    }
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(rx) = &s.rust.macros.format_rx else { return false };
    match rx.try_recv() {
        Ok((request, formatted)) => {
            s.rust.macros.format_rx = None;
            if let Some(view) = &mut s.rust.macros.view
                && view.request == request
                && let Some(text) = formatted
            {
                view.text = text;
                view.formatted = true;
            }
            true
        }
        Err(TryRecvError::Empty) => false,
        Err(TryRecvError::Disconnected) => {
            s.rust.macros.format_rx = None;
            true
        }
    }
}

const DOLLAR_CRATE: &str = "__codecraft_dollar_crate__";
const WRAPPER: &str = "fn __codecraft_expansion__() {";

/// Format an expansion with rustfmt: as items, else as the body of a function (expressions and
/// statements). `None` when rustfmt is missing or cannot parse it.
pub fn format_expansion(text: &str, edition: &str, cwd: &Path) -> Option<String> {
    if text.trim().is_empty() || text.len() > 4 << 20 {
        return None;
    }
    let source = text.replace("$crate", DOLLAR_CRATE);
    let restore = |t: String| t.replace(DOLLAR_CRATE, "$crate").trim_end().to_string();
    if let Ok(t) = codecraft_cargo::rustfmt(&source, edition, cwd)
        && !t.trim().is_empty()
    {
        return Some(restore(t));
    }
    let wrapped = format!("{WRAPPER}\n{source}\n}}\n");
    let formatted = codecraft_cargo::rustfmt(&wrapped, edition, cwd).ok()?;
    let lines: Vec<&str> = formatted.trim_end().lines().collect();
    let first = lines.first()?;
    if !first.starts_with("fn __codecraft_expansion__()") || lines.last().map(|l| l.trim()) != Some("}") {
        return None;
    }
    let body = lines.get(1..lines.len().saturating_sub(1))?;
    let out: Vec<String> = body.iter().map(|l| l.strip_prefix("    ").unwrap_or(l).to_string()).collect();
    let joined = out.join("\n");
    (!joined.trim().is_empty()).then(|| restore(joined))
}

/// Whether the caret sits on a macro call (`name!`), in a derive list or an attribute.
pub fn caret_on_macro(line: &str, column: usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let column = column.min(chars.len());
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = column;
    while start > 0 && chars.get(start - 1).is_some_and(|c| ident(*c)) {
        start -= 1;
    }
    let mut end = column;
    while chars.get(end).is_some_and(|c| ident(*c)) {
        end += 1;
    }
    if end > start && chars.get(end) == Some(&'!') {
        return true;
    }
    if column > 0
        && chars.get(column.saturating_sub(1)) == Some(&'!')
        && column >= 2
        && chars.get(column - 2).is_some_and(|c| ident(*c))
    {
        return true;
    }
    let trimmed = line.trim_start();
    if trimmed.starts_with("#[") || trimmed.starts_with("#![") {
        let lead = chars.len() - trimmed.chars().count();
        return column >= lead;
    }
    false
}

/// Alt+Enter on a macro call in Rust: Show Recursive Macro Expansion.
pub(crate) fn context_actions(s: &Session) -> Vec<ActionItem> {
    let Some((e, d)) = s.ws.active() else { return Vec::new() };
    if d.language != Language::Rust {
        return Vec::new();
    }
    let pos = d.doc.offset_to_position(e.selections.primary().head);
    let line = d.doc.line_text(pos.line).unwrap_or_default();
    if !caret_on_macro(&line, pos.column) {
        return Vec::new();
    }
    let title = "Show recursive macro expansion";
    vec![ActionItem {
        title: title.into(),
        kind: Some("macro".into()),
        payload: json!({ "title": title, "codecraftCommand": "rust.showRecursiveMacroExpansion" }),
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macro_calls_under_the_caret() {
        assert!(caret_on_macro("    println!(\"x\");", 6));
        assert!(caret_on_macro("    println!(\"x\");", 4));
        assert!(caret_on_macro("    println!(\"x\");", 12));
        assert!(caret_on_macro("#[derive(Debug, Clone)]", 10));
        assert!(caret_on_macro("    #[tokio::main]", 6));
        assert!(!caret_on_macro("    let x = foo(1);", 13));
        assert!(!caret_on_macro("    let x = !y;", 12));
        assert!(!caret_on_macro("", 5));
        // A column past the end is clamped, never a panic.
        assert!(!caret_on_macro("vec![1, 2]", 99));
        assert!(caret_on_macro("vec![1, 2]", 0));
    }

    #[test]
    fn formatting_items_and_expressions() {
        if codecraft_cargo::rustfmt("fn a() {}", "2021", Path::new(".")).is_err() {
            eprintln!("rustfmt not installed; skipping");
            return;
        }
        let items = format_expansion("impl  Foo{fn a(&self)->u8{1}}", "2021", Path::new(".")).unwrap();
        assert_eq!(items, "impl Foo {\n    fn a(&self) -> u8 {\n        1\n    }\n}");
        let expr =
            format_expansion("{$crate::io::_print(format_args!(\"{}\\n\",x));}", "2021", Path::new(".")).unwrap();
        assert!(expr.contains("$crate::io::_print(format_args!(\"{}\\n\", x));"), "{expr}");
        assert!(!expr.contains("__codecraft"), "{expr}");
        assert_eq!(format_expansion("   ", "2021", Path::new(".")), None);
        assert_eq!(format_expansion("fn (", "2021", Path::new(".")), None);
    }
}
