//! Menu sweep commands (M10 track F): File Properties (encoding, BOM, read-only, file type),
//! Power Save Mode, Export to HTML / Print, New › Python Package, New Projects Setup, Fill
//! Paragraph, Database Changes, Recently Changed Files / Recent Changes, Compare With…, Compare
//! with Clipboard, Context Info, Stop Background Processes / Show Running List / Processes,
//! preview tabs (Keep Tab Open), Show Hidden Tabs, Configure Editor Tabs and GitHub/GitLab
//! Manage Accounts. State and helpers are in `misc.rs`.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use codecraft_text::{ChangeSet, Edit, EditGroup, SelectionSet};
use codecraft_workspace::{EditorId, TextEncoding};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_project, has_tabs, ok, simple, spec};
use crate::misc::{self, AssociateDialog, CompareView, ProcessesMode, encoding};
use crate::params::{bool_or, opt_path, opt_str, req_str};
use crate::popup::{LocationItem, NewItemKind};
use crate::{CommandError, Effect, Popup, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------------------ File
        spec(
            "file.fileProperties.fileEncoding",
            "File Encoding",
            None,
            "{encoding?: \"UTF-8\"|\"UTF-16LE\"|\"UTF-16BE\"|\"ISO-8859-1\", mode?: \"reload\"|\"convert\"} — without `encoding` shows the chooser; reload re-reads the file in that encoding, convert saves it in that encoding",
            Context::Global,
            has_editor,
            file_encoding,
        ),
        simple("fileEncoding.close", "Close File Encoding Chooser", always, |s, _| {
            s.misc.encoding_dialog = false;
            ok()
        }),
        simple("file.fileProperties.removeBom", "Remove BOM", has_bom, |s, _| set_bom(s, false)),
        simple("file.fileProperties.addBom", "Add BOM", can_add_bom, |s, _| set_bom(s, true)),
        simple("file.fileProperties.makeFileReadOnly", "Make file read-only", has_file, toggle_read_only),
        spec(
            "file.fileProperties.associateWithFileType",
            "Associate with File Type…",
            None,
            "{pattern?: string, language?: string|null} — file name glob (`*.inc`) → language (`Rust`, `Python`, `Plain text`, …); null removes the association; without `language` shows the dialog",
            Context::Global,
            always,
            associate,
        ),
        simple("associateFileType.close", "Close Associate with File Type", always, |s, _| {
            s.misc.associate = None;
            ok()
        }),
        simple("file.powerSaveMode", "Power Save Mode", always, |s, _| {
            s.ws.settings.power_save_mode = !s.ws.settings.power_save_mode;
            let on = s.ws.settings.power_save_mode;
            if on && matches!(s.popup, Some(Popup::Completion(_))) {
                s.popup = None;
            }
            s.status = if on {
                "Power Save Mode is on: code insight and background tasks are limited".into()
            } else {
                "Power Save Mode is off".into()
            };
            Ok(json!({ "on": on }))
        }),
        spec(
            "file.export.filesOrSelectionToHtml",
            "Files or Selection to HTML…",
            None,
            "{output?: path, selection?: bool, lineNumbers?: bool, open?: bool} — the active file (or the selected lines) as highlighted HTML; default output is a temporary file, opened in the browser",
            Context::Global,
            has_editor,
            |s, p| export_html(s, p, false),
        ),
        spec(
            "file.print",
            "Print…",
            None,
            "{selection?: bool} — opens a printable HTML page of the active file in the browser",
            Context::Global,
            has_editor,
            |s, p| export_html(s, p, true),
        ),
        spec(
            "file.new.pythonPackage",
            "Python Package",
            None,
            "{path?: string} — a directory with an empty __init__.py; without `path` asks for a name",
            Context::Global,
            has_project,
            python_package,
        ),
        simple("file.newProjectsSetup.settingsForNewProjects", "Settings for New Projects…", always, |s, _| {
            s.execute("app.settings", &Value::Null)
        }),
        simple("file.newProjectsSetup.runConfigurationTemplates", "Run Configuration Templates…", has_project, |s, _| {
            s.execute("run.editConfigurations", &Value::Null)
        }),
        // ------------------------------------------------------------ Edit
        spec("edit.fillParagraph", "Fill Paragraph", None, "", Context::Editor, has_editor, fill_paragraph),
        // ------------------------------------------------------------ View
        simple("databaseChanges.list", "List Database Changes", always, |s, _| {
            Ok(json!({ "tables": misc::database_changes(s) }))
        }),
        simple("databaseChanges.submitAll", "Submit All Database Changes", always, |s, _| {
            for_each_change(s, "db.submitChanges")
        }),
        simple("databaseChanges.revertAll", "Revert All Database Changes", always, |s, _| {
            for_each_change(s, "db.revertChanges")
        }),
        simple("view.recentlyChangedFiles", "Recently Changed Files", has_project, recently_changed_files),
        simple("view.recentChanges", "Recent Changes", has_project, |s, _| {
            crate::local_history::show(s, crate::local_history::ViewMode::Recent).map_err(CommandError::Failed)?;
            Ok(json!({ "revisions": s.local_history.view.as_ref().map(|v| v.revisions.len()).unwrap_or(0) }))
        }),
        spec(
            "view.compareWith",
            "Compare With…",
            None,
            "{path?: string} — diff the active file with another file; without `path` shows a file chooser",
            Context::Global,
            has_editor,
            compare_with,
        ),
        spec(
            "view.compareWithClipboard",
            "Compare with Clipboard",
            None,
            "{text?: string} — diff the clipboard with the selection (or the whole file)",
            Context::Global,
            has_editor,
            compare_with_clipboard,
        ),
        simple("compare.close", "Close Diff", always, |s, _| {
            s.misc.compare = None;
            s.misc.compare_picker = None;
            s.misc.clipboard_request = false;
            ok()
        }),
        spec("view.contextInfo", "Context Info", None, "", Context::Editor, has_editor, context_info),
        // ------------------------------------------------------------ Run / Window › Processes
        simple("run.stopBackgroundProcesses", "Stop Background Processes…", always, |s, _| {
            open_processes(s, ProcessesMode::Stop)
        }),
        simple("run.showRunningList", "Show Running List", always, |s, _| open_processes(s, ProcessesMode::Running)),
        simple("window.processes.show", "Show", always, |s, _| open_processes(s, ProcessesMode::Tasks)),
        simple("window.processes.autoShow", "Auto Show", always, |s, _| {
            s.ws.settings.processes_auto_show = !s.ws.settings.processes_auto_show;
            Ok(json!({ "on": s.ws.settings.processes_auto_show }))
        }),
        simple("processes.list", "List Background Processes", always, |s, _| {
            Ok(json!({ "processes": misc::processes(s), "tasks": s.ws.tasks }))
        }),
        spec(
            "processes.stop",
            "Stop Background Process",
            None,
            "{kind: \"run\"|\"debug\"|\"languageServer\"|\"agent\"|\"database\"|\"git\", id: string}",
            Context::Global,
            always,
            |s, p| {
                let kind = req_str(p, "kind")?;
                let id = match p.get("id") {
                    Some(Value::Number(n)) => n.to_string(),
                    _ => req_str(p, "id")?.to_string(),
                };
                if !misc::stop_process(s, kind, &id) {
                    return Err(CommandError::Failed(format!("no running {kind} process {id}")));
                }
                Ok(json!({ "processes": misc::processes(s) }))
            },
        ),
        simple("processes.stopAll", "Stop All Background Processes", always, |s, _| {
            let all = misc::processes(s);
            let mut stopped = 0usize;
            for p in &all {
                stopped += usize::from(misc::stop_process(s, &p.kind, &p.id));
            }
            Ok(json!({ "stopped": stopped }))
        }),
        simple("processes.close", "Close Processes", always, |s, _| {
            s.misc.processes = None;
            ok()
        }),
        // ------------------------------------------------------------ Window › Editor Tabs
        spec(
            "projectView.openPreview",
            "Open in Preview Tab",
            None,
            "{path: string} — a single click in the Project view: replaces the preview tab until it is edited or kept",
            Context::Global,
            always,
            |s, p| {
                let path = crate::params::req_path(p, "path")?;
                if !path.is_file() {
                    return Err(CommandError::Failed(format!("{} is not a file", path.display())));
                }
                let id = misc::open_preview(s, &path)?;
                let preview = s.ws.editor(id).is_some_and(|e| e.preview);
                Ok(json!({ "editor": id.0, "preview": preview }))
            },
        ),
        spec(
            "window.editorTabs.keepTabOpen",
            "Keep Tab Open",
            None,
            "{editor?: int} — make a preview tab permanent",
            Context::Global,
            has_tabs,
            |s, p| {
                let id = match crate::params::opt_usize(p, "editor")? {
                    Some(n) => EditorId(n as u64),
                    None => s.ws.active_editor().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?,
                };
                Ok(json!({ "kept": misc::keep_open(s, id) }))
            },
        ),
        simple("window.editorTabs.showHiddenTabs", "Show Hidden Tabs", has_tabs, |s, _| {
            s.misc.hidden_tabs = true;
            Ok(json!({ "tabs": tab_list(s) }))
        }),
        simple("hiddenTabs.close", "Close Hidden Tabs", always, |s, _| {
            s.misc.hidden_tabs = false;
            ok()
        }),
        simple("window.editorTabs.configureEditorTabs", "Configure Editor Tabs…", always, |s, _| {
            s.execute("app.settings", &Value::Null)?;
            s.execute("settings.dialog.page", &json!({ "page": crate::settings::PAGE_EDITOR_TABS }))
        }),
        // ------------------------------------------------------------ Git › GitHub / GitLab
        simple("git.gitHub.manageAccounts", "Manage Accounts…", always, |s, _| {
            misc::start_accounts(
                s,
                "gh",
                "GitHub Accounts",
                "To add an account, run `gh auth login` in the Terminal tool window (GitHub CLI: https://cli.github.com).",
            );
            ok()
        }),
        simple("git.gitLab.manageAccounts", "Manage Accounts…", always, |s, _| {
            misc::start_accounts(
                s,
                "glab",
                "GitLab Accounts",
                "To add an account, run `glab auth login` in the Terminal tool window (GitLab CLI: https://gitlab.com/gitlab-org/cli).",
            );
            ok()
        }),
        simple("accounts.close", "Close Accounts", always, |s, _| {
            s.misc.accounts = None;
            ok()
        }),
    ]
}

fn failed(e: impl std::fmt::Display) -> CommandError {
    CommandError::Failed(e.to_string())
}

// ---------------------------------------------------------------- enabled predicates

fn has_file(s: &Session) -> Result<(), String> {
    let (_, d) = s.ws.active().ok_or("no editor is open")?;
    d.path.as_ref().map(|_| ()).ok_or_else(|| "the file is not saved yet".into())
}

fn has_bom(s: &Session) -> Result<(), String> {
    has_file(s)?;
    if s.ws.active().is_some_and(|(_, d)| d.bom) { Ok(()) } else { Err("the file has no BOM".into()) }
}

fn can_add_bom(s: &Session) -> Result<(), String> {
    has_file(s)?;
    match s.ws.active() {
        Some((_, d)) if d.bom => Err("the file already has a BOM".into()),
        Some((_, d)) if d.encoding == TextEncoding::Latin1 => Err("ISO-8859-1 has no BOM".into()),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------- file properties

fn active_doc(s: &Session) -> Result<(codecraft_workspace::DocId, Option<PathBuf>), CommandError> {
    let (_, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    Ok((d.id, d.path.clone()))
}

fn file_encoding(s: &mut Session, p: &Value) -> CmdResult {
    let (doc, path) = active_doc(s)?;
    let Some(name) = opt_str(p, "encoding") else {
        s.misc.encoding_dialog = true;
        let d = s.ws.doc(doc);
        return Ok(json!({
            "encoding": d.map(|d| d.encoding.label()),
            "bom": d.is_some_and(|d| d.bom),
            "choices": TextEncoding::ALL.iter().map(|e| e.label()).collect::<Vec<_>>(),
        }));
    };
    let enc = TextEncoding::from_name(name)
        .ok_or_else(|| CommandError::BadParams(format!("unknown encoding `{name}`; use UTF-8, UTF-16LE, UTF-16BE or ISO-8859-1")))?;
    let mode = opt_str(p, "mode").unwrap_or("convert");
    match (mode, path) {
        ("reload", Some(path)) => {
            if s.ws.doc(doc).is_some_and(|d| d.doc.is_modified()) {
                return Err(CommandError::Failed("save or revert the changes before reloading in another encoding".into()));
            }
            let bytes = encoding::read_bytes(&path).map_err(failed)?;
            let text = encoding::decode(&bytes, enc).map_err(failed)?;
            let bom = bytes.starts_with(encoding::bom_bytes(enc)) && enc != TextEncoding::Latin1;
            replace_text(s, doc, &text)?;
            if let Some(od) = s.ws.doc_mut(doc) {
                od.encoding = enc;
                od.bom = bom;
                od.doc.mark_saved();
            }
        }
        ("reload", None) => return Err(CommandError::Failed("the file is not saved yet".into())),
        ("convert", path) => {
            let od = s.ws.doc(doc).ok_or_else(|| failed("the document is gone"))?;
            let text = od.doc.to_disk_text();
            if !encoding::can_encode(&text, enc) {
                return Err(CommandError::Failed(format!("the file has characters that {} cannot represent", enc.label())));
            }
            let bom = od.bom && enc != TextEncoding::Latin1;
            if let Some(od) = s.ws.doc_mut(doc) {
                od.encoding = enc;
                od.bom = bom;
            }
            if let Some(path) = path {
                save_doc(s, doc, &path)?;
            }
        }
        (other, _) => return Err(CommandError::BadParams(format!("unknown mode `{other}`; use reload or convert"))),
    }
    s.misc.encoding_dialog = false;
    s.status = format!("File encoding: {}", enc.label());
    Ok(json!({ "encoding": enc.label() }))
}

/// Replace a document's whole text (undoable, carets mapped through the change).
fn replace_text(s: &mut Session, doc: codecraft_workspace::DocId, text: &str) -> Result<(), CommandError> {
    let Some(editor) = s.ws.editor_for_doc(doc) else { return Ok(()) };
    s.ws.set_active_editor(editor);
    let was_read_only = s.ws.doc(doc).is_some_and(|d| d.read_only);
    if let Some(od) = s.ws.doc_mut(doc) {
        od.read_only = false;
    }
    let r = s.edit_active(EditGroup::Standalone, |d, sels, _| crate::reformat::diff_edit(d, text, sels));
    if let Some(od) = s.ws.doc_mut(doc) {
        od.read_only = was_read_only;
    }
    r.map(|_| ())
}

/// Write a document to disk in its encoding and mark it saved.
fn save_doc(s: &mut Session, doc: codecraft_workspace::DocId, path: &Path) -> Result<(), CommandError> {
    let od = s.ws.doc(doc).ok_or_else(|| failed("the document is gone"))?;
    encoding::write_document(path, od).map_err(failed)?;
    if let Some(od) = s.ws.doc_mut(doc) {
        od.doc.mark_saved();
        od.disk_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    }
    s.lsp_did_save(path);
    Ok(())
}

/// Add or remove the byte order mark of the file on disk (unsaved edits stay in the editor).
fn set_bom(s: &mut Session, bom: bool) -> CmdResult {
    let (doc, path) = active_doc(s)?;
    let path = path.ok_or_else(|| CommandError::Disabled("the file is not saved yet".into()))?;
    let enc = s.ws.doc(doc).map(|d| d.encoding).unwrap_or_default();
    let mark = encoding::bom_bytes(enc);
    if mark.is_empty() {
        return Err(CommandError::Failed(format!("{} has no byte order mark", enc.label())));
    }
    let bytes = encoding::read_bytes(&path).map_err(failed)?;
    let body = bytes.strip_prefix(mark).unwrap_or(&bytes);
    let mut out = Vec::with_capacity(body.len() + mark.len());
    if bom {
        out.extend_from_slice(mark);
    }
    out.extend_from_slice(body);
    codecraft_fs::write_file_atomic(&path, &out).map_err(failed)?;
    if let Some(od) = s.ws.doc_mut(doc) {
        od.bom = bom;
        od.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    }
    s.status = if bom { "Added BOM".into() } else { "Removed BOM".into() };
    Ok(json!({ "bom": bom }))
}

/// Make file read-only / writable: the file's write permission and the editor's read-only state.
fn toggle_read_only(s: &mut Session, _: &Value) -> CmdResult {
    let (doc, path) = active_doc(s)?;
    let path = path.ok_or_else(|| CommandError::Disabled("the file is not saved yet".into()))?;
    let read_only = !s.ws.doc(doc).is_some_and(|d| d.read_only);
    set_writable(&path, !read_only).map_err(|e| failed(format!("{}: {e}", path.display())))?;
    if let Some(od) = s.ws.doc_mut(doc) {
        od.read_only = read_only;
    }
    s.status = if read_only { "The file is read-only".into() } else { "The file is writable".into() };
    Ok(json!({ "readOnly": read_only }))
}

#[cfg(unix)]
fn set_writable(path: &Path, writable: bool) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path)?.permissions();
    let mode = perm.mode();
    // Read-only clears every write bit; writable gives the owner write access back.
    perm.set_mode(if writable { mode | 0o200 } else { mode & !0o222 });
    std::fs::set_permissions(path, perm)
}

#[cfg(not(unix))]
fn set_writable(path: &Path, writable: bool) -> std::io::Result<()> {
    let mut perm = std::fs::metadata(path)?.permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perm.set_readonly(!writable);
    std::fs::set_permissions(path, perm)
}

fn associate(s: &mut Session, p: &Value) -> CmdResult {
    let active_path = s.ws.active().and_then(|(_, d)| d.path.clone());
    let pattern = opt_str(p, "pattern")
        .map(|p| p.trim().to_string())
        .or_else(|| active_path.as_deref().map(misc::default_pattern))
        .unwrap_or_default();
    let Some(lang) = p.get("language") else {
        let language = active_path
            .as_deref()
            .map(|path| misc::language_for(&s.ws.settings, path).name())
            .unwrap_or("Plain text")
            .to_string();
        s.misc.associate = Some(AssociateDialog { pattern, language });
        return ok();
    };
    if pattern.is_empty() || pattern.chars().count() > 200 || pattern.contains(['/', '\\', ',']) {
        return Err(CommandError::BadParams("`pattern` must be a file name glob such as *.inc".into()));
    }
    match lang {
        Value::Null => {
            s.ws.settings.file_type_associations.remove(&pattern);
        }
        Value::String(name) if name.trim().is_empty() => {
            s.ws.settings.file_type_associations.remove(&pattern);
        }
        Value::String(name) => {
            let l = misc::language_by_name(name)
                .ok_or_else(|| CommandError::BadParams(format!("unknown language `{name}`")))?;
            s.ws.settings.file_type_associations.insert(pattern.clone(), l.name().to_string());
        }
        _ => return Err(CommandError::BadParams("`language` must be a language name or null".into())),
    }
    misc::reapply_languages(s);
    s.misc.associate = None;
    Ok(json!({ "associations": s.ws.settings.file_type_associations }))
}

// ---------------------------------------------------------------- export / print / new

fn export_html(s: &mut Session, p: &Value, print: bool) -> CmdResult {
    let (eid, doc) = s.active_ids().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let selection = bool_or(p, "selection", false);
    let line_numbers = bool_or(p, "lineNumbers", true);
    let sel = s.ws.editor(eid).map(|e| e.selections.primary());
    let palette = s.misc.palette.clone();
    let od = s.ws.doc_mut(doc).ok_or_else(|| failed("the document is gone"))?;
    let lines = match sel.filter(|r| selection && r.start() != r.end()) {
        Some(r) => od.doc.char_to_line(r.start())..od.doc.char_to_line(r.end()).saturating_add(1),
        None => 0..od.doc.len_lines(),
    };
    let title = od.title();
    let text = od.doc.text();
    let html = misc::export::to_html(
        &misc::export::Source { title: &title, text: &text, highlights: od.highlights(), lines, line_numbers },
        &palette,
    );
    let output = match opt_path(p, "output") {
        Some(o) => crate::normalize_path(&o),
        None => {
            let dir = std::env::temp_dir().join("codecraft-export");
            std::fs::create_dir_all(&dir).map_err(failed)?;
            dir.join(format!("{title}.html"))
        }
    };
    codecraft_fs::write_text_file_atomic(&output, &html).map_err(failed)?;
    if print || bool_or(p, "open", opt_path(p, "output").is_none()) {
        s.effects.push(Effect::OpenUrl(format!("file://{}", output.display())));
    }
    s.status = if print {
        "Opened a printable page in the browser".into()
    } else {
        format!("Exported to {}", output.display())
    };
    Ok(json!({ "output": output }))
}

fn python_package(s: &mut Session, p: &Value) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        let dir = s
            .ws
            .project
            .as_ref()
            .and_then(|pr| pr.selected.clone())
            .map(|sel| if sel.is_dir() { sel } else { sel.parent().map(Path::to_path_buf).unwrap_or(sel) })
            .or_else(|| s.project_root())
            .ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
        s.popup = Some(Popup::NewItem { kind: NewItemKind::PythonPackage, dir, name: String::new() });
        return ok();
    };
    let path = crate::normalize_path(&path);
    create_python_package(&path)?;
    s.jobs_rescan();
    if let Some(proj) = &mut s.ws.project {
        proj.reveal(&path.join("__init__.py"));
    }
    Ok(json!({ "path": path }))
}

/// `dir/__init__.py` (and `dir`). An existing package is an error; an existing plain directory
/// becomes a package.
pub(crate) fn create_python_package(dir: &Path) -> Result<(), CommandError> {
    let init = dir.join("__init__.py");
    if init.exists() {
        return Err(failed(format!("{} is already a package", dir.display())));
    }
    if dir.exists() && !dir.is_dir() {
        return Err(failed(format!("{} already exists", dir.display())));
    }
    std::fs::create_dir_all(dir).map_err(failed)?;
    std::fs::write(&init, "").map_err(failed)?;
    Ok(())
}

// ---------------------------------------------------------------- edit

fn fill_paragraph(s: &mut Session, _: &Value) -> CmdResult {
    let (eid, doc) = s.active_ids().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let margin = s.ws.settings.right_margin.clamp(10, 1000);
    let cfg = s.ops_config(doc);
    let caret = s.ws.editor(eid).map(|e| e.selections.primary().head).unwrap_or(0);
    let Some(od) = s.ws.doc(doc) else { return ok() };
    let line = od.doc.char_to_line(caret);
    let block = cfg.block_comment.is_some();
    let Some((range, new)) = misc::fill::fill_paragraph(&od.doc, line, margin, cfg.line_comment.as_deref(), block)
    else {
        s.status = "Nothing to fill".into();
        return Ok(json!({ "changed": false }));
    };
    s.edit_active(EditGroup::Standalone, |_, _, _| {
        let end = range.start + new.chars().count();
        Edit { changes: ChangeSet::single(range.start, range.end, new), selections: SelectionSet::caret(end) }
    })?;
    Ok(json!({ "changed": true }))
}

// ---------------------------------------------------------------- database changes

fn for_each_change(s: &mut Session, cmd: &str) -> CmdResult {
    let ids: Vec<u64> = s.db.results.iter().filter(|r| !r.edits.is_empty() && !r.edits.submitting).map(|r| r.id).collect();
    let mut errors = Vec::new();
    for id in &ids {
        if let Err(e) = s.execute(cmd, &json!({ "result": id })) {
            errors.push(e.to_string());
        }
    }
    if errors.is_empty() { Ok(json!({ "tables": ids.len() })) } else { Err(failed(errors.join("\n"))) }
}

// ---------------------------------------------------------------- navigation

/// Files with recent changes: modified open documents, then files by their latest local
/// history snapshot.
fn recently_changed_files(s: &mut Session, _: &Value) -> CmdResult {
    crate::local_history::ensure_loaded(s);
    let mut paths: Vec<(PathBuf, String)> = s
        .ws
        .docs()
        .filter(|d| d.doc.is_modified())
        .filter_map(|d| d.path.clone().map(|p| (p, "modified".to_string())))
        .collect();
    if let Some(store) = s.local_history.store() {
        let now = crate::local_history::now_ms();
        let mut entries: Vec<_> = store.entries().iter().filter(|e| e.path.is_some()).collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.time_ms));
        for e in entries {
            let Some(abs) = store.abs(e) else { continue };
            if paths.iter().any(|(p, _)| *p == abs) || !abs.is_file() {
                continue;
            }
            paths.push((abs, format!("{} · {}", crate::local_history::when(e.time_ms, now), e.kind.label())));
            if paths.len() >= 50 {
                break;
            }
        }
    }
    let root = s.project_root();
    let items: Vec<LocationItem> = paths
        .into_iter()
        .map(|(path, what)| {
            let rel = root.as_deref().and_then(|r| path.strip_prefix(r).ok()).unwrap_or(&path).display().to_string();
            LocationItem { preview: format!("{rel}  ({what})"), path, line: 0, column: 0 }
        })
        .collect();
    if items.is_empty() {
        s.status = "No recently changed files".into();
        return Ok(json!({ "files": [] }));
    }
    let files: Vec<&PathBuf> = items.iter().map(|i| &i.path).collect();
    let out = json!({ "files": files });
    s.popup = Some(Popup::Locations { title: "Recently Changed Files".into(), items, selected: 0 });
    Ok(out)
}

fn active_text_and_title(s: &Session) -> Result<(String, String), CommandError> {
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let sel = e.selections.primary();
    if sel.start() != sel.end() {
        return Ok((d.doc.slice(sel.start()..sel.end()), format!("{} (selection)", d.title())));
    }
    Ok((d.doc.text(), d.title()))
}

fn compare_with(s: &mut Session, p: &Value) -> CmdResult {
    let Some(other) = opt_path(p, "path") else {
        s.misc.compare_picker = Some(String::new());
        return ok();
    };
    let other = crate::normalize_path(&other);
    let (_, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let (left, left_title) = (d.doc.text(), d.path.as_deref().map(|p| p.display().to_string()).unwrap_or(d.title()));
    let right = match s.ws.doc_by_path(&other).and_then(|id| s.ws.doc(id)) {
        Some(od) => od.doc.text(),
        None => encoding::read_document(&other).map(|(t, _, _)| t.replace("\r\n", "\n")).map_err(failed)?,
    };
    s.misc.compare_picker = None;
    s.misc.compare = Some(CompareView {
        title: format!(
            "{} vs {}",
            Path::new(&left_title).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            other.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
        ),
        left_title,
        right_title: other.display().to_string(),
        left,
        right,
    });
    Ok(json!({ "identical": s.misc.compare.as_ref().is_some_and(|c| c.left == c.right) }))
}

fn compare_with_clipboard(s: &mut Session, p: &Value) -> CmdResult {
    let clip = match opt_str(p, "text") {
        Some(t) => {
            s.misc.clipboard_request = false;
            t.to_string()
        }
        None => {
            // The shell may answer with the system clipboard (it re-runs this with `text`).
            s.misc.clipboard_request = true;
            s.clipboard.as_ref().map(|(t, _)| t.clone()).unwrap_or_default()
        }
    };
    let (text, title) = active_text_and_title(s)?;
    let identical = clip.replace("\r\n", "\n") == text;
    s.misc.compare = Some(CompareView {
        title: format!("Clipboard vs {title}"),
        left_title: "Clipboard".into(),
        right_title: title,
        left: clip.replace("\r\n", "\n"),
        right: text,
    });
    Ok(json!({ "identical": identical }))
}

/// Context Info (Ctrl+Shift+Q): the header lines of the items around the caret.
fn context_info(s: &mut Session, _: &Value) -> CmdResult {
    let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
    let caret = e.selections.primary().head;
    let text = d.doc.text();
    let extents = crate::structure::extents(d.language, &text);
    let crumbs = crate::structure::crumbs_at(&extents, caret);
    let caret_line = d.doc.char_to_line(caret);
    let headers: Vec<String> = crumbs
        .iter()
        .filter(|c| c.line != caret_line)
        .filter_map(|c| d.doc.line_text(c.line).map(|t| (c.line, t)))
        .map(|(line, t)| format!("{:>5}  {}", line + 1, t.trim_end()))
        .collect();
    if headers.is_empty() {
        s.status = "No context info at the caret".into();
        return Ok(json!({ "headers": [] }));
    }
    let lang = d.language.lsp_id();
    let popup = Popup::Hover { editor: e.id, offset: caret, text: format!("```{lang}\n{}\n```", headers.join("\n")) };
    s.popup = Some(popup);
    Ok(json!({ "headers": headers }))
}

// ---------------------------------------------------------------- processes and tabs

fn open_processes(s: &mut Session, mode: ProcessesMode) -> CmdResult {
    s.misc.processes = Some(mode);
    let mut list = misc::processes(s);
    if mode == ProcessesMode::Running {
        list.retain(|p| p.kind == "run" || p.kind == "debug");
        if list.is_empty() {
            s.status = "Nothing is running".into();
        }
    }
    Ok(json!({ "processes": list, "tasks": s.ws.tasks }))
}

fn tab_list(s: &Session) -> Vec<Value> {
    s.ws.tabs()
        .iter()
        .filter_map(|id| {
            let e = s.ws.editor(*id)?;
            let d = s.ws.doc(e.doc)?;
            Some(json!({
                "editor": id.0,
                "title": d.title(),
                "path": d.path,
                "preview": e.preview,
                "pinned": e.pinned,
                "modified": d.doc.is_modified(),
                "active": s.ws.active_editor() == Some(*id),
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use codecraft_syntax::Language;
    use codecraft_workspace::TextEncoding;

    use crate::misc::encoding::encode;
    use crate::{Effect, Popup, Session};

    fn project(files: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf, Session) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for (name, bytes) in files {
            let p = root.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.execute("file.open", &json!({ "path": root })).unwrap();
        (dir, root, s)
    }

    fn open(s: &mut Session, path: &Path) {
        s.execute("file.open", &json!({ "path": path })).unwrap();
    }

    fn active_text(s: &Session) -> String {
        s.ws.active().map(|(_, d)| d.doc.text()).unwrap_or_default()
    }

    fn pump_until(s: &mut Session, what: impl Fn(&Session) -> bool) {
        let start = Instant::now();
        while !what(s) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out");
            s.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn encodings_round_trip_through_open_save_convert_and_reload() {
        let text = "x = 'é'\ny = 1\n";
        let utf16 = encode(text, TextEncoding::Utf16Le, true).unwrap();
        let (_d, root, mut s) = project(&[("a.py", &utf16)]);
        let file = root.join("a.py");
        open(&mut s, &file);
        let (_, d) = s.ws.active().unwrap();
        assert_eq!((d.encoding, d.bom, d.doc.text().as_str()), (TextEncoding::Utf16Le, true, text));
        // Saving keeps the encoding and the BOM.
        s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
        s.execute("editor.type", &json!({ "text": "#" })).unwrap();
        s.execute("file.saveAll", &json!({})).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), encode(&format!("#{text}"), TextEncoding::Utf16Le, true).unwrap());
        // Convert to UTF-8 rewrites the file (the BOM stays a UTF-8 BOM).
        let v = s.execute("file.fileProperties.fileEncoding", &json!({ "encoding": "utf-8" })).unwrap();
        assert_eq!(v["encoding"], "UTF-8");
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(bytes, [&[0xEF, 0xBB, 0xBF][..], format!("#{text}").as_bytes()].concat());
        // Latin-1 cannot hold every char.
        s.execute("editor.type", &json!({ "text": "€" })).unwrap();
        assert!(s.execute("file.fileProperties.fileEncoding", &json!({ "encoding": "ISO-8859-1" })).is_err());
        s.execute("edit.undo", &json!({})).unwrap();
        // Reload as Latin-1 shows the UTF-8 bytes as Latin-1 chars (undoable), and back.
        s.execute("file.fileProperties.fileEncoding", &json!({ "encoding": "UTF-8", "mode": "convert" })).unwrap();
        s.execute("file.fileProperties.removeBom", &json!({})).unwrap();
        s.execute("file.fileProperties.fileEncoding", &json!({ "encoding": "latin1", "mode": "reload" })).unwrap();
        assert!(active_text(&s).contains("Ã©"), "{}", active_text(&s));
        assert_eq!(s.ws.active().unwrap().1.encoding, TextEncoding::Latin1);
        s.execute("file.fileProperties.fileEncoding", &json!({ "encoding": "UTF-8", "mode": "reload" })).unwrap();
        assert_eq!(active_text(&s), format!("#{text}"));
        // Without `encoding` the chooser opens; bad names are rejected.
        let v = s.execute("file.fileProperties.fileEncoding", &json!({})).unwrap();
        assert!(s.misc.encoding_dialog);
        assert_eq!(v["choices"].as_array().unwrap().len(), 4);
        assert!(s.execute("file.fileProperties.fileEncoding", &json!({ "encoding": "EBCDIC" })).is_err());
        assert!(s.execute("file.fileProperties.fileEncoding", &json!({ "encoding": "UTF-8", "mode": "x" })).is_err());
    }

    #[test]
    fn bom_add_and_remove_touch_only_the_mark() {
        let (_d, root, mut s) = project(&[("a.txt", b"hello\n")]);
        let file = root.join("a.txt");
        open(&mut s, &file);
        assert!(s.execute("file.fileProperties.removeBom", &json!({})).is_err(), "no BOM to remove");
        s.execute("file.fileProperties.addBom", &json!({})).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"\xEF\xBB\xBFhello\n");
        assert!(s.ws.active().unwrap().1.bom);
        assert!(s.execute("file.fileProperties.addBom", &json!({})).is_err(), "already has one");
        // The BOM survives a save and a reopen.
        s.execute("editor.type", &json!({ "text": "> " })).unwrap();
        s.execute("file.saveAll", &json!({})).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"\xEF\xBB\xBF> hello\n");
        s.execute("file.fileProperties.removeBom", &json!({})).unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"> hello\n");
        assert!(!s.ws.active().unwrap().1.bom);
    }

    #[cfg(unix)]
    #[test]
    fn make_read_only_toggles_permissions_and_editing() {
        use std::os::unix::fs::PermissionsExt;
        let (_d, root, mut s) = project(&[("a.txt", b"text\n")]);
        let file = root.join("a.txt");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        open(&mut s, &file);
        let v = s.execute("file.fileProperties.makeFileReadOnly", &json!({})).unwrap();
        assert_eq!(v["readOnly"], true);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o440);
        assert!(s.execute("editor.type", &json!({ "text": "x" })).is_err());
        let v = s.execute("file.fileProperties.makeFileReadOnly", &json!({})).unwrap();
        assert_eq!(v["readOnly"], false);
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o640);
        s.execute("editor.type", &json!({ "text": "x" })).unwrap();
        // A file opened read-only is reported as such.
        std::fs::set_permissions(root.join("a.txt"), std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::write(root.join("b.txt"), "b").unwrap();
        std::fs::set_permissions(root.join("b.txt"), std::fs::Permissions::from_mode(0o444)).unwrap();
        open(&mut s, &root.join("b.txt"));
        assert!(s.ws.active().unwrap().1.read_only);
        std::fs::set_permissions(root.join("a.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn tab_titles(s: &Session) -> Vec<(String, bool)> {
        s.ws.tabs()
            .iter()
            .map(|id| {
                let e = s.ws.editor(*id).unwrap();
                (s.ws.doc(e.doc).unwrap().title(), e.preview)
            })
            .collect()
    }

    #[test]
    fn preview_tabs_are_replaced_until_edited_or_kept() {
        let (_d, root, mut s) =
            project(&[("a.rs", b"fn a() {}\n"), ("b.rs", b"fn b() {}\n"), ("c.rs", b"fn c() {}\n"), ("d.rs", b"d")]);
        open(&mut s, &root.join("a.rs"));
        s.effects.clear();
        let v = s.execute("projectView.openPreview", &json!({ "path": root.join("b.rs") })).unwrap();
        assert_eq!(v["preview"], true);
        assert!(!s.effects.contains(&Effect::FocusEditor), "the Project view keeps the focus");
        s.execute("projectView.openPreview", &json!({ "path": root.join("c.rs") })).unwrap();
        assert_eq!(tab_titles(&s), vec![("a.rs".into(), false), ("c.rs".into(), true)]);
        // Selecting an open file does not touch its tab.
        s.execute("projectView.openPreview", &json!({ "path": root.join("a.rs") })).unwrap();
        s.execute("projectView.openPreview", &json!({ "path": root.join("c.rs") })).unwrap();
        assert_eq!(tab_titles(&s), vec![("a.rs".into(), false), ("c.rs".into(), true)]);
        // An edit makes it permanent; the next preview opens beside it.
        s.execute("editor.type", &json!({ "text": "// x\n" })).unwrap();
        s.pump();
        s.execute("projectView.openPreview", &json!({ "path": root.join("b.rs") })).unwrap();
        assert_eq!(tab_titles(&s), vec![("a.rs".into(), false), ("c.rs".into(), false), ("b.rs".into(), true)]);
        // A double click (file.open) keeps it; so does Keep Tab Open.
        open(&mut s, &root.join("b.rs"));
        assert_eq!(tab_titles(&s).last(), Some(&("b.rs".into(), false)));
        s.execute("projectView.openPreview", &json!({ "path": root.join("d.rs") })).unwrap();
        assert_eq!(s.execute("window.editorTabs.keepTabOpen", &json!({})).unwrap()["kept"], true);
        assert_eq!(s.execute("window.editorTabs.keepTabOpen", &json!({})).unwrap()["kept"], false);
        assert!(tab_titles(&s).iter().all(|(_, p)| !p));
        // With the preview tab off, a single click opens a normal tab.
        s.execute("window.editorTabs.closeAllTabs", &json!({})).unwrap();
        s.ws.settings.preview_tab = false;
        let v = s.execute("projectView.openPreview", &json!({ "path": root.join("a.rs") })).unwrap();
        assert_eq!(v["preview"], false);
        assert!(s.execute("projectView.openPreview", &json!({ "path": root.join("nope.rs") })).is_err());
        // Show Hidden Tabs lists the group's tabs.
        let v = s.execute("window.editorTabs.showHiddenTabs", &json!({})).unwrap();
        assert_eq!(v["tabs"].as_array().unwrap().len(), 1);
        assert!(s.misc.hidden_tabs);
    }

    #[test]
    fn tab_limit_closes_unmodified_tabs() {
        let (_d, root, mut s) = project(&[("a", b"a"), ("b", b"b"), ("c", b"c")]);
        s.ws.settings.editor_tab_limit = 2;
        open(&mut s, &root.join("a"));
        s.execute("editor.type", &json!({ "text": "x" })).unwrap();
        open(&mut s, &root.join("b"));
        open(&mut s, &root.join("c"));
        // `a` is modified, so `b` goes.
        assert_eq!(tab_titles(&s).into_iter().map(|t| t.0).collect::<Vec<_>>(), vec!["a", "c"]);
    }

    #[test]
    fn power_save_mode_toggles_and_gates_live_todo_scans() {
        let (_d, root, mut s) = project(&[("a.rs", b"fn a() {}\n")]);
        open(&mut s, &root.join("a.rs"));
        assert_eq!(s.execute("file.powerSaveMode", &json!({})).unwrap()["on"], true);
        assert!(s.power_save());
        s.execute("view.toolWindows.todo", &json!({})).unwrap();
        s.pump();
        pump_until(&mut s, |s| !s.todo.busy());
        s.execute("editor.type", &json!({ "text": "// TODO later\n" })).unwrap();
        for _ in 0..5 {
            s.pump();
        }
        assert_eq!(s.todo.count(), 0, "no live scan in Power Save Mode");
        assert_eq!(s.execute("file.powerSaveMode", &json!({})).unwrap()["on"], false);
        pump_until(&mut s, |s| s.todo.count() == 1);
    }

    #[test]
    fn associate_with_file_type_relabels_open_documents() {
        let (_d, root, mut s) = project(&[("a.inc", b"fn x() {}\n")]);
        open(&mut s, &root.join("a.inc"));
        assert_eq!(s.ws.active().unwrap().1.language, Language::PlainText);
        s.execute("file.fileProperties.associateWithFileType", &json!({})).unwrap();
        let dialog = s.misc.associate.clone().unwrap();
        assert_eq!((dialog.pattern.as_str(), dialog.language.as_str()), ("*.inc", "Plain text"));
        s.execute("file.fileProperties.associateWithFileType", &json!({ "language": "Rust" })).unwrap();
        assert_eq!(s.ws.active().unwrap().1.language, Language::Rust);
        assert!(s.misc.associate.is_none());
        assert_eq!(s.ws.settings.file_type_associations.get("*.inc").map(String::as_str), Some("Rust"));
        assert!(
            s.execute("file.fileProperties.associateWithFileType", &json!({ "pattern": "*.inc", "language": "Cobol" }))
                .is_err()
        );
        assert!(
            s.execute("file.fileProperties.associateWithFileType", &json!({ "pattern": "a/b", "language": "Rust" }))
                .is_err()
        );
        s.execute("file.fileProperties.associateWithFileType", &json!({ "pattern": "*.inc", "language": null })).unwrap();
        assert_eq!(s.ws.active().unwrap().1.language, Language::PlainText);
    }

    #[test]
    fn fill_paragraph_rewraps_the_comment_at_the_caret() {
        let (_d, root, mut s) = project(&[("a.rs", b"// one two three four five\n// six\nfn x() {}\n")]);
        open(&mut s, &root.join("a.rs"));
        s.ws.settings.right_margin = 12;
        s.execute("editor.setSelection", &json!({ "anchor": 4, "head": 4 })).unwrap();
        assert_eq!(s.execute("edit.fillParagraph", &json!({})).unwrap()["changed"], true);
        assert_eq!(active_text(&s), "// one two\n// three\n// four five\n// six\nfn x() {}\n");
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(active_text(&s), "// one two three four five\n// six\nfn x() {}\n");
        s.execute("editor.setSelection", &json!({ "anchor": 36, "head": 36 })).unwrap();
        assert_eq!(s.execute("edit.fillParagraph", &json!({})).unwrap()["changed"], false);
    }

    #[test]
    fn export_and_print_write_highlighted_html() {
        let (_d, root, mut s) = project(&[("a.rs", b"fn main() {}\n// <tag>\n")]);
        open(&mut s, &root.join("a.rs"));
        s.misc.palette.insert("keyword".into(), "#112233".into());
        let out = root.join("out").join("a.html");
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        s.effects.clear();
        let v = s.execute("file.export.filesOrSelectionToHtml", &json!({ "output": out })).unwrap();
        assert_eq!(v["output"], json!(out));
        let html = std::fs::read_to_string(&out).unwrap();
        assert!(html.contains("<span class=\"keyword\">fn</span>") && html.contains("&lt;tag&gt;"));
        assert!(html.contains(".keyword{color:#112233}"));
        assert!(s.effects.is_empty(), "an explicit output is not opened");
        // Selection: whole lines of the selection only.
        s.execute("editor.setSelection", &json!({ "anchor": 14, "head": 16 })).unwrap();
        s.execute("file.export.filesOrSelectionToHtml", &json!({ "output": out, "selection": true })).unwrap();
        let html = std::fs::read_to_string(&out).unwrap();
        assert!(!html.contains("main") && html.contains("tag"));
        let v = s.execute("file.print", &json!({})).unwrap();
        let printed = PathBuf::from(v["output"].as_str().unwrap());
        assert!(printed.is_file());
        assert!(matches!(s.effects.last(), Some(Effect::OpenUrl(u)) if u.ends_with("a.rs.html")));
        let _ = std::fs::remove_file(printed);
    }

    #[test]
    fn new_python_package_creates_init_py() {
        let (_d, root, mut s) = project(&[("x.py", b"")]);
        s.execute("file.new.pythonPackage", &json!({})).unwrap();
        assert!(matches!(
            &s.popup,
            Some(Popup::NewItem { kind: crate::popup::NewItemKind::PythonPackage, dir, .. }) if *dir == root
        ));
        s.execute("file.new.pythonPackage", &json!({ "path": root.join("pkg/sub") })).unwrap();
        assert!(root.join("pkg/sub/__init__.py").is_file());
        assert!(s.execute("file.new.pythonPackage", &json!({ "path": root.join("pkg/sub") })).is_err());
        assert!(s.execute("file.new.pythonPackage", &json!({ "path": root.join("x.py") })).is_err());
    }

    #[test]
    fn compare_with_file_and_clipboard() {
        let (_d, root, mut s) = project(&[("a.txt", b"one\ntwo\n"), ("b.txt", b"one\nTWO\n")]);
        open(&mut s, &root.join("a.txt"));
        s.execute("view.compareWith", &json!({})).unwrap();
        assert_eq!(s.misc.compare_picker.as_deref(), Some(""));
        let v = s.execute("view.compareWith", &json!({ "path": root.join("b.txt") })).unwrap();
        assert_eq!(v["identical"], false);
        let c = s.misc.compare.clone().unwrap();
        assert_eq!((c.left.as_str(), c.right.as_str()), ("one\ntwo\n", "one\nTWO\n"));
        assert!(s.misc.compare_picker.is_none());
        assert!(s.execute("view.compareWith", &json!({ "path": root.join("missing.txt") })).is_err());
        let v = s.execute("view.compareWithClipboard", &json!({ "text": "one\r\ntwo\r\n" })).unwrap();
        assert_eq!(v["identical"], true);
        // Without text: the internal clipboard, and a request for the system one.
        s.clipboard = Some(("two".into(), false));
        s.execute("editor.setSelection", &json!({ "anchor": 4, "head": 7 })).unwrap();
        let v = s.execute("view.compareWithClipboard", &json!({})).unwrap();
        assert_eq!(v["identical"], true);
        assert!(s.misc.clipboard_request);
        s.execute("compare.close", &json!({})).unwrap();
        assert!(s.misc.compare.is_none() && !s.misc.clipboard_request);
    }

    #[test]
    fn context_info_shows_enclosing_headers() {
        let src = "mod m {\n    impl S {\n        fn f(&self) {\n            let x = 1;\n        }\n    }\n}\n";
        let (_d, root, mut s) = project(&[("a.rs", src.as_bytes())]);
        open(&mut s, &root.join("a.rs"));
        let caret = src.find("let x").unwrap();
        s.execute("editor.setSelection", &json!({ "anchor": caret, "head": caret })).unwrap();
        let v = s.execute("view.contextInfo", &json!({})).unwrap();
        let headers: Vec<String> = serde_json::from_value(v["headers"].clone()).unwrap();
        assert_eq!(headers, vec!["    1  mod m {", "    2      impl S {", "    3          fn f(&self) {"]);
        assert!(matches!(&s.popup, Some(Popup::Hover { text, .. }) if text.contains("impl S {")));
        s.popup = None;
        s.execute("editor.setSelection", &json!({ "anchor": 0, "head": 0 })).unwrap();
        assert_eq!(s.execute("view.contextInfo", &json!({})).unwrap()["headers"], json!([]));
        assert!(s.popup.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn background_processes_are_listed_and_stopped() {
        let (_d, root, mut s) = project(&[]);
        let v = s.execute("run.stopBackgroundProcesses", &json!({})).unwrap();
        assert_eq!(v["processes"], json!([]));
        assert_eq!(s.misc.processes, Some(crate::misc::ProcessesMode::Stop));
        let cmd = crate::runs::ProcessCommand::shell("sleep 30", &root);
        let tab = crate::runs::start_process(&mut s, &cmd, "Sleeper").unwrap();
        let v = s.execute("run.showRunningList", &json!({})).unwrap();
        let list = v["processes"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!((list[0]["kind"].as_str(), list[0]["title"].as_str()), (Some("run"), Some("Sleeper")));
        s.execute("processes.stop", &json!({ "kind": "run", "id": tab.0 })).unwrap();
        pump_until(&mut s, |s| !s.runs.is_running(tab));
        assert!(s.execute("processes.stop", &json!({ "kind": "run", "id": tab.0 })).is_err());
        assert!(s.execute("processes.stop", &json!({ "kind": "nope", "id": "1" })).is_err());
        assert_eq!(s.execute("window.processes.autoShow", &json!({})).unwrap()["on"], true);
        s.execute("processes.close", &json!({})).unwrap();
        // Opening the project starts background work of its own (e.g. interpreter detection);
        // auto show reacts to tasks appearing, so start from none.
        pump_until(&mut s, |s| s.ws.tasks.is_empty());
        s.pump();
        s.ws.tasks.insert("t".into(), codecraft_workspace::BackgroundTask {
            title: "Indexing".into(),
            message: None,
            percentage: None,
        });
        s.pump();
        assert_eq!(s.misc.processes, Some(crate::misc::ProcessesMode::Tasks), "auto show");
    }

    #[cfg(unix)]
    #[test]
    fn manage_accounts_shows_the_cli_status_without_logging_in() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("gh");
        let log = dir.path().join("args.txt");
        std::fs::write(&gh, format!("#!/bin/sh\necho \"$@\" > '{}'\necho 'github.com'\necho '  Logged in to github.com account tester' >&2\n", log.display()))
            .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.misc.cli_override.insert("gh", gh);
        s.misc.cli_override.insert("glab", dir.path().join("missing-glab"));
        s.execute("git.gitHub.manageAccounts", &json!({})).unwrap();
        assert!(s.misc.accounts.as_ref().unwrap().loading);
        pump_until(&mut s, |s| s.misc.accounts.as_ref().is_some_and(|a| !a.loading));
        let a = s.misc.accounts.clone().unwrap();
        assert!(a.text.contains("Logged in to github.com account tester"), "{}", a.text);
        assert!(a.hint.contains("gh auth login"));
        assert_eq!(std::fs::read_to_string(&log).unwrap().trim(), "auth status");
        s.execute("git.gitLab.manageAccounts", &json!({})).unwrap();
        pump_until(&mut s, |s| s.misc.accounts.as_ref().is_some_and(|a| !a.loading));
        assert!(s.misc.accounts.as_ref().unwrap().text.contains("was not found"));
        s.execute("accounts.close", &json!({})).unwrap();
        assert!(s.misc.accounts.is_none());
    }

    #[test]
    fn recently_changed_files_and_recent_changes() {
        let (_d, root, mut s) = project(&[("a.txt", b"a\n"), ("b.txt", b"b\n")]);
        assert_eq!(s.execute("view.recentlyChangedFiles", &json!({})).unwrap()["files"], json!([]));
        open(&mut s, &root.join("a.txt"));
        s.execute("editor.type", &json!({ "text": "x" })).unwrap();
        s.execute("file.saveAll", &json!({})).unwrap();
        open(&mut s, &root.join("b.txt"));
        s.execute("editor.type", &json!({ "text": "y" })).unwrap();
        let v = s.execute("view.recentlyChangedFiles", &json!({})).unwrap();
        assert_eq!(v["files"], json!([root.join("b.txt"), root.join("a.txt")]));
        assert!(matches!(&s.popup, Some(Popup::Locations { title, items, .. }) if title == "Recently Changed Files" && items.len() == 2));
        let v = s.execute("view.recentChanges", &json!({})).unwrap();
        assert!(v["revisions"].as_u64().unwrap() >= 1);
        assert!(s.local_history.view.is_some());
    }

    #[test]
    fn database_changes_and_misc_dialogs() {
        let mut s = Session::new();
        s.lsp_enabled = false;
        assert_eq!(s.execute("databaseChanges.list", &json!({})).unwrap()["tables"], json!([]));
        assert_eq!(s.execute("databaseChanges.submitAll", &json!({})).unwrap()["tables"], 0);
        s.execute("view.toolWindows.databaseChanges", &json!({})).unwrap();
        assert!(s.ws.layout.is_visible(codecraft_workspace::ToolWindowId::DatabaseChanges));
        s.execute("window.editorTabs.configureEditorTabs", &json!({})).unwrap();
        assert!(matches!(s.popup, Some(Popup::Settings)));
        assert_eq!(s.prefs.dialog.as_ref().map(|d| d.page.as_str()), Some(crate::settings::PAGE_EDITOR_TABS));
        s.execute("settings.dialog.cancel", &json!({})).unwrap();
        s.execute("file.newProjectsSetup.settingsForNewProjects", &json!({})).unwrap();
        assert!(matches!(s.popup, Some(Popup::Settings)));
        // Quick Type Definition needs a language server.
        assert!(s.execute("view.quickTypeDefinition", &Value::Null).is_err());
    }
}
