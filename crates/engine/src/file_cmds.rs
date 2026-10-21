//! File menu and application menu commands.

use std::path::Path;

use serde_json::{Value, json};

use codecraft_text::LineEnding;

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, has_project, ok, simple, spec};
use crate::params::opt_path;
use crate::popup::NewItemKind;
use crate::{CommandError, Effect, Popup, Session};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "file.open",
            "Open…",
            None,
            "{path?: string} — a file opens in an editor (and its Cargo project if none is open); a directory opens as the project. Without `path` the UI asks.",
            Context::Global,
            always,
            open,
        ),
        simple("file.saveAll", "Save All", always, save_all),
        simple("file.reloadAllFromDisk", "Reload All from Disk", always, reload_all),
        simple("file.closeProject", "Close Project", has_project, close_project),
        simple("file.invalidateCaches", "Invalidate Caches…", has_project, invalidate_caches),
        spec(
            "file.fileProperties.lineSeparators.lfUnixAndMacOS",
            "LF - Unix and macOS (\\n)",
            None,
            "",
            Context::Global,
            has_editor,
            |s, _| set_line_ending(s, LineEnding::Lf),
        ),
        spec(
            "file.fileProperties.lineSeparators.crlfWindows",
            "CRLF - Windows (\\r\\n)",
            None,
            "",
            Context::Global,
            has_editor,
            |s, _| set_line_ending(s, LineEnding::CrLf),
        ),
        spec(
            "file.new.file",
            "File",
            None,
            "{path?: string} — create and open a file",
            Context::Global,
            always,
            |s, p| new_item(s, p, NewItemKind::File),
        ),
        spec(
            "file.new.directory",
            "Directory",
            None,
            "{path?: string} — create a directory",
            Context::Global,
            has_project,
            |s, p| new_item(s, p, NewItemKind::Directory),
        ),
        spec(
            "file.new.cargoCrate",
            "Cargo Crate",
            None,
            "{path?: string, lib?: bool} — `cargo new` inside the project",
            Context::Global,
            has_project,
            |s, p| new_item(s, p, NewItemKind::CargoCrate),
        ),
        spec(
            "file.new.project",
            "Project…",
            None,
            "{path?: string, lib?: bool} — create a Cargo project with `cargo new` and open it",
            Context::Global,
            always,
            new_project,
        ),
        simple("app.aboutRustRover", "About Code Craft", always, |s, _| {
            s.popup = Some(Popup::About);
            ok()
        }),
        simple("app.quitRustRover", "Quit Code Craft", always, |s, _| {
            let _ = s.save_all();
            s.effects.push(Effect::Quit);
            ok()
        }),
        simple("app.quitAndKeepWindows", "Quit and Keep Windows", always, |s, _| {
            let _ = s.save_all();
            s.effects.push(Effect::Quit);
            ok()
        }),
        spec(
            "file.synchronize",
            "Synchronize",
            None,
            "Reload files changed on disk (unmodified ones) and rescan the project; the UI runs this on window focus.",
            Context::Global,
            always,
            synchronize,
        ),
    ]
}

fn open(s: &mut Session, p: &Value) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        s.effects.push(Effect::PickOpenPath);
        return ok();
    };
    let path = crate::normalize_path(&path);
    if path.is_dir() {
        s.open_project(&path);
        return Ok(json!({ "project": path }));
    }
    if !path.is_file() {
        return Err(CommandError::Failed(format!("{} does not exist", path.display())));
    }
    if s.ws.project.is_none()
        && let Some(manifest) = codecraft_cargo::find_manifest(&path)
        && let Some(dir) = manifest.parent()
    {
        s.open_project(dir);
    }
    let editor = s.open_file(&path)?;
    Ok(json!({ "editor": editor.0 }))
}

fn save_all(s: &mut Session, _: &Value) -> CmdResult {
    let n = s.save_all()?;
    s.status = if n == 0 { "All files are saved".into() } else { format!("Saved {n} file(s)") };
    if n > 0 {
        crate::settings_cmds::check_on_save(s);
    }
    Ok(json!({ "saved": n }))
}

/// Reload every open document from disk, discarding unsaved edits (undoable per document).
fn reload_all(s: &mut Session, _: &Value) -> CmdResult {
    let mut reloaded = 0usize;
    let mut errors = Vec::new();
    let ids: Vec<_> = s.ws.docs().filter(|d| d.path.is_some()).map(|d| d.id).collect();
    for id in ids {
        let Some(path) = s.ws.doc(id).and_then(|d| d.path.clone()) else { continue };
        match codecraft_fs::read_text_file(&path) {
            Ok(text) => {
                let editor = s.ws.editor_for_doc(id);
                let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
                if let Some(od) = s.ws.doc_mut(id)
                    && od.doc.replace_all(&text, &sels).is_ok()
                {
                    od.doc.mark_saved();
                    od.disk_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                    reloaded += 1;
                }
                if let Some(e) = editor {
                    let len = s.ws.doc(id).map(|d| d.doc.len_chars()).unwrap_or(0);
                    s.ws.set_selections(e, sels.clamp(len));
                }
            }
            Err(e) => errors.push(e.to_string()),
        }
    }
    s.jobs_rescan();
    if !errors.is_empty() {
        s.status = errors.join("; ");
    } else {
        s.status = format!("Reloaded {reloaded} file(s) from disk");
    }
    Ok(json!({ "reloaded": reloaded }))
}

fn synchronize(s: &mut Session, _: &Value) -> CmdResult {
    let mut reloaded = 0usize;
    let ids: Vec<_> = s.ws.docs().filter(|d| d.path.is_some() && !d.doc.is_modified()).map(|d| d.id).collect();
    for id in ids {
        let Some((path, old)) = s.ws.doc(id).and_then(|d| Some((d.path.clone()?, d.disk_mtime))) else { continue };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime.is_none() || mtime == old {
            continue;
        }
        if let Ok(text) = codecraft_fs::read_text_file(&path) {
            let editor = s.ws.editor_for_doc(id);
            let sels = editor.and_then(|e| s.ws.editor(e)).map(|e| e.selections.clone()).unwrap_or_default();
            if let Some(od) = s.ws.doc_mut(id) {
                if od.doc.replace_all(&text, &sels).is_ok() {
                    od.doc.mark_saved();
                    reloaded += 1;
                }
                od.disk_mtime = mtime;
            }
            if let Some(e) = editor {
                let len = s.ws.doc(id).map(|d| d.doc.len_chars()).unwrap_or(0);
                s.ws.set_selections(e, sels.clamp(len));
            }
        }
    }
    s.jobs_rescan();
    Ok(json!({ "reloaded": reloaded }))
}

fn close_project(s: &mut Session, _: &Value) -> CmdResult {
    s.close_project();
    ok()
}

fn invalidate_caches(s: &mut Session, _: &Value) -> CmdResult {
    s.restart_server(None);
    s.jobs_rescan();
    s.status = "Caches invalidated: restarting language servers".into();
    ok()
}

fn set_line_ending(s: &mut Session, le: LineEnding) -> CmdResult {
    if let Some((_, od)) = s.ws.active_mut() {
        od.doc.set_line_ending(le);
    }
    s.status = format!("Line separator: {}", le.label());
    ok()
}

/// Directory for new items: the selected directory in the Project view, else the active file's
/// directory, else the project root.
fn target_dir(s: &Session) -> Option<std::path::PathBuf> {
    let p = s.ws.project.as_ref()?;
    if let Some(sel) = &p.selected {
        return Some(if sel.is_dir() { sel.clone() } else { sel.parent()?.to_path_buf() });
    }
    Some(p.root.clone())
}

fn new_item(s: &mut Session, p: &Value, kind: NewItemKind) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        let dir = target_dir(s)
            .or_else(|| s.project_root())
            .ok_or_else(|| CommandError::Disabled("no project is open".into()))?;
        s.popup = Some(Popup::NewItem { kind, dir, name: String::new() });
        return ok();
    };
    let path = crate::normalize_path(&path);
    if path.exists() {
        return Err(CommandError::Failed(format!("{} already exists", path.display())));
    }
    let io = |e: std::io::Error| CommandError::Failed(e.to_string());
    match kind {
        NewItemKind::File => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(io)?;
            }
            std::fs::write(&path, "").map_err(io)?;
            s.jobs_rescan();
            s.open_file(&path)?;
        }
        NewItemKind::Directory => {
            std::fs::create_dir_all(&path).map_err(io)?;
            s.jobs_rescan();
        }
        NewItemKind::CargoCrate => {
            cargo_new(&path, crate::params::bool_or(p, "lib", false))?;
            s.jobs_rescan();
        }
        NewItemKind::PythonPackage => {
            crate::misc_cmds::create_python_package(&path)?;
            s.jobs_rescan();
        }
    }
    if s.remote.project.is_some() {
        crate::remote::tree_operation(s, &path, None, path.is_dir(), true)?;
    }
    if let Some(proj) = &mut s.ws.project {
        proj.reveal(&path);
    }
    Ok(json!({ "path": path }))
}

fn new_project(s: &mut Session, p: &Value) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        s.effects.push(Effect::PickNewProjectPath);
        return ok();
    };
    let path = crate::normalize_path(&path);
    cargo_new(&path, crate::params::bool_or(p, "lib", false))?;
    s.open_project(&path);
    let main = path.join("src").join(if crate::params::bool_or(p, "lib", false) { "lib.rs" } else { "main.rs" });
    if main.is_file() {
        s.open_file(&main)?;
    }
    Ok(json!({ "project": path }))
}

fn cargo_new(path: &Path, lib: bool) -> Result<(), CommandError> {
    let out = std::process::Command::new(codecraft_cargo::find_cargo())
        .arg("new")
        .arg(if lib { "--lib" } else { "--bin" })
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| CommandError::Failed(format!("cannot run cargo: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(CommandError::Failed(String::from_utf8_lossy(&out.stderr).trim().to_string()))
    }
}
