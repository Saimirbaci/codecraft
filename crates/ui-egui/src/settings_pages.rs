//! Settings pages added in M13 track ST: Editor › Font (family chooser), Code Style per
//! language (rustfmt / clang-format options with a preview, indents), Live Templates (editable)
//! and TODO (patterns and filters). Thin: every change is an engine command (`settings.set`,
//! `codeStyle.*`, `liveTemplates.*`); reads go through the engine's state.

use std::collections::BTreeMap;

use egui::{FontId, RichText, ScrollArea, TextEdit};
use serde_json::{Value, json};

use codecraft_engine::settings::{self, code_style};
use codecraft_engine::templates::{CONTEXTS, EXPRESSION_FUNCTIONS, Template};
use codecraft_engine::{Session, prefs_cmds};
use codecraft_workspace::{Settings, TodoFilter, TodoPattern};

use crate::UiState;
use crate::settings_ui::{Cmds, choice, setting_grid};

/// View state of these pages.
#[derive(Default)]
pub struct PagesUi {
    /// The live template being edited (a new one, or a copy of a list row).
    template: Option<TemplateForm>,
    template_filter: String,
    /// TODO › Patterns: the pattern being typed and whether it is case-sensitive.
    new_pattern: String,
    new_pattern_case: bool,
    new_filter: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TemplateForm {
    /// The template being replaced (abbreviation and contexts), `None` for a new one.
    original: Option<(String, Vec<String>)>,
    abbreviation: String,
    description: String,
    text: String,
    languages: Vec<String>,
    /// Variable → (expression, default, skip if defined).
    vars: BTreeMap<String, (String, String, bool)>,
    enabled: bool,
    error: Option<String>,
}

impl TemplateForm {
    fn from(t: &Template) -> Self {
        let mut vars = BTreeMap::new();
        for v in t.variables() {
            let e = t.expressions.get(&v).cloned().unwrap_or_default();
            let d = t.defaults.get(&v).cloned().unwrap_or_default();
            vars.insert(v.clone(), (e, d, t.skip_if_defined.contains(&v)));
        }
        Self {
            original: Some((t.abbreviation.clone(), t.languages.clone())),
            abbreviation: t.abbreviation.clone(),
            description: t.description.clone(),
            text: t.text.clone(),
            languages: t.languages.clone(),
            vars,
            enabled: t.enabled,
            error: None,
        }
    }

    fn params(&self) -> Value {
        let names = Template::new("x", "", &self.text, &[]).variables();
        let mut expressions = serde_json::Map::new();
        let mut defaults = serde_json::Map::new();
        let mut skip = Vec::new();
        for n in &names {
            let Some((e, d, s)) = self.vars.get(n) else { continue };
            if !e.trim().is_empty() {
                expressions.insert(n.clone(), json!(e));
            }
            if !d.is_empty() {
                defaults.insert(n.clone(), json!(d));
            }
            if *s {
                skip.push(n.clone());
            }
        }
        let mut p = json!({
            "abbreviation": self.abbreviation,
            "description": self.description,
            "text": self.text,
            "languages": self.languages,
            "expressions": expressions,
            "defaults": defaults,
            "skipIfDefined": skip,
            "enabled": self.enabled,
        });
        if let Some((a, l)) = &self.original {
            p["original"] = json!({ "abbreviation": a, "languages": l });
        }
        p
    }
}

fn muted(st: &UiState, t: impl Into<String>) -> RichText {
    RichText::new(t.into()).color(st.theme.text_muted)
}

// ---------------------------------------------------------------- font

pub(crate) fn font_page(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, draft: &Settings, cmds: &mut Cmds) {
    let fonts = &mut s.prefs_services.fonts;
    if fonts.families.is_none() && !fonts.scanning {
        fonts.start_scan();
    }
    let families = prefs_cmds::font_families(s, !st.prefs.all_fonts);
    let current = draft.editor_font_family.clone();
    let default_label = format!("Default ({})", if st.mono_font_name.is_empty() { "built-in" } else { &st.mono_font_name });
    egui::Grid::new("font-family-grid").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
        ui.label("Font").on_hover_text("The editor font; characters it lacks come from the default fonts");
        ui.horizontal(|ui| {
            let shown = if current.is_empty() { default_label.clone() } else { current.clone() };
            let mut chosen = current.clone();
            egui::ComboBox::from_id_salt("editor-font-family").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut chosen, String::new(), default_label.as_str());
                ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for f in &families {
                        ui.selectable_value(&mut chosen, f.name.clone(), f.name.as_str());
                    }
                });
            });
            if chosen != current {
                cmds.push(("settings.set", json!({ "key": "editor_font_family", "value": chosen })));
            }
            if s.prefs_services.fonts.scanning {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
        ui.end_row();
        ui.label("");
        ui.checkbox(&mut st.prefs.all_fonts, "Show non-monospaced fonts");
        ui.end_row();
    });
    if let Some(e) = &s.prefs_services.fonts.error {
        ui.label(RichText::new(e).color(st.theme.warning));
    }
    if !current.is_empty() && !s.prefs_services.fonts.scanning && s.prefs_services.fonts.family(&current).is_none() {
        ui.label(RichText::new(format!("`{current}` is not installed; the default font is used.")).color(st.theme.warning));
    }
    ui.add_space(6.0);
    setting_grid(ui, st, settings::PAGE_FONT, draft, "", cmds);
    ui.add_space(8.0);
    let size = draft.editor_font_size.clamp(6.0, 72.0);
    egui::Frame::new().fill(st.theme.editor_bg).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = (size * (draft.line_spacing - 1.0)).max(0.0);
        for line in ["fn main() {", "    let answer = 42; // 0O il1 {}[]", "}"] {
            ui.label(RichText::new(line).font(FontId::monospace(size)).color(st.theme.text_editor));
        }
    });
    ui.label(muted(st, format!("Editor font in use: {}", st.mono_font_name)).size(11.0));
}

// ---------------------------------------------------------------- code style

fn indent_row(ui: &mut egui::Ui, lang: &str, indent: &Value, cmds: &mut Cmds) {
    let mut tab = indent["tab_size"].as_i64().unwrap_or(4);
    let mut size = indent["indent_size"].as_i64().unwrap_or(4);
    let mut use_tab = indent["use_tab"].as_bool().unwrap_or(false);
    if ui.add(egui::DragValue::new(&mut tab).range(1..=16).prefix("Tab size: ")).changed() {
        cmds.push(("codeStyle.set", json!({ "language": lang, "key": "tab_size", "value": tab })));
    }
    if ui.add(egui::DragValue::new(&mut size).range(1..=16).prefix("Indent: ")).changed() {
        cmds.push(("codeStyle.set", json!({ "language": lang, "key": "indent_size", "value": size })));
    }
    if ui.checkbox(&mut use_tab, "Use tab character").changed() {
        cmds.push(("codeStyle.set", json!({ "language": lang, "key": "use_tab", "value": use_tab })));
    }
}

fn option_widget(ui: &mut egui::Ui, lang: &str, o: &Value, cmds: &mut Cmds) {
    let key = o["key"].as_str().unwrap_or_default().to_string();
    let default = o["default"].as_str().unwrap_or_default().to_string();
    let value = &o["value"];
    let set = |cmds: &mut Cmds, v: Value| cmds.push(("codeStyle.set", json!({ "language": lang, "key": key, "value": v })));
    match o["kind"].as_str() {
        Some("bool") => {
            let mut on = value.as_bool().unwrap_or(default == "true");
            if ui.checkbox(&mut on, "").changed() {
                set(cmds, json!(on));
            }
        }
        Some("int") => {
            let (min, max) = (o["min"].as_i64().unwrap_or(0), o["max"].as_i64().unwrap_or(1000));
            let mut n = value.as_i64().unwrap_or_else(|| default.parse().unwrap_or(min));
            if ui.add(egui::DragValue::new(&mut n).range(min..=max)).changed() {
                set(cmds, json!(n));
            }
        }
        _ => {
            let choices: Vec<String> =
                o["choices"].as_array().map(|a| a.iter().filter_map(|c| c.as_str().map(String::from)).collect()).unwrap_or_default();
            let cur = value.as_str().map(String::from).unwrap_or(default.clone());
            if let Some(v) = choice(ui, &format!("{lang}-{key}"), &cur, &choices) {
                set(cmds, v);
            }
        }
    }
    if !value.is_null() && ui.small_button("Reset").on_hover_text(format!("Remove from the file (default: {default})")).clicked() {
        set(cmds, Value::Null);
    }
}

pub(crate) fn code_style_page(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, lang: &str, cmds: &mut Cmds) {
    let view = match prefs_cmds::code_style_get(s, &json!({ "language": lang })) {
        Ok(v) => v,
        Err(e) => {
            ui.label(RichText::new(e.to_string()).color(st.theme.error));
            return;
        }
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("Editor").strong());
        indent_row(ui, lang, &view["indent"], cmds);
        if ui.button("Reset").on_hover_text("Use the general tab size and remove the formatter options").clicked() {
            cmds.push(("codeStyle.reset", json!({ "language": lang })));
        }
    });
    let Some(options) = view["options"].as_array() else { return };
    ui.add_space(8.0);
    match view["file"].as_str() {
        Some(f) => {
            let state = if view["exists"] == true { "" } else { " (created on Apply)" };
            ui.label(muted(st, format!("Formatter options in {f}{state}")));
        }
        None => {
            ui.label(muted(st, "Open a project to edit its formatter options."));
            return;
        }
    }
    egui::Grid::new(("code-style-options", lang)).num_columns(2).spacing([18.0, 6.0]).striped(true).show(ui, |ui| {
        for o in options {
            let label = o["label"].as_str().unwrap_or_default();
            let unstable = o["unstable"] == true;
            let text = if unstable { format!("{label} (nightly)") } else { label.to_string() };
            let tip = format!("{} — `{}`", o["description"].as_str().unwrap_or_default(), o["key"].as_str().unwrap_or_default());
            ui.label(text).on_hover_text(tip);
            ui.horizontal(|ui| option_widget(ui, lang, o, cmds));
            ui.end_row();
        }
    });
    ui.add_space(8.0);
    let preview = s.prefs_services.preview.view.clone();
    ui.horizontal(|ui| {
        let tool = if lang == "rust" { "rustfmt" } else { "clang-format" };
        if ui.button("Preview").on_hover_text(format!("Format a sample with {tool} and these options")).clicked() {
            cmds.push(("codeStyle.preview", json!({ "language": lang })));
        }
        if preview.running {
            ui.add(egui::Spinner::new().size(12.0));
        }
    });
    let kind = code_style::StyleFileKind::for_language(lang);
    if preview.kind.is_some() && preview.kind == kind {
        if let Some(e) = &preview.error {
            ui.label(RichText::new(e).color(st.theme.warning));
        }
        if let Some(text) = &preview.text {
            egui::Frame::new().fill(st.theme.editor_bg).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
                ScrollArea::both().id_salt(("code-style-preview", lang)).max_height(260.0).show(ui, |ui| {
                    ui.label(RichText::new(text).font(FontId::monospace(12.0)).color(st.theme.text_editor));
                });
            });
        }
    }
}

pub(crate) fn code_style_other_page(ui: &mut egui::Ui, st: &UiState, draft: &Settings, cmds: &mut Cmds) {
    ui.label(muted(st, "Tab and indent per language (the file's own indentation wins when it has some)."));
    ui.add_space(4.0);
    egui::Grid::new("code-style-other").num_columns(3).spacing([14.0, 6.0]).striped(true).show(ui, |ui| {
        for (id, label) in code_style::LANGUAGES.iter().filter(|(id, _)| !matches!(*id, "rust" | "cpp" | "python")) {
            let i = code_style::indent_for_id(draft, id);
            ui.label(*label);
            ui.horizontal(|ui| {
                indent_row(ui, id, &json!({ "tab_size": i.tab_size, "indent_size": i.indent_size, "use_tab": i.use_tab }), cmds);
            });
            if draft.indent_by_language.contains_key(*id) {
                if ui.small_button("Reset").clicked() {
                    cmds.push(("codeStyle.reset", json!({ "language": id })));
                }
            } else {
                ui.label("");
            }
            ui.end_row();
        }
    });
}

// ---------------------------------------------------------------- live templates

pub(crate) fn templates_page(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState, cmds: &mut Cmds) {
    let theme = st.theme.clone();
    let path = s.editing.templates.path();
    ui.label(muted(
        st,
        match &path {
            Some(p) => format!("Changes are saved at once to {}.", p.display()),
            None => "User templates: set CODECRAFT_HOME to store them.".into(),
        },
    ));
    if let Some(e) = &s.editing.templates.error {
        ui.label(RichText::new(e).color(theme.error));
    }
    ui.horizontal(|ui| {
        if ui.button("+ Add").clicked() {
            st.prefs.pages.template = Some(TemplateForm {
                languages: vec!["rust".into()],
                text: "$END$".into(),
                enabled: true,
                ..TemplateForm::default()
            });
        }
        if ui.button("Restore Defaults").on_hover_text("Undo changes to the built-in templates").clicked() {
            cmds.push(("liveTemplates.restoreDefaults", Value::Null));
        }
        ui.add(TextEdit::singleline(&mut st.prefs.pages.template_filter).hint_text("🔍 Filter").desired_width(200.0));
    });
    ui.add_space(4.0);
    let entries = s.editing.templates.entries();
    let q = st.prefs.pages.template_filter.trim().to_lowercase();
    let selected = st.prefs.pages.template.as_ref().and_then(|f| f.original.clone());
    ScrollArea::vertical().id_salt("templates-list").max_height(220.0).show(ui, |ui| {
        egui::Grid::new("templates-grid").num_columns(4).spacing([14.0, 4.0]).striped(true).show(ui, |ui| {
            for e in entries.iter().filter(|e| {
                q.is_empty()
                    || e.template.abbreviation.to_lowercase().contains(&q)
                    || e.template.description.to_lowercase().contains(&q)
                    || e.template.languages.iter().any(|l| l.contains(&q))
            }) {
                let t = &e.template;
                let lang = t.languages.first().cloned().unwrap_or_default();
                let mut on = t.enabled;
                if ui.checkbox(&mut on, "").on_hover_text("Offer this template").changed() {
                    cmds.push((
                        "liveTemplates.setEnabled",
                        json!({ "abbreviation": t.abbreviation, "language": lang, "enabled": on }),
                    ));
                }
                let color = if e.builtin { theme.text } else { theme.link };
                let is_sel = selected.as_ref().is_some_and(|(a, l)| *a == t.abbreviation && *l == t.languages);
                let abbr = egui::Button::selectable(is_sel, RichText::new(&t.abbreviation).monospace().color(color));
                if ui.add(abbr).on_hover_text(&t.text).clicked() {
                    st.prefs.pages.template = Some(TemplateForm::from(t));
                }
                ui.label(&t.description);
                ui.label(RichText::new(t.languages.join(", ")).color(theme.text_muted));
                ui.end_row();
            }
        });
    });
    let Some(mut form) = st.prefs.pages.template.clone() else {
        ui.label(muted(st, "Select a template to edit it, or Add a new one."));
        return;
    };
    ui.separator();
    let mut save = false;
    let mut delete = false;
    let mut close = false;
    ui.horizontal(|ui| {
        save = ui.button("Save").clicked();
        if form.original.is_some() {
            delete = ui.button("Delete").clicked();
        }
        close = ui.button("Close").clicked();
        ui.checkbox(&mut form.enabled, "Enabled");
    });
    if let Some(e) = &form.error {
        ui.label(RichText::new(e).color(theme.error));
    }
    egui::Grid::new("template-form").num_columns(2).spacing([14.0, 6.0]).show(ui, |ui| {
        ui.label("Abbreviation:");
        ui.add(TextEdit::singleline(&mut form.abbreviation).desired_width(160.0));
        ui.end_row();
        ui.label("Description:");
        ui.add(TextEdit::singleline(&mut form.description).desired_width(f32::INFINITY));
        ui.end_row();
        ui.label("Template text:");
        ui.add(
            TextEdit::multiline(&mut form.text)
                .font(FontId::monospace(12.5))
                .desired_rows(5)
                .desired_width(f32::INFINITY)
                .hint_text("$NAME$ variables, $END$ for the caret, $SELECTION$ for Surround With"),
        );
        ui.end_row();
        ui.label("Applicable in:");
        ui.horizontal_wrapped(|ui| {
            for c in CONTEXTS {
                let mut on = form.languages.iter().any(|l| l == c);
                if ui.checkbox(&mut on, *c).changed() {
                    if on {
                        form.languages.push((*c).to_string());
                    } else {
                        form.languages.retain(|l| l != c);
                    }
                }
            }
        });
        ui.end_row();
    });
    let names = Template::new("x", "", &form.text, &[]).variables();
    if !names.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("Variables").strong()).on_hover_text(format!("Expression functions: {}", EXPRESSION_FUNCTIONS.join(", ")));
        egui::Grid::new("template-vars").num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
            ui.label(muted(st, "Name"));
            ui.label(muted(st, "Expression"));
            ui.label(muted(st, "Default value"));
            ui.label(muted(st, "Skip if defined"));
            ui.end_row();
            for n in &names {
                let entry = form.vars.entry(n.clone()).or_default();
                ui.label(RichText::new(n).monospace());
                ui.add(TextEdit::singleline(&mut entry.0).hint_text("e.g. snakeCase(NAME)").desired_width(200.0));
                ui.add(TextEdit::singleline(&mut entry.1).desired_width(140.0));
                ui.checkbox(&mut entry.2, "");
                ui.end_row();
            }
        });
    }
    if save {
        match s.execute("liveTemplates.save", &form.params()) {
            Ok(_) => {
                form.original = Some((form.abbreviation.trim().to_string(), form.languages.clone()));
                form.error = None;
            }
            Err(e) => form.error = Some(e.to_string()),
        }
    }
    if delete && let Some((a, l)) = &form.original {
        let r = s.execute("liveTemplates.delete", &json!({ "abbreviation": a, "language": l.first() }));
        match r {
            Ok(_) => close = true,
            Err(e) => form.error = Some(e.to_string()),
        }
    }
    st.prefs.pages.template = if close { None } else { Some(form) };
}

// ---------------------------------------------------------------- TODO

pub(crate) fn todo_page(ui: &mut egui::Ui, st: &mut UiState, draft: &Settings, cmds: &mut Cmds) {
    let theme = st.theme.clone();
    ui.label(RichText::new("Patterns").strong());
    ui.label(muted(st, "Regular expressions found in comments by the TODO tool window."));
    let patterns = draft.todo_patterns.clone();
    let set_patterns = |cmds: &mut Cmds, list: &[TodoPattern]| cmds.push(("settings.set", json!({ "key": "todo_patterns", "value": list })));
    egui::Grid::new("todo-patterns").num_columns(3).spacing([14.0, 4.0]).striped(true).show(ui, |ui| {
        for (i, p) in patterns.iter().enumerate() {
            ui.label(RichText::new(&p.pattern).monospace());
            let mut case = p.case_sensitive;
            if ui.checkbox(&mut case, "Case sensitive").changed() {
                let mut next = patterns.clone();
                if let Some(x) = next.get_mut(i) {
                    x.case_sensitive = case;
                }
                set_patterns(cmds, &next);
            }
            if ui.small_button("Remove").clicked() {
                let next: Vec<TodoPattern> = patterns.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, p)| p.clone()).collect();
                set_patterns(cmds, &next);
            }
            ui.end_row();
        }
    });
    ui.horizontal(|ui| {
        ui.add(TextEdit::singleline(&mut st.prefs.pages.new_pattern).hint_text(r"\bhack\b.*").desired_width(240.0));
        ui.checkbox(&mut st.prefs.pages.new_pattern_case, "Case sensitive");
        let text = st.prefs.pages.new_pattern.trim().to_string();
        if ui.add_enabled(!text.is_empty(), egui::Button::new("Add Pattern")).clicked() {
            let mut next = patterns.clone();
            next.push(TodoPattern { pattern: text, case_sensitive: st.prefs.pages.new_pattern_case });
            set_patterns(cmds, &next);
            st.prefs.pages.new_pattern.clear();
        }
        if ui.button("Reset to Defaults").clicked() {
            cmds.push(("settings.reset", json!({ "key": "todo_patterns" })));
        }
    });
    ui.add_space(10.0);
    ui.label(RichText::new("Filters").strong());
    ui.label(muted(st, "Each filter shows only the items of the patterns ticked (TODO tool window › Filter)."));
    let filters = draft.todo_filters.clone();
    let set_filters = |cmds: &mut Cmds, list: &[TodoFilter]| cmds.push(("settings.set", json!({ "key": "todo_filters", "value": list })));
    for (i, f) in filters.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&f.name).strong().color(theme.link));
            for p in &patterns {
                let mut on = f.patterns.contains(&p.pattern);
                if ui.checkbox(&mut on, RichText::new(&p.pattern).monospace()).changed() {
                    let mut next = filters.clone();
                    if let Some(x) = next.get_mut(i) {
                        if on {
                            x.patterns.push(p.pattern.clone());
                        } else {
                            x.patterns.retain(|q| *q != p.pattern);
                        }
                    }
                    set_filters(cmds, &next);
                }
            }
            if ui.small_button("Remove").clicked() {
                let next: Vec<TodoFilter> = filters.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, f)| f.clone()).collect();
                set_filters(cmds, &next);
            }
        });
    }
    ui.horizontal(|ui| {
        ui.add(TextEdit::singleline(&mut st.prefs.pages.new_filter).hint_text("Filter name").desired_width(200.0));
        let name = st.prefs.pages.new_filter.trim().to_string();
        if ui.add_enabled(!name.is_empty(), egui::Button::new("Add Filter")).clicked() {
            let mut next = filters.clone();
            next.push(TodoFilter { name, patterns: Vec::new() });
            set_filters(cmds, &next);
            st.prefs.pages.new_filter.clear();
        }
    });
}
