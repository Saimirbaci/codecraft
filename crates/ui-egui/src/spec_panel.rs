//! The Spec View tool window, docked under Project: the project's specs (`specs/<feature>/`),
//! each requirement with its status and the code that points at it. Every action is an engine
//! command (`spec.*`); this file only draws and collects clicks.

use std::path::Path;

use egui::{Color32, Key, RichText, ScrollArea, Sense};
use serde_json::{Value, json};

use codecraft_engine::Session;
use codecraft_spec::{CodeChangeKind, EarsKind, Feature, Requirement, Severity, SpecStatus, TaskState};

use crate::theme::Theme;
use crate::{UiState, run_command};

/// What the status chip says and which theme colour it uses.
fn chip(theme: &Theme, status: SpecStatus) -> (&'static str, Color32) {
    match status {
        SpecStatus::SpecChanged => ("Spec changed", theme.warning),
        SpecStatus::CodeChanged => ("Code changed", theme.vcs_modified),
        SpecStatus::Unlinked => ("Unlinked", theme.text_muted),
        SpecStatus::Linked => ("Linked", theme.accent),
        SpecStatus::InSync => ("In sync", theme.success),
    }
}

fn rel(s: &Session, path: &Path) -> String {
    match &s.ws.project {
        Some(p) => p.relative(path).into_owned(),
        None => path.display().to_string(),
    }
}

fn excerpt(text: &str) -> String {
    let t: String = text.chars().take(400).collect();
    if text.chars().count() > 400 { format!("{t}…") } else { t }
}

type Cmds = Vec<(&'static str, Value)>;

/// What the inline editor is for.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    New { feature: String },
    Edit { feature: String, id: String },
}

/// The inline requirement editor: view state until Save runs `spec.addRequirement` or
/// `spec.editRequirement`, which may refuse (and then say why here).
#[derive(Clone, Debug)]
struct Editor {
    target: Target,
    title: String,
    text: String,
    /// What the fields held when the editor opened: only what changed is sent.
    original: (String, String),
    error: Option<String>,
    focus: bool,
}

impl Editor {
    fn new_requirement(feature: &str) -> Self {
        Self {
            target: Target::New { feature: feature.to_string() },
            title: String::new(),
            text: String::new(),
            original: (String::new(), String::new()),
            error: None,
            focus: true,
        }
    }

    fn edit(feature: &str, r: &Requirement) -> Self {
        Self {
            target: Target::Edit { feature: feature.to_string(), id: r.id.clone() },
            title: r.title.clone(),
            text: r.text.clone(),
            original: (r.title.clone(), r.text.clone()),
            error: None,
            focus: true,
        }
    }

    fn can_save(&self) -> bool {
        match self.target {
            Target::New { .. } => !(self.title.trim().is_empty() && self.text.trim().is_empty()),
            Target::Edit { .. } => {
                (self.title.as_str(), self.text.as_str()) != (self.original.0.as_str(), self.original.1.as_str())
            }
        }
    }

    /// The command Save runs.
    fn command(&self) -> (&'static str, Value) {
        match &self.target {
            Target::New { feature } => {
                ("spec.addRequirement", json!({ "feature": feature, "title": self.title, "text": self.text }))
            }
            Target::Edit { feature, id } => {
                let mut p = json!({ "feature": feature, "id": id });
                if self.title != self.original.0 {
                    p["title"] = json!(self.title);
                }
                if self.text != self.original.1 {
                    p["text"] = json!(self.text);
                }
                ("spec.editRequirement", p)
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Save,
    Cancel,
}

/// The editor and what the user did with it this frame.
#[derive(Default)]
struct Editing {
    editor: Option<Editor>,
    outcome: Option<Outcome>,
}

/// The form: a title, the text, the reason the last save was refused, Save and Cancel.
fn editor_form(ui: &mut egui::Ui, theme: &Theme, ed: &mut Editor) -> Option<Outcome> {
    let mut outcome = None;
    let first_frame = ed.focus;
    let form = egui::Frame::group(ui.style()).show(ui, |ui| {
        let what = match ed.target {
            Target::New { .. } => "New requirement",
            Target::Edit { .. } => "Edit requirement",
        };
        ui.label(RichText::new(what).small().strong());
        let title = ui.add(egui::TextEdit::singleline(&mut ed.title).hint_text("Title").desired_width(f32::INFINITY));
        let text = ui.add(
            egui::TextEdit::multiline(&mut ed.text)
                .hint_text("WHEN <trigger> THE SYSTEM SHALL <response>.")
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        if ed.focus {
            title.request_focus();
            ed.focus = false;
        }
        if let Some(error) = &ed.error {
            ui.label(RichText::new(error).small().color(theme.error));
        }
        ui.horizontal(|ui| {
            if ui.add_enabled(ed.can_save(), egui::Button::new("Save")).clicked() {
                outcome = Some(Outcome::Save);
            }
            let escape = (title.has_focus() || text.has_focus()) && ui.input(|i| i.key_pressed(Key::Escape));
            if ui.button("Cancel").clicked() || escape {
                outcome = Some(Outcome::Cancel);
            }
        });
    });
    if first_frame {
        // A form opened at the end of a long list must not be below the fold.
        form.response.scroll_to_me(Some(egui::Align::Center));
    }
    outcome
}

pub fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let theme = st.theme.clone();
    let mut cmds: Cmds = Vec::new();
    let has_project = s.ws.project.is_some();

    // The "new spec" name field is view state only; creating it is `spec.new`.
    let adding_id = ui.make_persistent_id("spec-new-open");
    let name_id = ui.make_persistent_id("spec-new-name");
    let mut adding: bool = ui.data(|d| d.get_temp(adding_id)).unwrap_or(false);
    let mut name: String = ui.data(|d| d.get_temp(name_id)).unwrap_or_default();

    let editor_id = ui.make_persistent_id("spec-editor");
    let mut editing = Editing { editor: ui.data(|d| d.get_temp(editor_id)).unwrap_or(None), outcome: None };

    let mut just_opened = false;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(has_project, egui::Button::new("＋ New Spec").small())
            .on_hover_text("Create specs/<name>/")
            .clicked()
        {
            adding = !adding;
            just_opened = adding;
        }
        if ui.add_enabled(has_project, egui::Button::new("⟳").small()).on_hover_text("Refresh").clicked() {
            cmds.push(("spec.refresh", Value::Null));
        }
        if s.spec.busy() {
            ui.spinner();
        }
    });
    if adding {
        ui.horizontal(|ui| {
            let edit = ui.add(egui::TextEdit::singleline(&mut name).hint_text("feature name").desired_width(150.0));
            if just_opened {
                edit.request_focus();
            }
            let submit =
                ui.button("Create").clicked() || (edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)));
            if submit && !name.trim().is_empty() {
                cmds.push(("spec.new", json!({ "name": name.trim() })));
                name.clear();
                adding = false;
            }
            if ui.button("✕").on_hover_text("Cancel").clicked() {
                name.clear();
                adding = false;
            }
        });
    }
    ui.data_mut(|d| {
        d.insert_temp(adding_id, adding);
        d.insert_temp(name_id, name);
    });

    if !has_project {
        ui.label(RichText::new("Open a project to see its specs.").color(theme.text_muted));
    } else if s.spec.features.is_empty() {
        if s.spec.scanned() {
            empty_state(ui, &theme);
        } else {
            ui.label(RichText::new("Scanning…").color(theme.text_muted));
        }
    } else {
        summary(ui, s, &theme);
        ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for f in &s.spec.features {
                feature_section(ui, s, &theme, f, &mut cmds, &mut editing);
            }
            reference_problems(ui, s, &theme, &mut cmds);
        });
    }
    match (editing.outcome, editing.editor.as_mut()) {
        (Some(Outcome::Cancel), _) => editing.editor = None,
        (Some(Outcome::Save), Some(ed)) => {
            // Run it here: a refused edit (a heading typed into the text) is explained in the form.
            let (id, params) = ed.command();
            match s.execute(id, &params) {
                Ok(_) => editing.editor = None,
                Err(e) => ed.error = Some(e.to_string()),
            }
        }
        _ => {}
    }
    ui.data_mut(|d| d.insert_temp(editor_id, editing.editor));
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

fn empty_state(ui: &mut egui::Ui, theme: &Theme) {
    ui.add_space(4.0);
    ui.label(RichText::new("No specs yet.").strong());
    ui.label(
        RichText::new(
            "A spec is a folder specs/<feature>/ with requirements.md, design.md and tasks.md. \
             Requirements have ids (REQ-1); code points at them with `// spec: REQ-1` or #[spec(\"REQ-1\")].",
        )
        .color(theme.text_muted)
        .small(),
    );
}

/// One line of totals, so a glance says whether anything needs attention.
fn summary(ui: &mut egui::Ui, s: &Session, theme: &Theme) {
    let (mut total, mut changed, mut code_changed, mut unlinked) = (0usize, 0usize, 0usize, 0usize);
    for f in &s.spec.features {
        for r in &f.requirements {
            total += 1;
            match s.spec.status_of(f, r) {
                SpecStatus::SpecChanged => changed += 1,
                SpecStatus::CodeChanged => code_changed += 1,
                SpecStatus::Unlinked => unlinked += 1,
                SpecStatus::Linked | SpecStatus::InSync => {}
            }
        }
    }
    let mut text = format!("{} feature(s), {total} requirement(s)", s.spec.features.len());
    if changed > 0 {
        text.push_str(&format!(" · {changed} spec changed"));
    }
    if code_changed > 0 {
        text.push_str(&format!(" · {code_changed} code changed"));
    }
    if unlinked > 0 {
        text.push_str(&format!(" · {unlinked} unlinked"));
    }
    if s.spec.truncated {
        text.push_str(" · references truncated");
    }
    ui.label(RichText::new(text).color(theme.text_muted).small());
}

fn feature_section(ui: &mut egui::Ui, s: &Session, theme: &Theme, f: &Feature, cmds: &mut Cmds, editing: &mut Editing) {
    let (done, total) = f.task_progress();
    let mut header = f.name.clone();
    if total > 0 {
        header.push_str(&format!("  {done}/{total} tasks"));
    }
    let issues = f.diagnostics.iter().filter(|d| d.severity >= Severity::Warning).count();
    if issues > 0 {
        header.push_str(&format!("  ⚠ {issues}"));
    }
    let open = egui::CollapsingHeader::new(RichText::new(header).strong())
        .id_salt(("spec-feature", &f.name))
        .default_open(true)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for (label, which, exists) in [
                    ("Requirements", "requirements", f.requirements_file.is_some()),
                    ("Design", "design", f.design_file.is_some()),
                    ("Tasks", "tasks", f.tasks_file.is_some()),
                ] {
                    let text =
                        RichText::new(label).small().color(if exists { theme.link } else { theme.text_disabled });
                    if ui.add_enabled(exists, egui::Label::new(text).sense(Sense::click())).clicked() {
                        cmds.push(("spec.open", json!({ "feature": f.name, "file": which })));
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(RichText::new("＋ Requirement").small()).frame(false))
                        .on_hover_text("Add a requirement to requirements.md")
                        .clicked()
                    {
                        editing.editor = Some(Editor::new_requirement(&f.name));
                    }
                });
            });
            if f.requirements.is_empty() {
                ui.label(RichText::new("No requirements yet (## REQ-1: Title).").color(theme.text_muted).small());
            }
            for r in &f.requirements {
                requirement_row(ui, s, theme, f, r, cmds, editing);
            }
            if let Some(ed) = editing.editor.as_mut().filter(|e| e.target == Target::New { feature: f.name.clone() }) {
                editing.outcome = editor_form(ui, theme, ed).or(editing.outcome);
            }
            for d in f.diagnostics.iter().filter(|d| d.severity >= Severity::Warning) {
                let color = if d.severity == Severity::Error { theme.error } else { theme.warning };
                let text = if d.line > 0 {
                    format!("{}:{}  {}", d.file, d.line, d.message)
                } else {
                    format!("{}  {}", d.file, d.message)
                };
                let resp =
                    ui.add(egui::Label::new(RichText::new(text).small().color(color)).truncate().sense(Sense::click()));
                if resp.clicked() && d.line > 0 {
                    cmds.push(("spec.openLink", json!({ "path": f.dir.join(&d.file), "line": d.line })));
                }
            }
        });
    open.header_response.context_menu(|ui| {
        if ui.button("Mark All Requirements In Sync").clicked() {
            cmds.push(("spec.markSynced", json!({ "feature": f.name })));
            ui.close();
        }
    });
}

fn requirement_row(
    ui: &mut egui::Ui,
    s: &Session,
    theme: &Theme,
    f: &Feature,
    r: &Requirement,
    cmds: &mut Cmds,
    editing: &mut Editing,
) {
    let status = s.spec.status_of(f, r);
    let (label, color) = chip(theme, status);
    let links = s.spec.index.links_of(&f.name, &r.id);
    let changes = s.spec.code_changes_of(&f.name, &r.id);
    let tasks: Vec<_> = f.tasks.iter().filter(|t| t.requirements.contains(&r.id)).collect();
    let toggle_id = ui.make_persistent_id(("spec-links", &f.name, &r.id));
    let mut show_links: bool = ui.data(|d| d.get_temp(toggle_id)).unwrap_or(false);

    ui.horizontal(|ui| {
        ui.add_space(8.0);
        ui.label(RichText::new(label).small().color(color));
        ui.label(RichText::new(&r.id).monospace().small().color(theme.text_muted));
        let tip = if r.ears == EarsKind::Unstructured {
            format!("{}\n\nNot in EARS form (WHEN … THE SYSTEM SHALL …)", excerpt(&r.text))
        } else {
            excerpt(&r.text)
        };
        let title = ui.add(egui::Label::new(r.display_title()).truncate().sense(Sense::click())).on_hover_text(tip);
        if title.clicked() {
            cmds.push(("spec.open", json!({ "feature": f.name, "id": r.id })));
        }
        title.context_menu(|ui| {
            if ui.button("Go to Requirement").clicked() {
                cmds.push(("spec.open", json!({ "feature": f.name, "id": r.id })));
                ui.close();
            }
            if ui.button("Edit…").clicked() {
                editing.editor = Some(Editor::edit(&f.name, r));
                ui.close();
            }
            if ui.button("Mark In Sync").on_hover_text("Record this text and its code as implemented").clicked() {
                cmds.push(("spec.markSynced", json!({ "feature": f.name, "id": r.id })));
                ui.close();
            }
        });
        if !links.is_empty() || !changes.is_empty() || !tasks.is_empty() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let done = tasks.iter().filter(|t| t.state == TaskState::Done).count();
                let label = if links.is_empty() {
                    format!("☰ {done}/{}", tasks.len())
                } else {
                    format!("⛓ {}", links.len())
                };
                let toggle = ui
                    .add(egui::Button::new(RichText::new(label).small()).frame(false))
                    .on_hover_text("Code that points at this requirement, and its tasks");
                if toggle.clicked() {
                    show_links = !show_links;
                }
            });
        }
    });
    if show_links {
        for l in links {
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                let mut text = format!("{}:{}", rel(s, &l.path), l.line);
                if let Some(symbol) = &l.symbol {
                    text.push_str(&format!("  {}", symbol.signature));
                }
                if ui
                    .add(
                        egui::Label::new(RichText::new(text).monospace().small().color(theme.link))
                            .truncate()
                            .sense(Sense::click()),
                    )
                    .clicked()
                {
                    cmds.push(("spec.openLink", json!({ "path": l.path, "line": l.line })));
                }
            });
        }
        for c in &changes {
            let (mark, what) = match c.change {
                CodeChangeKind::Modified => ("~", "changed"),
                CodeChangeKind::Added => ("+", "added"),
                CodeChangeKind::Removed => ("−", "removed"),
            };
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                let text = format!("{mark} {}  {}  ({what} since the last sync)", c.path, c.symbol);
                let label = egui::Label::new(RichText::new(text).small().color(theme.vcs_modified)).truncate();
                match c.line {
                    Some(line) => {
                        if ui.add(label.sense(Sense::click())).clicked() {
                            let path = s.project_root().map(|root| root.join(&c.path));
                            if let Some(path) = path {
                                cmds.push(("spec.openLink", json!({ "path": path, "line": line })));
                            }
                        }
                    }
                    None => {
                        ui.add(label);
                    }
                }
            });
        }
    }
    if show_links {
        for t in &tasks {
            ui.horizontal(|ui| {
                ui.add_space(28.0);
                let (mark, next) = match t.state {
                    TaskState::Todo => ("☐", "done"),
                    TaskState::Doing => ("◐", "done"),
                    TaskState::Done => ("☑", "todo"),
                };
                let text = format!("{mark} {} {}", t.id, t.text);
                let label = egui::Label::new(RichText::new(text.trim_end()).small()).truncate().sense(Sense::click());
                if ui.add(label).on_hover_text("Click to tick or clear").clicked() {
                    cmds.push(("spec.setTask", json!({ "feature": f.name, "line": t.line, "state": next })));
                }
            });
        }
    }
    let target = Target::Edit { feature: f.name.clone(), id: r.id.clone() };
    if let Some(ed) = editing.editor.as_mut().filter(|e| e.target == target) {
        editing.outcome = editor_form(ui, theme, ed).or(editing.outcome);
    }
    ui.data_mut(|d| d.insert_temp(toggle_id, show_links));
}

/// References in code to requirements that do not exist (or are ambiguous): the code and the
/// specs disagree, which is what the view exists to show.
fn reference_problems(ui: &mut egui::Ui, s: &Session, theme: &Theme, cmds: &mut Cmds) {
    for (title, list) in
        [("Dangling references", &s.spec.index.dangling), ("Ambiguous references", &s.spec.index.ambiguous)]
    {
        if list.is_empty() {
            continue;
        }
        egui::CollapsingHeader::new(RichText::new(format!("{title} ({})", list.len())).color(theme.warning))
            .id_salt(("spec-problems", title))
            .default_open(false)
            .show(ui, |ui| {
                for d in list {
                    let text = format!("{}:{}  {}", rel(s, &d.path), d.line, d.reference);
                    if ui
                        .add(egui::Label::new(RichText::new(text).monospace().small()).truncate().sense(Sense::click()))
                        .clicked()
                    {
                        cmds.push(("spec.openLink", json!({ "path": d.path, "line": d.line })));
                    }
                }
            });
    }
}
