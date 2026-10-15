//! Settings pages added in M13 track ST, as commands: Code Style per language (`codeStyle.*`),
//! the editor font list (`fonts.*`), editable live templates (`liveTemplates.*`), colour scheme
//! export / import (`colorScheme.export`, `colorScheme.import`) and keymap copies
//! (`keymap.duplicate`, `keymap.rename`, `keymap.delete`).
//!
//! Like the other settings commands, they change the Settings dialog's draft while it is open
//! (Code Style formatter options are written to the project's files on Apply / OK) and the live
//! settings otherwise. Live templates are saved to the user templates file at once.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use codecraft_workspace::{LanguageIndent, Settings};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, simple, spec};
use crate::params::{bool_or, opt_path, opt_str, req_str};
use crate::settings::code_style::{self, StyleFileKind, StyleValues};
use crate::settings::{StyleDraft, fonts, keymap_copies, scheme_io};
use crate::settings_cmds::{dialog, dialog_mut, edit};
use crate::templates::{self, Template};
use crate::{CommandError, Effect, Session, settings};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        // ------------------------------------------------------------ code style
        spec(
            "codeStyle.get",
            "Code Style: Get",
            None,
            "{language: string} → {language, label, file, exists, options: [{key, label, kind, choices?, min?, max?, default, unstable, description, value}], indent: {tab_size, indent_size, use_tab}, indentCustom}; `language` is a Code Style id (rust, cpp, python, toml, …)",
            Context::Global,
            always,
            code_style_get,
        ),
        spec(
            "codeStyle.set",
            "Code Style: Set Option",
            None,
            "{language: string, key: string, value: any} — a formatter option (rustfmt.toml / .clang-format; null removes it) or the editor's `tab_size`, `indent_size`, `use_tab` for the language. Edits the dialog's draft while Settings is open; otherwise writes at once.",
            Context::Global,
            always,
            code_style_set,
        ),
        spec(
            "codeStyle.reset",
            "Code Style: Reset",
            None,
            "{language: string} — remove the language's indent setting and its formatter options",
            Context::Global,
            always,
            code_style_reset,
        ),
        spec(
            "codeStyle.preview",
            "Code Style: Preview",
            None,
            "{language: string} — format a sample with the (draft) options using rustfmt / clang-format on a worker; the result is in `codeStyle.previewResult`",
            Context::Global,
            always,
            code_style_preview,
        ),
        simple("codeStyle.previewResult", "Code Style: Preview Result", always, |s, _| {
            Ok(json!(s.prefs_services.preview.view))
        }),
        // ------------------------------------------------------------ fonts
        spec(
            "fonts.list",
            "List Fonts",
            None,
            "{monospaceOnly?: bool} → {scanning, families: [{name, monospace}], current, loaded, error}; the first call scans the system's font directories on a worker",
            Context::Global,
            always,
            fonts_list,
        ),
        simple("fonts.rescan", "Rescan Fonts", always, |s, _| {
            s.prefs_services.fonts.families = None;
            s.prefs_services.fonts.start_scan();
            ok()
        }),
        // ------------------------------------------------------------ live templates
        spec(
            "liveTemplates.list",
            "List Live Templates",
            None,
            "{language?: string} → {path, error, contexts, functions, templates: [{abbreviation, description, text, languages, defaults, expressions, skip_if_defined, enabled, builtin, replacesBuiltin}]}",
            Context::Global,
            always,
            templates_list,
        ),
        spec(
            "liveTemplates.save",
            "Save Live Template",
            None,
            "{abbreviation, text, languages: [id], description?, defaults?: {VAR: text}, expressions?: {VAR: expr}, skipIfDefined?: [VAR], enabled?, original?: {abbreviation, languages}} — add a template or replace `original` (editing a built-in makes a user copy)",
            Context::Global,
            always,
            templates_save,
        ),
        spec(
            "liveTemplates.delete",
            "Delete Live Template",
            None,
            "{abbreviation: string, language?: string} — a user template is removed; a built-in is hidden until Restore Defaults",
            Context::Global,
            always,
            |s, p| {
                let abbr = req_str(p, "abbreviation")?.to_string();
                let path = s.editing.templates.delete(&abbr, opt_str(p, "language")).map_err(CommandError::Failed)?;
                Ok(json!({ "path": path }))
            },
        ),
        spec(
            "liveTemplates.setEnabled",
            "Enable Live Template",
            None,
            "{abbreviation: string, language?: string, enabled: bool}",
            Context::Global,
            always,
            |s, p| {
                let abbr = req_str(p, "abbreviation")?.to_string();
                let on = p
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .ok_or_else(|| CommandError::BadParams("missing bool param `enabled`".into()))?;
                let path =
                    s.editing.templates.set_enabled(&abbr, opt_str(p, "language"), on).map_err(CommandError::Failed)?;
                Ok(json!({ "path": path }))
            },
        ),
        simple("liveTemplates.restoreDefaults", "Restore Default Live Templates", always, |s, _| {
            let path = s.editing.templates.restore_defaults().map_err(CommandError::Failed)?;
            s.status = "Built-in live templates restored".into();
            Ok(json!({ "path": path }))
        }),
        // ------------------------------------------------------------ colour scheme files
        spec(
            "colorScheme.export",
            "Export Color Scheme…",
            None,
            "{name?: string, path?: string} — write a scheme (default: the current one) as CodeCraft JSON; without `path` a save dialog asks",
            Context::Global,
            always,
            scheme_export,
        ),
        spec(
            "colorScheme.import",
            "Import Color Scheme…",
            None,
            "{path?: string} — add a scheme from CodeCraft JSON or an IntelliJ-format .icls file (known attributes mapped to our colours) and select it → {name, mapped, skipped}; without `path` an open dialog asks",
            Context::Global,
            always,
            scheme_import,
        ),
        // ------------------------------------------------------------ keymap copies
        spec(
            "keymap.duplicate",
            "Duplicate Keymap…",
            None,
            "{from?: string, name?: string} — a user keymap that inherits from `from` (default: the active keymap); it becomes active → {name}",
            Context::Global,
            always,
            |s, p| {
                let from = opt_str(p, "from").map(String::from);
                let name = opt_str(p, "name").map(String::from);
                let name = edit(s, |st| keymap_copies::duplicate(st, from.as_deref(), name.as_deref()))?;
                Ok(json!({ "name": name }))
            },
        ),
        spec(
            "keymap.rename",
            "Rename Keymap…",
            None,
            "{name?: string, newName: string} — rename a user keymap (default: the active one)",
            Context::Global,
            always,
            |s, p| {
                let new = req_str(p, "newName")?.to_string();
                let old = opt_str(p, "name").map(String::from);
                let name = edit(s, |st| {
                    let old = old.unwrap_or_else(|| crate::keymaps::active(st));
                    keymap_copies::rename(st, &old, &new)
                })?;
                Ok(json!({ "name": name }))
            },
        ),
        spec(
            "keymap.delete",
            "Delete Keymap",
            None,
            "{name?: string} — delete a user keymap (default: the active one); keymaps that inherited from it inherit from its parent",
            Context::Global,
            always,
            |s, p| {
                let name = opt_str(p, "name").map(String::from);
                edit(s, |st| {
                    let name = name.unwrap_or_else(|| crate::keymaps::active(st));
                    keymap_copies::delete(st, &name)
                })?;
                ok()
            },
        ),
    ]
}

// ---------------------------------------------------------------- code style

/// A Code Style language id from params (`c` and `cpp` are one page).
fn language_param(p: &Value) -> Result<&'static str, CommandError> {
    let l = req_str(p, "language")?.trim().to_ascii_lowercase();
    let l = if l == "c" || l == "c/c++" || l == "c++" { "cpp".to_string() } else { l };
    code_style::LANGUAGES
        .iter()
        .find(|(id, label)| *id == l || label.eq_ignore_ascii_case(&l))
        .map(|(id, _)| *id)
        .ok_or_else(|| CommandError::BadParams(format!("unknown Code Style language `{l}`")))
}

fn settings_source(s: &Session) -> &Settings {
    dialog(s).map(|d| &d.draft).unwrap_or(&s.ws.settings)
}

/// A file kind's current options: the dialog's draft (loaded from the file the first time),
/// else the file's.
fn style_values(s: &mut Session, kind: StyleFileKind) -> Result<Option<StyleValues>, String> {
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else { return Ok(None) };
    if let Some(d) = dialog(s).and_then(|d| d.code_style.get(kind)) {
        return Ok(Some(d.values.clone()));
    }
    let saved = code_style::read_file(kind, &kind.path(&root))?.unwrap_or_default();
    if let Some(d) = dialog_mut(s) {
        *d.code_style.slot(kind) = Some(StyleDraft { saved: saved.clone(), values: saved.clone() });
    }
    Ok(Some(saved))
}

/// Store a file kind's options: in the draft while Settings is open, else in the file.
fn store_values(s: &mut Session, kind: StyleFileKind, values: StyleValues) -> Result<(), String> {
    if let Some(d) = dialog_mut(s) {
        let slot = d.code_style.slot(kind);
        match slot {
            Some(draft) => draft.values = values,
            None => *slot = Some(StyleDraft { saved: StyleValues::new(), values }),
        }
        d.error = None;
        return Ok(());
    }
    let root = s.ws.project.as_ref().map(|p| p.root.clone()).ok_or("open a project to change its formatter files")?;
    let path = code_style::write_values(kind, &root, &values)?;
    s.status = format!("Saved {}", path.display());
    Ok(())
}

/// Write the dialog's Code Style drafts to the project's files (Apply / OK).
pub(crate) fn write_code_style(s: &mut Session) -> Result<(), String> {
    let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone()) else { return Ok(()) };
    let Some(d) = dialog_mut(s) else { return Ok(()) };
    for kind in [StyleFileKind::Rustfmt, StyleFileKind::ClangFormat] {
        let Some(draft) = d.code_style.slot(kind).as_mut() else { continue };
        if draft.saved == draft.values {
            continue;
        }
        code_style::write_values(kind, &root, &draft.values)?;
        draft.saved = draft.values.clone();
    }
    Ok(())
}

fn indent_json(i: LanguageIndent) -> Value {
    json!({ "tab_size": i.tab_size, "indent_size": i.indent_size, "use_tab": i.use_tab })
}

fn option_json(o: &code_style::StyleOption, value: Option<&Value>) -> Value {
    let mut v = json!({
        "key": o.key,
        "label": o.label,
        "default": o.default,
        "unstable": o.unstable,
        "description": o.description,
        "value": value,
    });
    match o.kind {
        code_style::OptionKind::Bool => v["kind"] = json!("bool"),
        code_style::OptionKind::Int { min, max } => {
            v["kind"] = json!("int");
            v["min"] = json!(min);
            v["max"] = json!(max);
        }
        code_style::OptionKind::Choice(c) => {
            v["kind"] = json!("choice");
            v["choices"] = json!(c);
        }
    }
    v
}

/// `codeStyle.get` (the Settings UI reads its page through this).
pub fn code_style_get(s: &mut Session, p: &Value) -> CmdResult {
    let lang = language_param(p)?;
    let label = code_style::LANGUAGES.iter().find(|(id, _)| *id == lang).map(|(_, l)| *l).unwrap_or(lang);
    let st = settings_source(s);
    let indent = code_style::indent_for_id(st, lang);
    let custom = st.indent_by_language.contains_key(lang);
    let mut out = json!({ "language": lang, "label": label, "indent": indent_json(indent), "indentCustom": custom });
    if let Some(kind) = StyleFileKind::for_language(lang) {
        let file = s.ws.project.as_ref().map(|p| kind.path(&p.root));
        let values = style_values(s, kind).map_err(CommandError::Failed)?;
        let options: Vec<Value> =
            kind.options().iter().map(|o| option_json(o, values.as_ref().and_then(|v| v.get(o.key)))).collect();
        out["file"] = json!(file);
        out["exists"] = json!(file.as_ref().is_some_and(|f| f.is_file()));
        out["options"] = json!(options);
        out["dirty"] = json!(dialog(s).and_then(|d| d.code_style.get(kind)).is_some_and(|d| d.saved != d.values));
    }
    if lang == "python" {
        out["formatter"] = json!(settings_source(s).python_formatter);
    }
    Ok(out)
}

fn code_style_set(s: &mut Session, p: &Value) -> CmdResult {
    let lang = language_param(p)?;
    let key = req_str(p, "key")?.trim().to_string();
    let value = p.get("value").cloned().unwrap_or(Value::Null);
    if matches!(key.as_str(), "tab_size" | "indent_size" | "use_tab") {
        edit(s, |st| {
            let mut i = code_style::indent_for_id(st, lang);
            match key.as_str() {
                "use_tab" => i.use_tab = value.as_bool().ok_or("use_tab: expected true or false")?,
                k => {
                    let n = value.as_u64().ok_or_else(|| format!("{k}: expected an integer"))?;
                    let n = usize::try_from(n).map_err(|e| e.to_string())?;
                    if k == "tab_size" { i.tab_size = n } else { i.indent_size = n }
                }
            }
            let mut map = st.indent_by_language.clone();
            map.insert(lang.to_string(), i);
            settings::set_value(st, "indent_by_language", &json!(map)).map(|_| ())
        })?;
        return ok();
    }
    let kind = StyleFileKind::for_language(lang)
        .ok_or_else(|| CommandError::BadParams(format!("{lang} has no formatter options; use tab_size, indent_size or use_tab")))?;
    let o = code_style::find_option(kind, &key)
        .ok_or_else(|| CommandError::BadParams(format!("unknown {lang} code style option `{key}`")))?;
    let checked = if value.is_null() { None } else { Some(code_style::check_value(o, &value).map_err(CommandError::BadParams)?) };
    let mut values = style_values(s, kind)
        .map_err(CommandError::Failed)?
        .ok_or_else(|| CommandError::Failed("open a project to change its formatter files".into()))?;
    match checked {
        Some(v) => {
            values.insert(key, v);
        }
        None => {
            values.remove(&key);
        }
    }
    store_values(s, kind, values).map_err(CommandError::Failed)?;
    ok()
}

fn code_style_reset(s: &mut Session, p: &Value) -> CmdResult {
    let lang = language_param(p)?;
    edit(s, |st| {
        let mut map = st.indent_by_language.clone();
        map.remove(lang);
        if lang == "cpp" {
            map.remove("c");
        }
        settings::set_value(st, "indent_by_language", &json!(map)).map(|_| ())
    })?;
    if let Some(kind) = StyleFileKind::for_language(lang)
        && s.ws.project.is_some()
    {
        store_values(s, kind, BTreeMap::new()).map_err(CommandError::Failed)?;
    }
    ok()
}

fn code_style_preview(s: &mut Session, p: &Value) -> CmdResult {
    let lang = language_param(p)?;
    let kind = StyleFileKind::for_language(lang)
        .ok_or_else(|| CommandError::BadParams(format!("{lang} has no formatter preview")))?;
    let values = style_values(s, kind).map_err(CommandError::Failed)?.unwrap_or_default();
    code_style::start_preview(&mut s.prefs_services.preview, kind, values);
    Ok(json!({ "started": s.prefs_services.preview.view.running, "sample": code_style::sample(kind) }))
}

// ---------------------------------------------------------------- fonts

fn fonts_list(s: &mut Session, p: &Value) -> CmdResult {
    let mono = bool_or(p, "monospaceOnly", false);
    let f = &mut s.prefs_services.fonts;
    if f.families.is_none() {
        f.start_scan();
    }
    let families: Vec<Value> = f
        .families
        .iter()
        .flatten()
        .filter(|x| !mono || x.monospace)
        .map(|x| json!({ "name": x.name, "monospace": x.monospace }))
        .collect();
    Ok(json!({
        "scanning": f.scanning,
        "families": families,
        "current": settings_source(s).editor_font_family,
        "loaded": s.prefs_services.fonts.loaded.as_ref().map(|l| &l.family),
        "error": s.prefs_services.fonts.error,
    }))
}

/// The families the Font dropdown offers (none until a scan finished).
pub fn font_families(s: &Session, monospace_only: bool) -> Vec<fonts::FontFamily> {
    s.prefs_services.fonts.families.iter().flatten().filter(|f| !monospace_only || f.monospace).cloned().collect()
}

// ---------------------------------------------------------------- live templates

fn templates_list(s: &mut Session, p: &Value) -> CmdResult {
    let lang = opt_str(p, "language").map(|l| l.trim().to_ascii_lowercase());
    let entries = s.editing.templates.entries();
    let list: Vec<Value> = entries
        .iter()
        .filter(|e| lang.as_ref().is_none_or(|l| e.template.languages.iter().any(|x| x == l || x == "any")))
        .map(|e| {
            let mut v = serde_json::to_value(&e.template).unwrap_or(Value::Null);
            v["enabled"] = json!(e.template.enabled);
            v["builtin"] = json!(e.builtin);
            v["replacesBuiltin"] = json!(e.replaces_builtin);
            v
        })
        .collect();
    Ok(json!({
        "path": s.editing.templates.path(),
        "error": s.editing.templates.error,
        "contexts": templates::CONTEXTS,
        "functions": templates::EXPRESSION_FUNCTIONS,
        "templates": list,
    }))
}

fn string_map(p: &Value, key: &str) -> Result<BTreeMap<String, String>, CommandError> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(BTreeMap::new()),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| CommandError::BadParams(format!("`{key}.{k}` must be text")))
            })
            .collect(),
        Some(_) => Err(CommandError::BadParams(format!("`{key}` must be an object"))),
    }
}

fn string_list(p: &Value, key: &str) -> Result<Vec<String>, CommandError> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|v| v.as_str().map(String::from).ok_or_else(|| CommandError::BadParams(format!("`{key}` must be strings"))))
            .collect(),
        Some(_) => Err(CommandError::BadParams(format!("`{key}` must be an array"))),
    }
}

fn templates_save(s: &mut Session, p: &Value) -> CmdResult {
    let mut t = Template::new(req_str(p, "abbreviation")?, opt_str(p, "description").unwrap_or(""), req_str(p, "text")?, &[]);
    t.languages = string_list(p, "languages")?;
    t.defaults = string_map(p, "defaults")?;
    t.expressions = string_map(p, "expressions")?;
    t.skip_if_defined = string_list(p, "skipIfDefined")?;
    t.enabled = bool_or(p, "enabled", true);
    let original = match p.get("original") {
        Some(o @ Value::Object(_)) => {
            let mut x = Template::new(req_str(o, "abbreviation")?, "", "", &[]);
            x.languages = string_list(o, "languages")?;
            Some(x)
        }
        _ => None,
    };
    let abbr = t.abbreviation.trim().to_string();
    let path = s.editing.templates.save_template(t, original.as_ref()).map_err(CommandError::BadParams)?;
    s.status = format!("Saved live template `{abbr}`");
    Ok(json!({ "path": path }))
}

// ---------------------------------------------------------------- colour scheme files

fn scheme_export(s: &mut Session, p: &Value) -> CmdResult {
    let st = settings_source(s);
    let name = opt_str(p, "name").map(String::from).unwrap_or_else(|| settings::effective_scheme(st));
    let scheme = match settings::canonical_theme(&name) {
        Some(b) => codecraft_workspace::ColorScheme { name: b.to_string(), base: b.to_string(), colors: BTreeMap::new() },
        None => st
            .user_schemes
            .iter()
            .find(|u| u.name == name)
            .cloned()
            .ok_or_else(|| CommandError::BadParams(format!("unknown colour scheme `{name}`")))?,
    };
    let Some(path) = opt_path(p, "path") else {
        let safe: String = scheme.name.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
        s.effects.push(Effect::PickFile {
            title: "Export Color Scheme".into(),
            command: "colorScheme.export".into(),
            param: "path".into(),
            save: true,
            extensions: vec!["json".into()],
            default_name: Some(format!("{safe}.json")),
        });
        // The picked path comes back as a second call without the name: keep it selected.
        return Ok(json!({ "picker": true }));
    };
    let text = scheme_io::to_json(&scheme).map_err(CommandError::Failed)?;
    codecraft_fs::write_text_file_atomic(&path, &text)
        .map_err(|e| CommandError::Failed(format!("cannot write {}: {e}", path.display())))?;
    s.status = format!("Exported colour scheme `{}` to {}", scheme.name, path.display());
    Ok(json!({ "path": path, "name": scheme.name }))
}

fn scheme_import(s: &mut Session, p: &Value) -> CmdResult {
    let Some(path) = opt_path(p, "path") else {
        s.effects.push(Effect::PickFile {
            title: "Import Color Scheme".into(),
            command: "colorScheme.import".into(),
            param: "path".into(),
            save: false,
            extensions: vec!["json".into(), "icls".into(), "xml".into()],
            default_name: None,
        });
        return Ok(json!({ "picker": true }));
    };
    let imported = scheme_io::import_file(&path).map_err(CommandError::Failed)?;
    let (mapped, skipped) = (imported.mapped, imported.skipped);
    let name = edit(s, |st| {
        let mut scheme = imported.scheme.clone();
        let taken: Vec<String> = st.user_schemes.iter().map(|u| u.name.clone()).collect();
        scheme.name = scheme_io::free_name(&scheme.name, &taken);
        let name = scheme.name.clone();
        let mut list = st.user_schemes.clone();
        list.push(scheme);
        settings::set_value(st, "user_schemes", &json!(list))?;
        settings::set_value(st, "color_scheme", &json!(name))?;
        Ok(name)
    })?;
    s.status = format!("Imported colour scheme `{name}` ({mapped} colours; {skipped} entries without a CodeCraft colour)");
    Ok(json!({ "name": name, "mapped": mapped, "skipped": skipped }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (Session, tempfile::TempDir) {
        let home = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.lsp_enabled = false;
        s.isolate_home(home.path());
        (s, home)
    }

    fn project(s: &mut Session) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        s.execute("file.open", &json!({ "path": root })).unwrap();
        (dir, root)
    }

    #[test]
    fn code_style_rust_draft_is_written_on_apply_only() {
        let (mut s, _home) = session();
        let (_dir, root) = project(&mut s);
        std::fs::write(root.join("rustfmt.toml"), "# mine\nmax_width = 120\n").unwrap();
        s.execute("app.settings", &json!({ "page": settings::PAGE_CODE_STYLE_RUST })).unwrap();
        let v = s.execute("codeStyle.get", &json!({ "language": "rust" })).unwrap();
        let mw = v["options"].as_array().unwrap().iter().find(|o| o["key"] == "max_width").unwrap();
        assert_eq!(mw["value"], 120);
        s.execute("codeStyle.set", &json!({ "language": "rust", "key": "tab_spaces", "value": 2 })).unwrap();
        s.execute("codeStyle.set", &json!({ "language": "rust", "key": "max_width", "value": null })).unwrap();
        s.execute("codeStyle.set", &json!({ "language": "rust", "key": "edition", "value": "2024" })).unwrap();
        assert_eq!(s.execute("codeStyle.get", &json!({ "language": "rust" })).unwrap()["dirty"], true);
        assert_eq!(std::fs::read_to_string(root.join("rustfmt.toml")).unwrap(), "# mine\nmax_width = 120\n", "draft only");
        for bad in [
            json!({ "language": "rust", "key": "max_width", "value": 1 }),
            json!({ "language": "rust", "key": "nope", "value": 1 }),
            json!({ "language": "cobol", "key": "x", "value": 1 }),
            json!({ "language": "python", "key": "max_width", "value": 80 }),
        ] {
            assert!(matches!(s.execute("codeStyle.set", &bad), Err(CommandError::BadParams(_))), "{bad}");
        }
        s.execute("settings.dialog.ok", &Value::Null).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("rustfmt.toml")).unwrap(),
            "# mine\ntab_spaces = 2\nedition = \"2024\"\n"
        );
        // Cancel drops a draft.
        s.execute("app.settings", &Value::Null).unwrap();
        s.execute("codeStyle.set", &json!({ "language": "rust", "key": "hard_tabs", "value": true })).unwrap();
        s.execute("settings.dialog.cancel", &Value::Null).unwrap();
        assert!(!std::fs::read_to_string(root.join("rustfmt.toml")).unwrap().contains("hard_tabs"));
        // Without the dialog the file is written at once.
        s.execute("codeStyle.set", &json!({ "language": "c", "key": "IndentWidth", "value": 3 })).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(".clang-format")).unwrap(), "IndentWidth: 3\n");
        s.execute("codeStyle.reset", &json!({ "language": "cpp" })).unwrap();
        assert_eq!(std::fs::read_to_string(root.join(".clang-format")).unwrap(), "");
    }

    #[test]
    fn code_style_indent_per_language_drives_the_editor() {
        let (mut s, _home) = session();
        let (_dir, root) = project(&mut s);
        std::fs::write(root.join("a.py"), "x = 1\n").unwrap();
        s.execute("codeStyle.set", &json!({ "language": "python", "key": "indent_size", "value": 2 })).unwrap();
        s.execute("codeStyle.set", &json!({ "language": "python", "key": "tab_size", "value": 8 })).unwrap();
        let v = s.execute("codeStyle.get", &json!({ "language": "python" })).unwrap();
        assert_eq!(v["indent"], json!({ "tab_size": 8, "indent_size": 2, "use_tab": false }));
        assert_eq!(v["indentCustom"], true);
        assert_eq!(v["formatter"], "auto");
        s.execute("file.open", &json!({ "path": root.join("a.py") })).unwrap();
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "  x = 1\n", "Tab indents by the Python indent");
        assert_eq!(s.active_tab_width(), 8);
        assert!(s.execute("codeStyle.set", &json!({ "language": "python", "key": "indent_size", "value": 0 })).is_err());
        s.execute("codeStyle.set", &json!({ "language": "python", "key": "use_tab", "value": true })).unwrap();
        s.execute("codeStyle.reset", &json!({ "language": "python" })).unwrap();
        assert!(s.ws.settings.indent_by_language.is_empty());
    }

    #[test]
    fn live_templates_are_edited_and_expand_with_expressions() {
        let (mut s, home) = session();
        let (_dir, root) = project(&mut s);
        let r = s
            .execute(
                "liveTemplates.save",
                &json!({
                    "abbreviation": "st",
                    "description": "Struct named after the file",
                    "text": "struct $NAME$ {\n\t$END$\n}",
                    "languages": ["rust"],
                    "expressions": { "NAME": "capitalize(camelCase(fileNameWithoutExtension()))" },
                    "skipIfDefined": ["NAME"],
                }),
            )
            .unwrap();
        assert_eq!(r["path"], json!(home.path().join("templates.json")));
        let list = s.execute("liveTemplates.list", &json!({ "language": "rust" })).unwrap();
        let st = list["templates"].as_array().unwrap().iter().find(|t| t["abbreviation"] == "st").unwrap();
        assert_eq!(st["replacesBuiltin"], true);
        std::fs::write(root.join("my_shape.rs"), "").unwrap();
        s.execute("file.open", &json!({ "path": root.join("my_shape.rs") })).unwrap();
        s.execute("editor.type", &json!({ "text": "st" })).unwrap();
        s.execute("editor.tab", &Value::Null).unwrap();
        assert_eq!(s.ws.active().unwrap().1.doc.text(), "struct MyShape {\n    \n}");
        // The NAME stop is skipped: the caret is at $END$.
        let caret = s.ws.active().unwrap().0.selections.primary().head;
        assert_eq!(caret, "struct MyShape {\n    ".chars().count());
        // Bad input is rejected; delete and restore.
        assert!(s.execute("liveTemplates.save", &json!({ "abbreviation": "x y", "text": "a", "languages": ["rust"] })).is_err());
        assert!(s.execute("liveTemplates.save", &json!({ "abbreviation": "x", "text": "a", "languages": "rust" })).is_err());
        s.execute("liveTemplates.delete", &json!({ "abbreviation": "st", "language": "rust" })).unwrap();
        s.execute("liveTemplates.setEnabled", &json!({ "abbreviation": "fn", "language": "rust", "enabled": false })).unwrap();
        let list = s.execute("liveTemplates.list", &json!({ "language": "rust" })).unwrap();
        let fns: Vec<&Value> = list["templates"].as_array().unwrap().iter().filter(|t| t["abbreviation"] == "fn").collect();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0]["enabled"], false);
        s.execute("liveTemplates.restoreDefaults", &Value::Null).unwrap();
        assert!(s.editing.templates.user_templates().is_empty());
    }

    #[test]
    fn color_schemes_export_and_import() {
        let (mut s, home) = session();
        s.execute("colorScheme.duplicate", &json!({ "name": "Mine" })).unwrap();
        s.execute("colorScheme.setColor", &json!({ "token": "keyword", "color": "#123456" })).unwrap();
        let out = home.path().join("mine.json");
        s.execute("colorScheme.export", &json!({ "path": out })).unwrap();
        let r = s.execute("colorScheme.import", &json!({ "path": out })).unwrap();
        assert_eq!(r["name"], "Mine (2)");
        assert_eq!(s.ws.settings.color_scheme, "Mine (2)");
        assert_eq!(s.ws.settings.user_schemes[1].colors.get("keyword").map(String::as_str), Some("#123456"));
        let icls = home.path().join("night.icls");
        std::fs::write(
            &icls,
            r#"<scheme name="Night" parent_scheme="Darcula"><attributes><option name="DEFAULT_STRING"><value><option name="FOREGROUND" value="6a8759"/></value></option></attributes></scheme>"#,
        )
        .unwrap();
        // Inside the dialog it goes to the draft.
        s.execute("app.settings", &Value::Null).unwrap();
        let r = s.execute("colorScheme.import", &json!({ "path": icls })).unwrap();
        assert_eq!((r["name"].as_str(), r["mapped"].as_u64()), (Some("Night"), Some(1)));
        assert_eq!(s.ws.settings.user_schemes.len(), 2, "not applied yet");
        s.execute("settings.dialog.ok", &Value::Null).unwrap();
        let night = s.ws.settings.user_schemes.iter().find(|u| u.name == "Night").unwrap();
        assert_eq!(night.colors.get("string").map(String::as_str), Some("#6A8759"));
        // Without a path, a file dialog is asked for.
        assert_eq!(s.execute("colorScheme.import", &Value::Null).unwrap()["picker"], true);
        assert!(matches!(s.effects.last(), Some(Effect::PickFile { .. })));
        assert!(s.execute("colorScheme.import", &json!({ "path": home.path().join("missing.icls") })).is_err());
        assert!(s.execute("colorScheme.export", &json!({ "name": "Nope", "path": out })).is_err());
    }

    #[test]
    fn keymap_copies_inherit_and_override() {
        let (mut s, _home) = session();
        s.execute("keymap.select", &json!({ "name": "macOS" })).unwrap();
        let r = s.execute("keymap.duplicate", &json!({})).unwrap();
        assert_eq!(r["name"], "macOS copy");
        let id = "help.findAction";
        assert_eq!(crate::keymaps::shortcuts(&s.ws.settings, id), vec!["Shift+Cmd+A"], "inherited");
        s.execute("keymap.addShortcut", &json!({ "id": id, "shortcut": "Cmd+K Cmd+A" })).unwrap();
        assert_eq!(crate::keymaps::shortcuts(&s.ws.settings, id), vec!["Shift+Cmd+A", "Cmd+K Cmd+A"]);
        // The parent is untouched; a child of the copy inherits the copy's change.
        s.execute("keymap.duplicate", &json!({ "name": "Child" })).unwrap();
        assert_eq!(crate::keymaps::shortcuts(&s.ws.settings, id), vec!["Shift+Cmd+A", "Cmd+K Cmd+A"]);
        let list = s.execute("keymap.list", &json!({ "query": "find action" })).unwrap();
        assert_eq!(list["parent"], "macOS copy");
        s.execute("keymap.select", &json!({ "name": "macOS" })).unwrap();
        assert_eq!(crate::keymaps::shortcuts(&s.ws.settings, id), vec!["Shift+Cmd+A"]);
        s.execute("keymap.rename", &json!({ "name": "macOS copy", "newName": "Mine" })).unwrap();
        s.execute("keymap.select", &json!({ "name": "Child" })).unwrap();
        assert_eq!(crate::keymaps::parent_of(&s.ws.settings, "Child").as_deref(), Some("Mine"));
        s.execute("keymap.delete", &json!({ "name": "Mine" })).unwrap();
        assert_eq!(crate::keymaps::shortcuts(&s.ws.settings, id), vec!["Shift+Cmd+A"], "re-parented to macOS");
        assert!(s.execute("keymap.delete", &json!({ "name": "macOS" })).is_err(), "built-ins cannot be deleted");
        assert!(s.execute("keymap.rename", &json!({ "name": "Child", "newName": "Emacs" })).is_err());
    }

    #[test]
    fn fonts_list_scans_fixture_dirs_only() {
        let (mut s, home) = session();
        let fonts_dir = home.path().join("fonts");
        std::fs::create_dir_all(&fonts_dir).unwrap();
        std::fs::write(fonts_dir.join("a.ttf"), fonts::tests::synthetic_font("Fixture Mono", "Regular", true, 400)).unwrap();
        std::fs::write(fonts_dir.join("b.ttf"), fonts::tests::synthetic_font("Fixture Sans", "Regular", false, 400)).unwrap();
        std::fs::write(fonts_dir.join("broken.ttf"), [0u8; 3000]).unwrap();
        let start = std::time::Instant::now();
        let v = loop {
            let v = s.execute("fonts.list", &json!({ "monospaceOnly": true })).unwrap();
            s.pump();
            if v["scanning"] == false && v["families"].as_array().is_some_and(|a| !a.is_empty()) {
                break v;
            }
            assert!(start.elapsed() < std::time::Duration::from_secs(10), "{v}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert_eq!(v["families"], json!([{ "name": "Fixture Mono", "monospace": true }]));
        s.execute("settings.set", &json!({ "key": "editor_font_family", "value": " Fixture Mono " })).unwrap();
        assert_eq!(s.ws.settings.editor_font_family, "Fixture Mono");
        let start = std::time::Instant::now();
        while s.prefs_services.fonts.loaded.is_none() && start.elapsed() < std::time::Duration::from_secs(10) {
            s.pump();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(s.prefs_services.fonts.loaded.as_ref().map(|l| l.family.as_str()), Some("Fixture Mono"));
        assert!(s.execute("settings.set", &json!({ "key": "editor_font_family", "value": "bad\u{0}" })).is_err());
    }

    #[test]
    fn crates_io_toggle_follows_the_setting_only_where_allowed() {
        let (mut s, _home) = session();
        s.execute("settings.set", &json!({ "key": "crates_io_lookups", "value": false })).unwrap();
        s.execute("settings.set", &json!({ "key": "crates_io_lookups", "value": true })).unwrap();
        assert!(!s.rust.deps.online, "tests never go online");
        s.prefs_services.network_lookups = true;
        s.execute("settings.set", &json!({ "key": "crates_io_lookups", "value": false })).unwrap();
        assert!(!s.rust.deps.online);
        s.execute("rust.cargo.setCratesIoLookups", &json!({ "enabled": false })).unwrap();
        assert!(!s.ws.settings.crates_io_lookups);
    }

    #[test]
    fn django_template_is_a_file_type_to_associate() {
        let (mut s, _home) = session();
        let (_dir, root) = project(&mut s);
        std::fs::write(root.join("page.tmpl"), "{% block x %}{{ y }}{% endblock %}\n").unwrap();
        s.execute("file.open", &json!({ "path": root.join("page.tmpl") })).unwrap();
        s.execute("file.fileProperties.associateWithFileType", &json!({ "language": "Django Template" })).unwrap();
        assert_eq!(s.ws.settings.file_type_associations.get("*.tmpl").map(String::as_str), Some("Django Template"));
        assert_eq!(s.ws.active().unwrap().1.language, codecraft_syntax::Language::DjangoTemplate);
    }

    #[test]
    fn analysis_levels_persist_per_project() {
        let (mut s, _home) = session();
        let (_dir, root) = project(&mut s);
        s.pump();
        s.execute("file.open", &json!({ "path": root.join("main.rs") })).unwrap();
        s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({ "level": "syntax" })).unwrap();
        let text = std::fs::read_to_string(root.join(".codecraft/analysis.json")).unwrap();
        assert!(text.contains("\"main.rs\": \"Syntax\""), "{text}");
        // Reopening the project loads it.
        s.execute("file.closeProject", &Value::Null).ok();
        s.ws.analysis_levels.clear();
        s.pump();
        s.execute("file.open", &json!({ "path": root })).unwrap();
        s.pump();
        assert_eq!(s.ws.analysis_level(&root.join("main.rs")), codecraft_workspace::AnalysisLevel::Syntax);
        s.execute("file.open", &json!({ "path": root.join("main.rs") })).unwrap();
        s.execute("code.analyzeCode.configureCurrentFileAnalysis", &json!({ "level": "all" })).unwrap();
        assert!(!root.join(".codecraft/analysis.json").exists(), "nothing custom left: the file goes");
        // A corrupt file is ignored.
        std::fs::create_dir_all(root.join(".codecraft")).unwrap();
        std::fs::write(root.join(".codecraft/analysis.json"), "{ nope").unwrap();
        assert!(settings::analysis_levels::load(&root).is_empty());
        std::fs::write(root.join(".codecraft/analysis.json"), r#"{"../escape.rs": "None", "/abs.rs": "None", "ok.rs": "None"}"#).unwrap();
        assert_eq!(settings::analysis_levels::load(&root).len(), 1);
    }
}
