//! Django / Jinja2 templates (M12 track PY): Go to Declaration from `{% extends "x.html" %}`,
//! `{% include %}`, `{% import %}`, `{% from %}` and `{% embed %}` to the template file, and
//! completion of block tags (`{% bl` → `block`, `{% end` → the end tag of the innermost open
//! block first) and filters (`{{ x|up` → `upper`). Highlighting, folding and the outline are in
//! [`codecraft_syntax::django`].
//!
//! Template names resolve against every `templates` directory of the project (the one holding
//! the current file first, as Django's app directories and Flask/Jinja loaders do), then the
//! current file's directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde_json::{Value, json};

use codecraft_syntax::Language;
use codecraft_syntax::django::{self, CompletionKind};

use crate::commands::{CmdResult, CommandSpec, Context, Enabled, Run, always, spec};
use crate::popup::{CompletionItem, CompletionPopup};
use crate::{CommandError, Popup, Session};

/// Most `templates` directories searched.
const MAX_TEMPLATE_DIRS: usize = 200;

fn template_doc(s: &Session) -> bool {
    s.ws.active().is_some_and(|(_, d)| d.language == Language::DjangoTemplate)
}

/// The `templates` directories a name may resolve against, best first.
pub fn template_dirs(s: &Session, current: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let push = |d: PathBuf, dirs: &mut Vec<PathBuf>| {
        if !dirs.contains(&d) && dirs.len() < MAX_TEMPLATE_DIRS {
            dirs.push(d);
        }
    };
    if let Some(cur) = current {
        for a in cur.ancestors().skip(1) {
            if a.file_name().is_some_and(|n| n == "templates") {
                push(a.to_path_buf(), &mut dirs);
                break;
            }
        }
    }
    if let Some(p) = &s.ws.project {
        for (_, node) in p.tree.iter() {
            if node.is_dir && node.name == "templates" {
                push(node.path.clone(), &mut dirs);
            }
        }
        push(p.root.join("templates"), &mut dirs);
    }
    if let Some(dir) = current.and_then(Path::parent) {
        push(dir.to_path_buf(), &mut dirs);
    }
    dirs
}

/// The file a template name refers to.
pub fn resolve(s: &Session, current: Option<&Path>, name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.len() > 1000 || name.contains('\0') {
        return None;
    }
    let rel = Path::new(name.trim_start_matches('/'));
    template_dirs(s, current).into_iter().map(|d| d.join(rel)).find(|p| p.is_file())
}

/// Go to the template referenced at the caret (or `name`).
fn navigate(s: &mut Session, name: Option<String>) -> CmdResult {
    let (current, name) = {
        let (e, d) = s.ws.active().ok_or_else(|| CommandError::Disabled("no editor is open".into()))?;
        let name = match name {
            Some(n) => n,
            None => {
                let caret = e.selections.primary().head;
                django::template_reference_at(&d.doc.text(), caret)
                    .map(|r| r.name)
                    .ok_or_else(|| CommandError::Failed("no template name at the caret".into()))?
            }
        };
        (d.path.clone(), name)
    };
    let path = resolve(s, current.as_deref(), &name)
        .ok_or_else(|| CommandError::Failed(format!("template `{name}` not found in the project's templates directories")))?;
    s.navigate_to(&path, 0)?;
    Ok(json!({ "path": path }))
}

fn item(label: &str, insert: String, detail: Option<&str>, rank: u8, start: usize, prefix: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        detail: detail.map(str::to_string),
        kind: "keyword".into(),
        filter_text: label.get(prefix.len()..).filter(|_| label.starts_with(prefix)).unwrap_or(label).to_string(),
        sort_text: format!("{rank}{label}"),
        insert_text: insert,
        replace_start: Some(start),
        resolved: true,
        ..CompletionItem::default()
    }
}

/// Completion items at `offset` of a template: block tags, end tags, filters.
pub fn completions(text: &str, offset: usize) -> Option<(usize, Vec<CompletionItem>)> {
    let ctx = django::completion_context(text, offset)?;
    let prefix = ctx.prefix.as_str();
    let mut items = Vec::new();
    match ctx.kind {
        CompletionKind::Tag | CompletionKind::EndTag => {
            let open = django::open_blocks(text, offset);
            if let Some(inner) = open.last() {
                let end = format!("end{inner}");
                if end.starts_with(prefix) {
                    items.push(item(&end, end.clone(), Some("closes the open block"), 0, ctx.start, prefix));
                }
            }
            for (name, desc) in django::TAGS {
                if name.starts_with(prefix) && !items.iter().any(|i| i.label == *name) {
                    let rank = if name.starts_with("end") { 2 } else { 1 };
                    items.push(item(name, (*name).to_string(), Some(desc), rank, ctx.start, prefix));
                }
            }
        }
        CompletionKind::Filter => {
            for f in django::FILTERS {
                if f.starts_with(prefix) {
                    items.push(item(f, (*f).to_string(), Some("filter"), 1, ctx.start, prefix));
                }
            }
        }
    }
    Some((ctx.start, items))
}

fn complete(s: &mut Session) -> CmdResult {
    let Some((e, d)) = s.ws.active() else { return Err(CommandError::Disabled("no editor is open".into())) };
    let editor = e.id;
    let caret = e.selections.primary().head.min(d.doc.len_chars());
    let Some((anchor, items)) = completions(&d.doc.text(), caret).filter(|(_, i)| !i.is_empty()) else {
        s.status = "No suggestions".into();
        return Ok(json!({ "items": 0 }));
    };
    let n = items.len();
    s.popup = Some(Popup::Completion(CompletionPopup { editor, anchor, items, ..CompletionPopup::default() }));
    s.refresh_completion_filter();
    Ok(json!({ "items": n }))
}

// ---------------------------------------------------------------- routing

type Pair = (Enabled, Run);
static ORIGINAL: OnceLock<HashMap<&'static str, Pair>> = OnceLock::new();

const DECLARATION: &str = "navigate.declarationOrUsages";
const COMPLETION: &str = "code.codeCompletion.basic";

/// Make Go to Declaration and Basic Completion handle templates.
pub(crate) fn route(specs: &mut [CommandSpec]) {
    let mut orig = HashMap::new();
    for spec in specs.iter_mut() {
        let (enabled, run): Pair = match spec.id {
            DECLARATION => (declaration_enabled, declaration_run),
            COMPLETION => (completion_enabled, completion_run),
            _ => continue,
        };
        orig.insert(spec.id, (spec.enabled, spec.run));
        spec.enabled = enabled;
        spec.run = run;
    }
    let _ = ORIGINAL.set(orig);
}

fn orig_enabled(id: &str, s: &Session) -> Result<(), String> {
    match ORIGINAL.get().and_then(|m| m.get(id)) {
        Some((e, _)) => e(s),
        None => Err("not available".into()),
    }
}

fn orig_run(id: &str, s: &mut Session, p: &Value) -> CmdResult {
    match ORIGINAL.get().and_then(|m| m.get(id)) {
        Some((_, r)) => r(s, p),
        None => Err(CommandError::Unknown(id.to_string())),
    }
}

fn declaration_enabled(s: &Session) -> Result<(), String> {
    if template_doc(s) { Ok(()) } else { orig_enabled(DECLARATION, s) }
}

fn declaration_run(s: &mut Session, p: &Value) -> CmdResult {
    if template_doc(s) { navigate(s, None) } else { orig_run(DECLARATION, s, p) }
}

fn completion_enabled(s: &Session) -> Result<(), String> {
    if template_doc(s) { Ok(()) } else { orig_enabled(COMPLETION, s) }
}

fn completion_run(s: &mut Session, p: &Value) -> CmdResult {
    if template_doc(s) { complete(s) } else { orig_run(COMPLETION, s, p) }
}

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![spec(
        "django.goToTemplate",
        "Go to Template",
        None,
        "{name?: string} — open the template named at the caret (`{% extends/include \"x.html\" %}`) or `name`, \
         searched in the project's templates directories",
        Context::Global,
        always,
        |s, p| {
            let name = match p.get("name") {
                Some(Value::String(n)) => Some(n.clone()),
                None | Some(Value::Null) => None,
                Some(_) => return Err(CommandError::BadParams("`name` must be a string".into())),
            };
            navigate(s, name)
        },
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_and_filter_completions() {
        let text = "{% block content %}{% for x in xs %}{% en";
        let (start, items) = completions(text, text.chars().count()).unwrap();
        assert_eq!(start, text.chars().count() - 2);
        assert_eq!(items[0].label, "endfor", "the innermost open block's end tag comes first");
        assert!(items.iter().all(|i| i.label.starts_with("en")));
        let text = "{% bl";
        let (_, items) = completions(text, 5).unwrap();
        assert!(items.iter().any(|i| i.label == "block"));
        let text = "{{ name|up";
        let (_, items) = completions(text, text.len()).unwrap();
        assert!(items.iter().any(|i| i.label == "upper"));
        assert!(completions("plain text", 3).is_none());
        assert!(completions("", 99).is_none());
    }
}
