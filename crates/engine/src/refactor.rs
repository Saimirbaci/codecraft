//! Refactorings with preview, file operations and hierarchies (M7, track R).
//!
//! - Every multi-file edit (rename, code actions, file moves, safe delete) becomes an
//!   [`EditPreview`] the user (or an agent) reviews and applies; single-file edits apply at once.
//! - Refactor This, extract/inline, Generate and Implement/Override Methods are rust-analyzer
//!   assists (code actions narrowed by kind and assist id), resolved and applied through the same
//!   path.
//! - Call/Type/Method hierarchies fill the Hierarchy tool window lazily.
//! - Move/Copy/Rename/Safe Delete of files, with `workspace/willRenameFiles` edits and a usage
//!   check before deleting.

pub mod file_ops;
pub mod hierarchy;
pub mod preview;

use std::path::{Path, PathBuf};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codecraft_lsp::lsp_types as lt;
use codecraft_lsp::{ResponseError, ServerKind};
use codecraft_syntax::Language;
use codecraft_workspace::{EditorId, NotificationLevel, ToolWindowId};

use crate::popup::ActionItem;
use crate::{CommandError, Popup, Session, convert};

pub use file_ops::{Usage, UsageKind};
pub use hierarchy::{Direction, Hierarchy, HierarchyKind, HierarchyNode};
pub use preview::{ApplyReport, EditPreview, EntryKind, PreviewEntry};

/// Dialogs of the file refactorings (drawn by the UI, driven by commands).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum RefactorDialog {
    /// Move File (F6): `target` is the destination directory (or full path).
    MoveFile { path: PathBuf, target: String },
    /// Copy File (F5): `target` is the destination directory, `name` the new name.
    CopyFile { path: PathBuf, target: String, name: String },
    /// Rename a file or directory in place.
    RenameFile { path: PathBuf, name: String },
    /// Safe Delete: `usages` is `None` while the search runs.
    SafeDelete { path: PathBuf, usages: Option<Vec<Usage>> },
}

impl RefactorDialog {
    pub fn path(&self) -> &Path {
        match self {
            Self::MoveFile { path, .. }
            | Self::CopyFile { path, .. }
            | Self::RenameFile { path, .. }
            | Self::SafeDelete { path, .. } => path,
        }
    }
}

#[derive(Default)]
pub struct RefactorState {
    /// The multi-file edit waiting for Apply/Cancel.
    pub preview: Option<EditPreview>,
    pub dialog: Option<RefactorDialog>,
    /// The Hierarchy tool window's tree.
    pub hierarchy: Option<Hierarchy>,
    /// What the last applied refactoring did.
    pub last_report: Option<ApplyReport>,
    usages: Option<Receiver<(PathBuf, Vec<Usage>)>>,
    generation: u64,
}

pub(crate) fn pump(s: &mut Session) -> bool {
    let Some(rx) = &s.refactor.usages else { return false };
    let Ok((path, usages)) = rx.try_recv() else { return false };
    s.refactor.usages = None;
    if let Some(RefactorDialog::SafeDelete { path: p, usages: u }) = &mut s.refactor.dialog
        && *p == path
    {
        *u = Some(usages);
    }
    true
}

// ---------------------------------------------------------------- code action filters

/// Which code actions a refactoring command offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActionFilter {
    RefactorThis,
    ExtractVariable,
    ExtractFunction,
    ExtractConstant,
    Inline,
    ChangeSignature,
    Generate,
    ImplementMembers,
    OverrideMembers,
}

/// rust-analyzer's assist id (`data.id` is `extract_variable:RefactorExtract:0`).
pub(crate) fn assist_id(action: &Value) -> Option<&str> {
    let id = action.get("data")?.get("id")?.as_str()?;
    id.split(':').next()
}

impl ActionFilter {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::RefactorThis => "Refactor This",
            Self::ExtractVariable => "Extract Variable",
            Self::ExtractFunction => "Extract Function",
            Self::ExtractConstant => "Extract Constant",
            Self::Inline => "Inline",
            Self::ChangeSignature => "Change Signature",
            Self::Generate => "Generate",
            Self::ImplementMembers => "Implement Members",
            Self::OverrideMembers => "Override Members",
        }
    }

    /// Code action kinds to ask for (`only`); empty = all (rust-analyzer reports Generate
    /// assists with the empty kind and Implement Members as a quick fix).
    fn only(self) -> &'static [&'static str] {
        match self {
            Self::RefactorThis | Self::ChangeSignature => &["refactor"],
            Self::ExtractVariable | Self::ExtractFunction | Self::ExtractConstant => &["refactor.extract"],
            Self::Inline => &["refactor.inline"],
            Self::Generate | Self::ImplementMembers | Self::OverrideMembers => &[],
        }
    }

    /// Apply right away when exactly one action matches (else a chooser opens).
    fn auto_apply(self) -> bool {
        !matches!(self, Self::RefactorThis | Self::Generate | Self::ChangeSignature)
    }

    fn empty_message(self) -> &'static str {
        match self {
            Self::RefactorThis => "No refactorings available here",
            Self::ExtractVariable => "Cannot perform refactoring: select an expression to extract into a variable",
            Self::ExtractFunction => "Cannot perform refactoring: select statements or an expression to extract",
            Self::ExtractConstant => "Cannot perform refactoring: select a constant expression to extract",
            Self::Inline => "Cannot perform refactoring: put the caret on a variable, function call or type alias",
            Self::ChangeSignature => "No signature refactorings available here",
            Self::Generate => "Nothing to generate here",
            Self::ImplementMembers => "No methods to implement have been found",
            Self::OverrideMembers => "No methods to override have been found",
        }
    }

    pub(crate) fn matches(self, action: &Value) -> bool {
        let id = assist_id(action).unwrap_or("");
        let title = action.get("title").and_then(Value::as_str).unwrap_or("");
        let kind = action.get("kind").and_then(Value::as_str).unwrap_or("");
        match self {
            Self::RefactorThis => kind.starts_with("refactor"),
            Self::ExtractVariable => id == "extract_variable" || title.starts_with("Extract into variable"),
            Self::ExtractFunction => id == "extract_function" || title.starts_with("Extract into function"),
            Self::ExtractConstant => id == "extract_constant" || title.starts_with("Extract into constant"),
            Self::Inline => id.starts_with("inline_") || kind.starts_with("refactor.inline"),
            Self::ChangeSignature => {
                ["param", "return_type", "signature", "arg"].iter().any(|k| id.contains(k))
                    && kind.starts_with("refactor")
            }
            Self::Generate => {
                id.starts_with("generate_")
                    || matches!(
                        id,
                        "add_missing_impl_members" | "add_missing_default_members" | "replace_derive_with_manual_impl"
                    )
                    || (id.is_empty() && title.starts_with("Generate"))
            }
            Self::ImplementMembers => id == "add_missing_impl_members" || title == "Implement missing members",
            Self::OverrideMembers => id == "add_missing_default_members" || title == "Implement default members",
        }
    }
}

/// Filter a code action response to the actions a command offers.
pub(crate) fn filter_actions(filter: ActionFilter, value: &Value) -> Vec<ActionItem> {
    let Some(list) = value.as_array() else { return Vec::new() };
    list.iter()
        .filter(|a| filter.matches(a))
        .filter_map(|a| {
            let title = a.get("title").and_then(Value::as_str)?.to_string();
            let kind = a.get("kind").and_then(Value::as_str).map(str::to_string);
            Some(ActionItem { title, kind, payload: a.clone() })
        })
        .take(200)
        .collect()
}

/// A popup entry that runs an engine command instead of a code action.
pub(crate) fn command_item(title: &str, command: &str) -> ActionItem {
    ActionItem {
        title: title.to_string(),
        kind: Some("file".into()),
        payload: json!({ "title": title, "codecraftCommand": command }),
    }
}

// ---------------------------------------------------------------- pending LSP requests

pub(crate) enum RefactorPending {
    PrepareRename { editor: EditorId, offset: usize, fallback: String },
    Rename { title: String, preview: bool },
    Actions { editor: EditorId, filter: ActionFilter, path: PathBuf, range: lt::Range, retries: u8 },
    HierarchyRoots { generation: u64 },
    HierarchyChildren { generation: u64, node: usize },
    Implementations { generation: u64 },
    SuperMethod { path: PathBuf, line: u32 },
    ParentModule,
    WillRename { renames: Vec<(PathBuf, PathBuf)>, title: String },
}

/// Route a response (or error) for a refactoring request.
pub(crate) fn on_response(s: &mut Session, pending: RefactorPending, result: Result<Value, ResponseError>) {
    match pending {
        RefactorPending::PrepareRename { editor, offset, fallback } => {
            on_prepare_rename(s, editor, offset, fallback, result)
        }
        RefactorPending::Rename { title, preview } => match result {
            Err(e) => cannot("Cannot Rename", s, &e.message),
            Ok(v) => {
                let edit: Option<lt::WorkspaceEdit> = serde_json::from_value(v).ok().flatten();
                match edit {
                    Some(edit) => match apply_or_preview(s, &title, &edit, preview) {
                        Ok(0) => {}
                        Ok(n) => s.status = format!("Renamed in {n} file(s)"),
                        Err(e) => cannot("Cannot Rename", s, &e.to_string()),
                    },
                    None => s.status = "Nothing to rename".into(),
                }
            }
        },
        RefactorPending::Actions { editor, filter, path, range, retries } => match result {
            // The document changed under the server (e.g. right after an edit): ask again.
            Err(e) if e.code == -32801 && retries < 3 => {
                if let Err(err) = send_actions(s, editor, filter, path, range, retries + 1) {
                    s.status = err.to_string();
                }
            }
            result => on_actions(s, editor, filter, result),
        },
        RefactorPending::HierarchyRoots { generation } => on_hierarchy_roots(s, generation, result),
        RefactorPending::HierarchyChildren { generation, node } => on_hierarchy_children(s, generation, node, result),
        RefactorPending::Implementations { generation } => on_implementations(s, generation, result),
        RefactorPending::SuperMethod { path, line } => on_super_method(s, &path, line, result),
        RefactorPending::ParentModule => {
            let locs = result.map(hierarchy::locations).unwrap_or_default();
            match locs.first() {
                Some((p, r)) => navigate_lsp(s, p, r),
                None => s.status = "No super method or parent module found".into(),
            }
        }
        RefactorPending::WillRename { renames, title } => {
            let edit: Option<lt::WorkspaceEdit> = result.ok().and_then(|v| serde_json::from_value(v).ok()).flatten();
            if let Err(e) = finish_move(s, &title, &renames, edit.as_ref()) {
                cannot(&title, s, &e.to_string());
            }
        }
    }
}

/// Report a refactoring that cannot be performed (RustRover's "Cannot perform refactoring").
fn cannot(title: &str, s: &mut Session, message: &str) {
    s.status = format!("{title}: {message}");
    s.popup = Some(Popup::Message { title: title.to_string(), text: message.to_string() });
}

// ---------------------------------------------------------------- previews

/// Apply a workspace edit at once when it touches one file, else open the preview (always with
/// `force_preview`). Returns the number of files changed (0 when the preview opened).
pub(crate) fn apply_or_preview(
    s: &mut Session,
    title: &str,
    edit: &lt::WorkspaceEdit,
    force_preview: bool,
) -> Result<usize, CommandError> {
    let preview = s.build_preview(title, edit)?;
    if preview.is_empty() {
        s.status = format!("{title}: nothing to change");
        return Ok(0);
    }
    if force_preview || preview.needs_preview() {
        open_preview(s, preview);
        return Ok(0);
    }
    let report = s.apply_preview(&preview);
    finish_report(s, title, report)
}

fn open_preview(s: &mut Session, preview: EditPreview) {
    s.status = format!("{}: review {} file(s), then Apply", preview.title, preview.files());
    s.refactor.preview = Some(preview);
}

fn finish_report(s: &mut Session, title: &str, report: ApplyReport) -> Result<usize, CommandError> {
    let n = report.files.len();
    let errors = report.errors.clone();
    s.refactor.last_report = Some(report);
    if errors.is_empty() { Ok(n) } else { Err(CommandError::Failed(format!("{title}: {}", errors.join("; ")))) }
}

/// Apply the open preview (the included entries).
pub(crate) fn apply_open_preview(s: &mut Session) -> Result<ApplyReport, CommandError> {
    let preview =
        s.refactor.preview.take().ok_or_else(|| CommandError::Failed("no refactoring preview is open".into()))?;
    let report = s.apply_preview(&preview);
    s.status = if report.errors.is_empty() {
        format!("{}: changed {} file(s)", preview.title, report.files.len())
    } else {
        format!("{}: {} error(s)", preview.title, report.errors.len())
    };
    s.refactor.last_report = Some(report.clone());
    Ok(report)
}

// ---------------------------------------------------------------- positions

/// Put the caret at `{path, line, column}` when given (zero-based), then the caret for LSP.
pub(crate) fn position(s: &mut Session, p: &Value) -> Result<(EditorId, PathBuf, usize, lt::Position), CommandError> {
    if let Some(path) = crate::params::opt_path(p, "path") {
        let line = crate::params::opt_usize(p, "line")?.unwrap_or(0);
        let column = crate::params::opt_usize(p, "column")?.unwrap_or(0);
        let path = crate::normalize_path(&path);
        if !path.is_file() {
            return Err(CommandError::Failed(format!("{} does not exist", path.display())));
        }
        s.navigate_to_position(&path, line, column)?;
    }
    s.caret_for_lsp()
}

/// The word at the caret of the active editor.
fn word_at_caret(s: &Session) -> String {
    let Some((e, d)) = s.ws.active() else { return String::new() };
    let r = codecraft_text::movement::word_range_at(&d.doc, e.selections.primary().head);
    d.doc.slice(r)
}

fn navigate_lsp(s: &mut Session, path: &Path, range: &lt::Range) {
    let text = s.current_text(path).unwrap_or_default();
    let line = range.start.line as usize;
    let col = convert::utf16_col_to_char(text.lines().nth(line).unwrap_or(""), range.start.character);
    if let Err(e) = s.navigate_to_position(path, line, col) {
        s.status = e.to_string();
    }
}

// ---------------------------------------------------------------- rename

/// Refactor › Rename: without a name, ask the server whether the symbol can be renamed
/// (reporting why not), then open the rename popup; with a name, rename (previewing multi-file
/// results, or always with `preview`).
pub(crate) fn rename(s: &mut Session, new_name: Option<&str>, preview: bool) -> Result<(), CommandError> {
    let (editor, path, offset, pos) = s.caret_for_lsp()?;
    match new_name {
        None => {
            let fallback = word_at_caret(s);
            s.lsp_send(
                |c| c.prepare_rename(&path, pos.line, pos.character),
                crate::lsp::Pending::Refactor(RefactorPending::PrepareRename { editor, offset, fallback }),
            )
        }
        Some(name) => {
            let name = name.trim();
            if name.is_empty() {
                return Err(CommandError::BadParams("the new name is empty".into()));
            }
            s.popup = None;
            let title = format!("Rename '{}' to '{name}'", word_at_caret(s));
            let n = name.to_string();
            s.lsp_send(
                |c| c.rename(&path, pos.line, pos.character, &n),
                crate::lsp::Pending::Refactor(RefactorPending::Rename { title, preview }),
            )
        }
    }
}

fn on_prepare_rename(
    s: &mut Session,
    editor: EditorId,
    offset: usize,
    fallback: String,
    result: Result<Value, ResponseError>,
) {
    let value = match result {
        Ok(v) => v,
        // Servers without prepareRename: ask for the name anyway.
        Err(e) if e.code == -32601 => Value::Null,
        Err(e) => return cannot("Cannot Rename", s, &e.message),
    };
    let placeholder = match serde_json::from_value::<Option<lt::PrepareRenameResponse>>(value.clone()) {
        Ok(Some(lt::PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. })) => placeholder,
        Ok(Some(lt::PrepareRenameResponse::Range(r))) => {
            s.ws.editor(editor)
                .and_then(|e| s.ws.doc(e.doc))
                .map(|d| d.doc.slice(convert::from_lsp_range(&d.doc, &r)))
                .unwrap_or(fallback)
        }
        Ok(Some(lt::PrepareRenameResponse::DefaultBehavior { .. })) => fallback,
        Ok(None) if value.is_null() && !fallback.is_empty() => {
            return cannot("Cannot Rename", s, "The element at the caret cannot be renamed");
        }
        _ => fallback,
    };
    if placeholder.is_empty() {
        return cannot("Cannot Rename", s, "Nothing to rename at the caret");
    }
    s.popup = Some(Popup::Rename { editor, offset, name: placeholder });
}

// ---------------------------------------------------------------- code actions

/// Ask for the code actions a command offers at the caret/selection.
pub(crate) fn code_actions(s: &mut Session, filter: ActionFilter, p: &Value) -> Result<(), CommandError> {
    if filter == ActionFilter::RefactorThis && s.lsp_ready().is_err() {
        // No language server: the file refactorings still work.
        let editor = s.active_ids().map(|(e, _)| e).ok_or_else(|| CommandError::Disabled("no editor".into()))?;
        s.popup = Some(Popup::Actions {
            editor,
            title: filter.title().into(),
            items: static_refactor_items(s, false),
            selected: 0,
            pending: false,
        });
        return Ok(());
    }
    let (editor, path, _, _) = position(s, p)?;
    let range = {
        let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor".into()))?;
        let sel = e.selections.primary();
        convert::lsp_range(&d.doc, sel.start()..sel.end())
    };
    if !filter.auto_apply() {
        s.popup = Some(Popup::Actions {
            editor,
            title: filter.title().into(),
            items: Vec::new(),
            selected: 0,
            pending: true,
        });
    } else {
        s.status = format!("{}…", filter.title());
    }
    send_actions(s, editor, filter, path, range, 0)
}

fn send_actions(
    s: &mut Session,
    editor: EditorId,
    filter: ActionFilter,
    path: PathBuf,
    range: lt::Range,
    retries: u8,
) -> Result<(), CommandError> {
    let only = filter.only();
    let p = path.clone();
    s.lsp_send(
        |c| c.code_action_only(&p, range, Vec::new(), only),
        crate::lsp::Pending::Refactor(RefactorPending::Actions { editor, filter, path, range, retries }),
    )
}

/// File refactorings offered by Refactor This next to the server's.
fn static_refactor_items(s: &Session, lsp: bool) -> Vec<ActionItem> {
    let mut items = Vec::new();
    if lsp {
        items.push(command_item("Rename…", "refactor.rename"));
    }
    if s.ws.active().is_some_and(|(_, d)| d.path.is_some()) {
        items.push(command_item("Move File…", "refactor.moveFile"));
        items.push(command_item("Copy File…", "refactor.copyFile"));
    }
    items
}

fn on_actions(s: &mut Session, editor: EditorId, filter: ActionFilter, result: Result<Value, ResponseError>) {
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            if matches!(&s.popup, Some(Popup::Actions { pending: true, .. })) {
                s.popup = None;
            }
            if e.code != -32800 && e.code != -32801 {
                s.status = format!("{}: {}", filter.title(), e.message);
            }
            return;
        }
    };
    let mut items = filter_actions(filter, &value);
    if filter == ActionFilter::RefactorThis {
        let mut all = static_refactor_items(s, true);
        all.append(&mut items);
        items = all;
    }
    if items.is_empty() {
        if matches!(&s.popup, Some(Popup::Actions { pending: true, .. })) {
            s.popup = None;
        }
        s.status = filter.empty_message().into();
        return;
    }
    if filter.auto_apply() && items.len() == 1 {
        s.status.clear();
        if let Some(item) = items.first()
            && let Err(e) = s.apply_code_action(&item.payload)
        {
            s.status = e.to_string();
        }
        return;
    }
    s.popup = Some(Popup::Actions { editor, title: filter.title().into(), items, selected: 0, pending: false });
}

// ---------------------------------------------------------------- hierarchies

/// Navigate › Call/Type/Method Hierarchy at the caret (or `{path, line, column}`).
pub(crate) fn start_hierarchy(s: &mut Session, kind: HierarchyKind, p: &Value) -> Result<(), CommandError> {
    let (_, path, offset, pos) = position(s, p)?;
    s.refactor.generation = s.refactor.generation.wrapping_add(1);
    let generation = s.refactor.generation;
    let title = match kind {
        HierarchyKind::Call => "Call Hierarchy",
        HierarchyKind::Type => "Type Hierarchy",
        HierarchyKind::Method => "Method Hierarchy",
    };
    let mut h = Hierarchy::new(kind, title.into(), generation);
    // Type/method fallbacks root the tree at the symbol under the caret.
    if kind != HierarchyKind::Call {
        let name = word_at_caret(s);
        let line_col = s.ws.active().map(|(_, d)| d.doc.offset_to_position(offset));
        if let Some(lc) = line_col {
            let root = HierarchyNode {
                name: if name.is_empty() { "?".into() } else { name },
                detail: None,
                kind: "symbol".into(),
                path: path.clone(),
                line: lc.line,
                column: lc.column,
                sites: 0,
                depth: 0,
                children: None,
                expanded: true,
                loading: true,
                item: None,
            };
            if let Some(i) = h.push(root) {
                h.roots.push(i);
                h.selected = Some(i);
            }
        }
    }
    s.refactor.hierarchy = Some(h);
    s.ws.layout.show(ToolWindowId::Hierarchy);
    let (line, ch) = (pos.line, pos.character);
    match kind {
        HierarchyKind::Call => s.lsp_send(
            |c| c.prepare_call_hierarchy(&path, line, ch),
            crate::lsp::Pending::Refactor(RefactorPending::HierarchyRoots { generation }),
        ),
        HierarchyKind::Type => s.lsp_send(
            |c| c.prepare_type_hierarchy(&path, line, ch),
            crate::lsp::Pending::Refactor(RefactorPending::HierarchyRoots { generation }),
        ),
        HierarchyKind::Method => implementations(s, &path, pos, generation),
    }
}

fn implementations(s: &mut Session, path: &Path, pos: lt::Position, generation: u64) -> Result<(), CommandError> {
    if let Some(h) = &mut s.refactor.hierarchy {
        h.server_types = false;
    }
    s.lsp_send(
        |c| c.implementation(path, pos.line, pos.character),
        crate::lsp::Pending::Refactor(RefactorPending::Implementations { generation }),
    )
}

fn current_hierarchy(s: &mut Session, generation: u64) -> Option<&mut Hierarchy> {
    s.refactor.hierarchy.as_mut().filter(|h| h.generation == generation)
}

fn on_hierarchy_roots(s: &mut Session, generation: u64, result: Result<Value, ResponseError>) {
    let Some(kind) = current_hierarchy(s, generation).map(|h| h.kind) else { return };
    let value = match result {
        Ok(v) => v,
        Err(e) if kind == HierarchyKind::Type => {
            // No typeHierarchy (rust-analyzer): subtypes are the implementations.
            log::debug!("typeHierarchy unavailable ({}); using implementations", e.message);
            let root = s.refactor.hierarchy.as_ref().and_then(|h| h.roots.first().and_then(|r| h.nodes.get(*r)));
            let Some((path, line, column)) = root.map(|n| (n.path.clone(), n.line, n.column)) else { return };
            let pos = s.ws.doc_by_path(&path).and_then(|id| s.ws.doc(id)).map(|d| {
                convert::lsp_pos(&d.doc, d.doc.position_to_offset(codecraft_text::Position::new(line, column)))
            });
            if let Some(pos) = pos
                && let Err(e) = implementations(s, &path, pos, generation)
            {
                set_message(s, generation, e.to_string());
            }
            return;
        }
        Err(e) => return set_message(s, generation, e.message),
    };
    let items: Vec<Value> = value.as_array().cloned().unwrap_or_default();
    let read = |p: &Path| s.current_text(p);
    let mut lines = hierarchy::LineCache::new(&read);
    let nodes: Vec<HierarchyNode> = items.iter().filter_map(|i| hierarchy::node_from_item(i, 0, &mut lines)).collect();
    drop(lines);
    let Some(h) = current_hierarchy(s, generation) else { return };
    h.pending = false;
    if nodes.is_empty() {
        h.message = Some(match kind {
            HierarchyKind::Call => "No call hierarchy at the caret (put it on a function or method)".into(),
            _ => "No type hierarchy at the caret".into(),
        });
        return;
    }
    h.nodes.clear();
    h.roots.clear();
    for n in nodes {
        if let Some(i) = h.push(n) {
            h.roots.push(i);
        }
    }
    h.selected = h.roots.first().copied();
    if let Some(first) = h.roots.first().copied() {
        let _ = toggle_node(s, first);
    }
}

fn set_message(s: &mut Session, generation: u64, message: String) {
    if let Some(h) = current_hierarchy(s, generation) {
        h.pending = false;
        h.message = Some(message);
        for n in &mut h.nodes {
            n.loading = false;
        }
    }
}

/// Expand (loading children on first use) or collapse a hierarchy node.
pub(crate) fn toggle_node(s: &mut Session, node: usize) -> Result<(), CommandError> {
    let h = s.refactor.hierarchy.as_mut().ok_or_else(|| CommandError::Failed("no hierarchy is shown".into()))?;
    let (generation, direction, server_types) = (h.generation, h.direction, h.server_types);
    let n = h.nodes.get_mut(node).ok_or_else(|| CommandError::BadParams(format!("no node {node}")))?;
    h.selected = Some(node);
    if n.children.is_some() || n.loading {
        n.expanded = !n.expanded;
        return Ok(());
    }
    let Some(item) = n.item.clone() else {
        n.children = Some(Vec::new());
        return Ok(());
    };
    n.loading = true;
    n.expanded = true;
    let pending = crate::lsp::Pending::Refactor(RefactorPending::HierarchyChildren { generation, node });
    let sent = match direction {
        Direction::Callers => s.lsp_send(|c| c.call_hierarchy_incoming(item), pending),
        Direction::Callees => s.lsp_send(|c| c.call_hierarchy_outgoing(item), pending),
        Direction::Subtypes if server_types => s.lsp_send(|c| c.type_hierarchy_subtypes(item), pending),
        Direction::Supertypes if server_types => s.lsp_send(|c| c.type_hierarchy_supertypes(item), pending),
        Direction::Subtypes | Direction::Supertypes => Ok(()),
    };
    if let Err(e) = sent {
        if let Some(n) = s.refactor.hierarchy.as_mut().and_then(|h| h.nodes.get_mut(node)) {
            n.loading = false;
            n.children = Some(Vec::new());
        }
        return Err(e);
    }
    Ok(())
}

fn on_hierarchy_children(s: &mut Session, generation: u64, node: usize, result: Result<Value, ResponseError>) {
    let Some((direction, depth)) = current_hierarchy(s, generation)
        .and_then(|h| h.nodes.get(node).map(|n| (h.direction, n.depth.saturating_add(1))))
    else {
        return;
    };
    let value = match result {
        Ok(v) => v,
        Err(e) => {
            if let Some(n) = current_hierarchy(s, generation).and_then(|h| h.nodes.get_mut(node)) {
                n.loading = false;
                n.children = Some(Vec::new());
            }
            s.status = e.message;
            return;
        }
    };
    let read = |p: &Path| s.current_text(p);
    let mut lines = hierarchy::LineCache::new(&read);
    let kids: Vec<HierarchyNode> = hierarchy::child_items(direction, &value)
        .into_iter()
        .filter_map(|(item, sites)| {
            hierarchy::node_from_item(&item, depth, &mut lines).map(|mut n| {
                n.sites = sites;
                n
            })
        })
        .collect();
    drop(lines);
    let Some(h) = current_hierarchy(s, generation) else { return };
    let mut ids = Vec::with_capacity(kids.len());
    for k in kids {
        match h.push(k) {
            Some(i) => ids.push(i),
            None => break,
        }
    }
    if let Some(n) = h.nodes.get_mut(node) {
        n.loading = false;
        n.children = Some(ids);
    }
}

fn on_implementations(s: &mut Session, generation: u64, result: Result<Value, ResponseError>) {
    let locs = match result {
        Ok(v) => hierarchy::locations(v),
        Err(e) => return set_message(s, generation, e.message),
    };
    let read = |p: &Path| s.current_text(p);
    let mut lines = hierarchy::LineCache::new(&read);
    let kids: Vec<HierarchyNode> = locs
        .iter()
        .take(hierarchy::MAX_NODES)
        .map(|(p, r)| hierarchy::node_from_location(p, r, 1, &mut lines))
        .collect();
    drop(lines);
    let Some(h) = current_hierarchy(s, generation) else { return };
    h.pending = false;
    let Some(root) = h.roots.first().copied() else { return };
    let mut ids = Vec::new();
    for k in kids {
        match h.push(k) {
            Some(i) => ids.push(i),
            None => break,
        }
    }
    if ids.is_empty() {
        h.message = Some("No implementations found".into());
    }
    if let Some(n) = h.nodes.get_mut(root) {
        n.loading = false;
        n.expanded = true;
        n.children = Some(ids);
    }
}

/// Switch callers/callees (or subtypes/supertypes) and reload from the roots.
pub(crate) fn set_direction(s: &mut Session, direction: Direction) -> Result<(), CommandError> {
    let h = s.refactor.hierarchy.as_mut().ok_or_else(|| CommandError::Failed("no hierarchy is shown".into()))?;
    let valid = match h.kind {
        HierarchyKind::Call => matches!(direction, Direction::Callers | Direction::Callees),
        HierarchyKind::Type | HierarchyKind::Method => matches!(direction, Direction::Subtypes | Direction::Supertypes),
    };
    if !valid {
        return Err(CommandError::BadParams(format!("{} does not apply to this hierarchy", direction.label())));
    }
    if h.direction == direction {
        return Ok(());
    }
    if !h.server_types && h.kind != HierarchyKind::Call {
        return Err(CommandError::Failed(
            "the language server does not provide type hierarchies; only implementations are shown".into(),
        ));
    }
    h.direction = direction;
    h.reset_children();
    if let Some(first) = h.roots.first().copied() {
        toggle_node(s, first)?;
    }
    Ok(())
}

/// Open a hierarchy node's location.
pub(crate) fn open_node(s: &mut Session, node: usize) -> Result<(), CommandError> {
    let n = s
        .refactor
        .hierarchy
        .as_ref()
        .and_then(|h| h.nodes.get(node))
        .ok_or_else(|| CommandError::BadParams(format!("no node {node}")))?;
    let (path, line, column) = (n.path.clone(), n.line, n.column);
    if let Some(h) = &mut s.refactor.hierarchy {
        h.selected = Some(node);
    }
    s.navigate_to_position(&path, line, column)
}

// ---------------------------------------------------------------- super method

/// Navigate › Super Method (Cmd+U): the trait item an impl item implements (via declaration),
/// else the parent module's `mod` declaration (like intellij-rust's Go to Super).
pub(crate) fn super_method(s: &mut Session, p: &Value) -> Result<(), CommandError> {
    let (_, path, _, pos) = position(s, p)?;
    let pending = RefactorPending::SuperMethod { path: path.clone(), line: pos.line };
    s.lsp_send(|c| c.declaration(&path, pos.line, pos.character), crate::lsp::Pending::Refactor(pending))
}

/// Navigate › Related Symbol: the declaration of the module the caret is in.
pub(crate) fn parent_module(s: &mut Session, p: &Value) -> Result<(), CommandError> {
    let (_, path, _, pos) = position(s, p)?;
    s.lsp_send(
        |c| c.parent_module(&path, pos.line, pos.character),
        crate::lsp::Pending::Refactor(RefactorPending::ParentModule),
    )
}

fn on_super_method(s: &mut Session, origin: &Path, line: u32, result: Result<Value, ResponseError>) {
    let locs = result.map(hierarchy::locations).unwrap_or_default();
    // Declaration of an inherent item is the item itself: not a super method.
    let elsewhere = locs.iter().find(|(p, r)| p != origin || r.start.line != line);
    if let Some((p, r)) = elsewhere {
        let (p, r) = (p.clone(), *r);
        return navigate_lsp(s, &p, &r);
    }
    let Some(pos) = s.ws.active().map(|(e, d)| convert::lsp_pos(&d.doc, e.selections.primary().head)) else { return };
    let origin = origin.to_path_buf();
    if let Err(e) = s.lsp_send(
        |c| c.parent_module(&origin, pos.line, pos.character),
        crate::lsp::Pending::Refactor(RefactorPending::ParentModule),
    ) {
        s.status = e.to_string();
    }
}

// ---------------------------------------------------------------- file operations

/// The file a file refactoring applies to: `path`, else the Project view selection, else the
/// active editor's file.
pub(crate) fn target_path(s: &Session, p: &Value) -> Result<PathBuf, CommandError> {
    let path = if let Some(path) = crate::params::opt_path(p, "path") {
        let path = crate::normalize_path(&path);
        if !path.exists() {
            return Err(CommandError::Failed(format!("{} does not exist", path.display())));
        }
        path
    } else if p.get("path").is_some() {
        return Err(CommandError::BadParams("`path` must be a non-empty string".into()));
    } else {
        let selected = s.ws.project.as_ref().and_then(|pr| pr.selected.clone()).filter(|p| p.exists());
        let active = s.ws.active().and_then(|(_, d)| d.path.clone()).filter(|p| p.exists());
        selected.or(active).ok_or_else(|| CommandError::Disabled("select a file first".into()))?
    };
    if s.ws.project.as_ref().is_some_and(|pr| pr.root.starts_with(&path)) || path.parent().is_none() {
        return Err(CommandError::Failed("the project root cannot be moved, copied or deleted here".into()));
    }
    Ok(path)
}

fn rust_related(path: &Path) -> bool {
    Language::from_path(path) == Language::Rust || path.is_dir()
}

/// Whether rust-analyzer is ready and wants `workspace/willRenameFiles`.
fn will_rename_supported(s: &Session) -> bool {
    s.server_capabilities(ServerKind::RustAnalyzer)
        .and_then(|c| c.workspace)
        .and_then(|w| w.file_operations)
        .and_then(|f| f.will_rename)
        .is_some()
}

/// Move or rename `from` to `dest` (a full path), asking rust-analyzer for the reference
/// updates first when it can. Returns whether the move already happened.
pub(crate) fn move_path(s: &mut Session, from: &Path, dest: &Path, title: &str) -> Result<bool, CommandError> {
    if dest.exists() && !file_ops::same_file(from, dest) {
        return Err(CommandError::Failed(format!("{} already exists", dest.display())));
    }
    let renames = vec![(from.to_path_buf(), dest.to_path_buf())];
    if rust_related(from) && will_rename_supported(s) {
        // Unsaved edits first, so the server computes edits against what is on disk too.
        s.save_docs_under(from).map_err(CommandError::Failed)?;
        let r = renames.clone();
        s.lsp_send_to(
            ServerKind::RustAnalyzer,
            |c| c.will_rename_files(&r),
            crate::lsp::Pending::Refactor(RefactorPending::WillRename { renames, title: title.to_string() }),
        )?;
        s.status = format!("{title}…");
        return Ok(false);
    }
    finish_move(s, title, &renames, None)?;
    Ok(true)
}

/// The rename plus the server's edits: preview when there are edits, else apply.
fn finish_move(
    s: &mut Session,
    title: &str,
    renames: &[(PathBuf, PathBuf)],
    edit: Option<&lt::WorkspaceEdit>,
) -> Result<(), CommandError> {
    let mut ops = match edit {
        Some(e) => preview::edit_ops(e).map_err(CommandError::Failed)?,
        None => Vec::new(),
    };
    let has_edits = ops.iter().any(|o| matches!(o, EntryKind::Edit { .. }));
    for (from, to) in renames {
        // The server may already include the file operation itself.
        let included =
            ops.iter().any(|o| matches!(o, EntryKind::Rename { from: f, to: t, .. } if f == from && t == to));
        if !included {
            ops.push(EntryKind::Rename {
                from: from.clone(),
                to: to.clone(),
                overwrite: false,
                ignore_if_exists: false,
            });
        }
    }
    let mut preview = preview::from_ops(title, ops, &|p| s.current_text(p)).map_err(CommandError::Failed)?;
    preview.notify_renames = true;
    if has_edits {
        open_preview(s, preview);
        return Ok(());
    }
    let report = s.apply_preview(&preview);
    finish_report(s, title, report)?;
    if let Some((_, to)) = renames.first()
        && let Some(p) = &mut s.ws.project
    {
        p.reveal(to);
    }
    s.status = format!("{title}: done");
    Ok(())
}

impl Session {
    /// Tell the server about renamed files (when it asked for `didRenameFiles`).
    pub(crate) fn notify_did_rename(&mut self, renames: &[(PathBuf, PathBuf)]) {
        let wants = self
            .server_capabilities(ServerKind::RustAnalyzer)
            .and_then(|c| c.workspace)
            .and_then(|w| w.file_operations)
            .and_then(|f| f.did_rename)
            .is_some();
        if wants
            && let Some(c) = self.ready_client(ServerKind::RustAnalyzer)
            && let Err(e) = c.did_rename_files(renames)
        {
            log::warn!("didRenameFiles: {e}");
        }
    }
}

/// Copy File: copy `from` to `to` (a directory or full path), saving open edits first.
pub(crate) fn copy(s: &mut Session, from: &Path, to: &Path) -> Result<PathBuf, CommandError> {
    let dest = file_ops::destination(from, to).map_err(CommandError::Failed)?;
    if dest.exists() {
        return Err(CommandError::Failed(format!("{} already exists", dest.display())));
    }
    s.save_docs_under(from).map_err(CommandError::Failed)?;
    file_ops::copy_path(from, &dest).map_err(CommandError::Failed)?;
    s.jobs_rescan();
    if let Some(p) = &mut s.ws.project {
        p.reveal(&dest);
    }
    s.status = format!("Copied to {}", dest.display());
    Ok(dest)
}

/// The project's files (for usage searches), capped.
fn project_files(s: &Session, near: &Path) -> Vec<PathBuf> {
    match &s.ws.project {
        Some(p) => p.tree.files().map(|n| n.path.clone()).take(50_000).collect(),
        None => near
            .parent()
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).take(5000).collect())
            .unwrap_or_default(),
    }
}

/// Open the Safe Delete dialog and search for usages on a worker thread.
pub(crate) fn start_safe_delete(s: &mut Session, path: &Path) {
    let files = project_files(s, path);
    // Open documents may be ahead of the disk.
    let open: std::collections::HashMap<PathBuf, String> =
        s.ws.docs().filter_map(|d| Some((d.path.clone()?, d.doc.text()))).collect();
    let (tx, rx) = crossbeam_channel::bounded(1);
    let target = path.to_path_buf();
    std::thread::spawn(move || {
        let read = |p: &Path| open.get(p).cloned().or_else(|| codecraft_fs::read_text_file(p).ok());
        let usages = file_ops::find_usages(&target, &files, &read);
        let _ = tx.send((target, usages));
    });
    s.refactor.usages = Some(rx);
    s.refactor.dialog = Some(RefactorDialog::SafeDelete { path: path.to_path_buf(), usages: None });
}

/// Delete `path`, removing its `mod` declarations. Usages come from the dialog's search, or are
/// searched now (headless `force`).
pub(crate) fn finish_safe_delete(s: &mut Session, path: &Path) -> Result<ApplyReport, CommandError> {
    let usages = match &s.refactor.dialog {
        Some(RefactorDialog::SafeDelete { path: p, usages: Some(u) }) if p == path => u.clone(),
        _ => {
            let files = project_files(s, path);
            file_ops::find_usages(path, &files, &|p| s.current_text(p))
        }
    };
    let ops = file_ops::safe_delete_ops(path, &usages, path.is_dir());
    let title = format!("Delete {}", path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    let preview = preview::from_ops(&title, ops, &|p| s.current_text(p)).map_err(CommandError::Failed)?;
    s.refactor.dialog = None;
    s.refactor.usages = None;
    let report = s.apply_preview(&preview);
    if report.errors.is_empty() {
        s.status = format!("Deleted {}", path.display());
    }
    s.refactor.last_report = Some(report.clone());
    if !report.errors.is_empty() {
        s.notify(NotificationLevel::Error, &title, report.errors.join("\n"));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn action(title: &str, kind: &str, id: &str) -> Value {
        json!({ "title": title, "kind": kind, "data": { "id": format!("{id}:Kind:0"), "codeActionParams": {} } })
    }

    #[test]
    fn filters_pick_rust_analyzer_assists() {
        let list = json!([
            action("Extract into variable", "refactor.extract", "extract_variable"),
            action("Extract into constant", "refactor.extract", "extract_constant"),
            action("Extract into function", "refactor.extract", "extract_function"),
            action("Inline variable", "refactor.inline", "inline_local_variable"),
            action("Generate `new`", "", "generate_new"),
            action("Implement missing members", "quickfix", "add_missing_impl_members"),
            action("Implement default members", "quickfix", "add_missing_default_members"),
            action("Remove unused parameter", "refactor.rewrite", "remove_unused_param"),
            action("Flip comma", "refactor.rewrite", "flip_comma"),
            { "title": "no kind or data" },
            "garbage",
        ]);
        let titles = |f: ActionFilter| filter_actions(f, &list).into_iter().map(|a| a.title).collect::<Vec<_>>();
        assert_eq!(titles(ActionFilter::ExtractVariable), ["Extract into variable"]);
        assert_eq!(titles(ActionFilter::ExtractConstant), ["Extract into constant"]);
        assert_eq!(titles(ActionFilter::ExtractFunction), ["Extract into function"]);
        assert_eq!(titles(ActionFilter::Inline), ["Inline variable"]);
        assert_eq!(titles(ActionFilter::ImplementMembers), ["Implement missing members"]);
        assert_eq!(titles(ActionFilter::OverrideMembers), ["Implement default members"]);
        assert_eq!(titles(ActionFilter::ChangeSignature), ["Remove unused parameter"]);
        assert_eq!(
            titles(ActionFilter::Generate),
            ["Generate `new`", "Implement missing members", "Implement default members"]
        );
        assert_eq!(titles(ActionFilter::RefactorThis).len(), 6);
        assert!(filter_actions(ActionFilter::Generate, &json!({ "not": "a list" })).is_empty());
        assert_eq!(assist_id(&list[0]), Some("extract_variable"));
        assert_eq!(assist_id(&json!({ "data": 5 })), None);
    }

    #[test]
    fn titles_match_without_data() {
        let a = json!({ "title": "Extract into variable", "kind": "refactor.extract" });
        assert!(ActionFilter::ExtractVariable.matches(&a));
        assert!(!ActionFilter::ExtractFunction.matches(&a));
        assert!(ActionFilter::ImplementMembers.matches(&json!({ "title": "Implement missing members" })));
    }

    #[test]
    fn command_items_are_marked() {
        let i = command_item("Rename…", "refactor.rename");
        assert_eq!(i.payload["codecraftCommand"], "refactor.rename");
    }
}
