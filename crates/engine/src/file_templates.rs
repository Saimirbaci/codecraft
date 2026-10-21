//! File › Save File as Template… and File › New › From Template…: file templates kept in
//! `$CODECRAFT_HOME/file-templates.json`.
//!
//! A template is a name, an extension and a text. Creating a file from it expands `${NAME}` (the
//! new file's name without extension), `${FILE_NAME}`, `${DATE}`, `${YEAR}`, `${USER}` and
//! `${PROJECT_NAME}`; other `${…}` stay as written.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::commands::{CmdResult, CommandSpec, Context, always, has_editor, ok, spec};
use crate::params::{opt_path, opt_str, req_str};
use crate::user_store::{Store, UserHome};
use crate::{CommandError, Session};

pub const FILE: &str = "file-templates.json";
/// Largest template text (chars).
const MAX_TEMPLATE_CHARS: usize = 1 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTemplate {
    pub name: String,
    /// Without the dot (`rs`, `py`); empty for none.
    pub extension: String,
    pub text: String,
}

/// The Save as Template / New from Template dialogs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TemplateDialog {
    Save { name: String, extension: String, error: Option<String> },
    New { template: Option<String>, name: String, directory: PathBuf, error: Option<String> },
}

pub struct FileTemplatesState {
    pub saved: Store<FileTemplate>,
    pub home: UserHome,
    pub dialog: Option<TemplateDialog>,
}

impl Default for FileTemplatesState {
    fn default() -> Self {
        Self { saved: Store::new(FILE), home: UserHome::default(), dialog: None }
    }
}

impl FileTemplatesState {
    pub fn set_home(&mut self, dir: PathBuf) {
        self.home.dir_override = Some(dir);
        self.saved.reset();
    }

    pub fn list(&mut self) -> &[FileTemplate] {
        self.saved.ensure(&self.home);
        &self.saved.items
    }
}

/// Expand a template's variables for a new file named `file_name`.
pub fn expand(text: &str, file_name: &str, project: &str) -> String {
    let stem = Path::new(file_name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
    text.replace("${NAME}", &stem)
        .replace("${FILE_NAME}", file_name)
        .replace("${DATE}", &crate::user_store::today())
        .replace("${YEAR}", &crate::user_store::current_year().to_string())
        .replace("${USER}", &user)
        .replace("${PROJECT_NAME}", project)
}

fn valid_name(name: &str) -> Result<String, CommandError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(CommandError::BadParams("the name is empty".into()));
    }
    if name.contains(['/', '\\', '\0']) || name == "." || name == ".." || name.chars().count() > 200 {
        return Err(CommandError::BadParams(format!("'{name}' is not a valid name")));
    }
    Ok(name.to_string())
}

fn save_as_template(s: &mut Session, p: &Value) -> CmdResult {
    let (path, text) = {
        let (_, d) = s.ws.active().ok_or_else(|| CommandError::Failed("no editor is open".into()))?;
        (d.path.clone(), d.doc.text())
    };
    let ext_default = path.as_deref().and_then(Path::extension).map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    let Some(name) = opt_str(p, "name") else {
        let stem = path.as_deref().and_then(Path::file_stem).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        s.file_templates.dialog = Some(TemplateDialog::Save { name: stem, extension: ext_default, error: None });
        return Ok(json!({ "dialog": true }));
    };
    let name = valid_name(name)?;
    if text.chars().count() > MAX_TEMPLATE_CHARS {
        return Err(CommandError::Failed("the file is too large for a template".into()));
    }
    let extension = opt_str(p, "extension").map(|e| e.trim().trim_start_matches('.').to_string()).unwrap_or(ext_default);
    if extension.contains(['/', '\\', '\0']) {
        return Err(CommandError::BadParams(format!("'{extension}' is not an extension")));
    }
    let home = s.file_templates.home.clone();
    s.file_templates.saved.ensure(&home);
    s.file_templates.saved.items.retain(|t| t.name != name);
    s.file_templates.saved.items.push(FileTemplate { name: name.clone(), extension, text });
    let file = s.file_templates.saved.save(&home).map_err(CommandError::Failed)?;
    s.file_templates.dialog = None;
    s.status = format!("Saved file template '{name}'");
    Ok(json!({ "name": name, "path": file }))
}

/// Where new files go: the selected project directory, else the active file's, else the root.
fn default_directory(s: &Session) -> Option<PathBuf> {
    if let Some(sel) = s.ws.project.as_ref().and_then(|p| p.selected.clone()) {
        return Some(if sel.is_dir() { sel } else { sel.parent()?.to_path_buf() });
    }
    if let Some(dir) = s.ws.active().and_then(|(_, d)| d.path.as_ref()).and_then(|p| p.parent()) {
        return Some(dir.to_path_buf());
    }
    s.project_root()
}

fn new_from_template(s: &mut Session, p: &Value) -> CmdResult {
    let template = opt_str(p, "template").map(str::to_string);
    let directory = opt_path(p, "directory").map(|d| crate::normalize_path(&d)).or_else(|| default_directory(s));
    let (Some(template), Some(name)) = (template.clone(), opt_str(p, "name")) else {
        if s.file_templates.list().is_empty() {
            return Err(CommandError::Failed("there are no file templates (File › Save File as Template…)".into()));
        }
        let directory = directory.ok_or_else(|| CommandError::Failed("open a project or a file first".into()))?;
        let first = s.file_templates.list().first().map(|t| t.name.clone());
        s.file_templates.dialog = Some(TemplateDialog::New { template: template.or(first), name: String::new(), directory, error: None });
        return Ok(json!({ "dialog": true }));
    };
    let directory = directory.ok_or_else(|| CommandError::BadParams("missing `directory`".into()))?;
    if !directory.is_dir() {
        return Err(CommandError::Failed(format!("{} is not a directory", directory.display())));
    }
    let t = s
        .file_templates
        .list()
        .iter()
        .find(|t| t.name == template)
        .cloned()
        .ok_or_else(|| CommandError::BadParams(format!("no file template named '{template}'")))?;
    let name = valid_name(name)?;
    let file_name = if t.extension.is_empty() || Path::new(&name).extension().is_some() { name } else { format!("{name}.{}", t.extension) };
    let path = directory.join(&file_name);
    if path.exists() {
        return Err(CommandError::Failed(format!("{} already exists", path.display())));
    }
    let project = s.ws.project.as_ref().map(|p| p.name.clone()).unwrap_or_default();
    let text = expand(&t.text, &file_name, &project);
    codecraft_fs::write_text_file_atomic(&path, &text).map_err(|e| CommandError::Failed(format!("cannot create {}: {e}", path.display())))?;
    s.file_templates.dialog = None;
    if s.ws.project.is_some() {
        s.jobs_rescan();
    }
    s.open_file(&path)?;
    crate::remote::after_save(s, std::slice::from_ref(&path));
    Ok(json!({ "path": path }))
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "file.saveFileAsTemplate",
            "Save File as Template…",
            None,
            "{name?: string, extension?: string} — save the active file's text as a file template; without `name` opens the dialog",
            Context::Global,
            has_editor,
            save_as_template,
        ),
        spec(
            "file.newFromTemplate",
            "From Template…",
            None,
            "{template?: string, name?: string, directory?: string} — create a file from a saved file template (expanding ${NAME}, ${DATE}, ${YEAR}, ${USER}, ${PROJECT_NAME}); without `template` and `name` opens the dialog",
            Context::Global,
            always,
            new_from_template,
        ),
        spec("file.templates.list", "File Templates", None, "→ [{name, extension, text}]", Context::Global, always, |s, _| {
            Ok(json!(s.file_templates.list()))
        }),
        spec("file.templates.delete", "Delete File Template", None, "{name: string}", Context::Global, always, |s, p| {
            let name = req_str(p, "name")?.to_string();
            let home = s.file_templates.home.clone();
            s.file_templates.saved.ensure(&home);
            let before = s.file_templates.saved.items.len();
            s.file_templates.saved.items.retain(|t| t.name != name);
            if before == s.file_templates.saved.items.len() {
                return Err(CommandError::BadParams(format!("no file template named '{name}'")));
            }
            s.file_templates.saved.save(&home).map_err(CommandError::Failed)?;
            ok()
        }),
        spec("file.templates.dialog.ok", "OK", None, "Run the open file template dialog", Context::Global, |s| s.file_templates.dialog.as_ref().map(|_| ()).ok_or_else(|| "no dialog is open".into()), |s, _| {
            let d = s.file_templates.dialog.clone().ok_or_else(|| CommandError::Failed("no dialog".into()))?;
            let r = match &d {
                TemplateDialog::Save { name, extension, .. } => save_as_template(s, &json!({ "name": name, "extension": extension })),
                TemplateDialog::New { template, name, directory, .. } => {
                    new_from_template(s, &json!({ "template": template, "name": name, "directory": directory }))
                }
            };
            if let Err(e) = &r {
                match &mut s.file_templates.dialog {
                    Some(TemplateDialog::Save { error, .. } | TemplateDialog::New { error, .. }) => *error = Some(e.to_string()),
                    None => {}
                }
            }
            r
        }),
        spec("file.templates.dialog.cancel", "Cancel", None, "", Context::Global, always, |s, _| {
            s.file_templates.dialog = None;
            ok()
        }),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn save_and_create_from_template() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let src = dir.join("widget.rs");
        std::fs::write(&src, "//! ${NAME} (${YEAR})\npub struct ${NAME};\n").unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(&dir.join("home"));
        s.execute("file.open", &json!({ "path": src })).unwrap();
        assert_eq!(s.execute("file.saveFileAsTemplate", &json!({})).unwrap()["dialog"], true);
        assert!(matches!(&s.file_templates.dialog, Some(TemplateDialog::Save { name, extension, .. }) if name == "widget" && extension == "rs"));
        s.execute("file.templates.dialog.ok", &json!({})).unwrap();
        s.file_templates.saved.reset();
        assert_eq!(s.file_templates.list().len(), 1);
        let v = s.execute("file.newFromTemplate", &json!({ "template": "widget", "name": "Gadget", "directory": dir })).unwrap();
        let made = PathBuf::from(v["path"].as_str().unwrap());
        assert_eq!(made, dir.join("Gadget.rs"));
        let text = std::fs::read_to_string(&made).unwrap();
        assert!(text.starts_with("//! Gadget ("), "{text}");
        assert!(text.contains("pub struct Gadget;"));
        assert!(!text.contains("${YEAR}"));
        assert_eq!(s.ws.active().unwrap().1.path.as_deref(), Some(made.as_path()));
        // Existing file, bad names, unknown template.
        assert!(s.execute("file.newFromTemplate", &json!({ "template": "widget", "name": "Gadget", "directory": dir })).is_err());
        assert!(s.execute("file.newFromTemplate", &json!({ "template": "widget", "name": "../x", "directory": dir })).is_err());
        assert!(s.execute("file.newFromTemplate", &json!({ "template": "nope", "name": "y", "directory": dir })).is_err());
        s.execute("file.templates.delete", &json!({ "name": "widget" })).unwrap();
        assert!(s.execute("file.newFromTemplate", &json!({})).is_err());
    }
}
