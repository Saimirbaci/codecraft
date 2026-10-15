//! Settings (Cmd+,) with its searchable page tree, the keymap editor, the colour scheme editor,
//! View › Quick Switch Scheme (Ctrl+`) and Run › Edit Configurations. Thin: every change is an
//! engine command (`settings.*`, `keymap.*`, `colorScheme.*`, `quickSwitch.*`, `runConfig.*`);
//! the engine keeps the drafts behind Apply / OK / Cancel.

use egui::{Align, Align2, Color32, Event, FontId, Key, Layout, Modifiers, RichText, ScrollArea, TextEdit, vec2};
use serde_json::{Value, json};

use codecraft_engine::run_config_store::{RunKind, Selection, UserRunConfig, env_text};
use codecraft_engine::settings::{self, Kind, QuickLevel, SettingDef};
use codecraft_engine::{Popup, Session, keymaps, registry};
use codecraft_workspace::Settings;

use crate::theme::Theme;
use crate::{UiState, keymap, run_command};

/// View state of the settings UI.
#[derive(Default)]
pub struct PrefsUi {
    /// Settings that the theme, fonts and bindings were last built from.
    sig: String,
    /// The command waiting for a key press in the keymap editor.
    recording: Option<&'static str>,
    /// A recorded shortcut that other commands use: `(command, shortcut, others)`.
    conflict: Option<(&'static str, String, Vec<&'static str>)>,
    keymap_filter: String,
    /// The Edit Configurations form being typed into.
    run_form: Option<RunForm>,
    run_error: Option<String>,
    // ---- M13 track ST
    /// Recording a two-stroke shortcut (the keymap editor's "Second stroke" toggle).
    pub(crate) record_chord: bool,
    /// The first stroke recorded while waiting for the second.
    pub(crate) recorded_first: Option<String>,
    /// The first stroke of a chord that was pressed; the next key completes it.
    pub(crate) pending_chord: Option<keymap::Shortcut>,
    /// The active keymap's two-stroke bindings.
    pub(crate) chords: Vec<keymap::ChordBinding>,
    /// The fonts installed at start (the editor font goes in front of their monospace family).
    pub(crate) base_fonts: Option<egui::FontDefinitions>,
    /// The engine's font generation the fonts were last built for.
    pub(crate) font_generation: u64,
    /// Settings › Editor › Font: list every family, not only monospaced ones.
    pub(crate) all_fonts: bool,
    /// The keymap name being typed in Rename….
    pub(crate) keymap_rename: Option<String>,
    /// Settings pages' own view state (live template form, TODO inputs).
    pub(crate) pages: crate::settings_pages::PagesUi,
}

impl PrefsUi {
    /// Commands rebuild the fonts on the next sync (tests).
    pub fn font_generation(&self) -> u64 {
        self.font_generation
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunForm {
    index: usize,
    /// Editing this kind's template instead of a configuration (M13 track RC).
    template: Option<RunKind>,
    pub(crate) name: String,
    kind: RunKind,
    command: String,
    env: String,
    env_file: String,
    working_dir: String,
    /// Where it runs (M11: Local, Docker, SSH).
    target: codecraft_engine::run_targets::RunTarget,
    /// M13 track RC: before-launch tasks, debugger options, project file, CMake executable.
    pub(crate) before_launch: Vec<codecraft_engine::run_config_store::BeforeLaunchTask>,
    pub(crate) debugger_init: String,
    pub(crate) debugger_stop: bool,
    pub(crate) debugger_env: String,
    /// Python: justMyCode and subprocess debugging (M14 track DBG).
    pub(crate) debugger_just_my_code: bool,
    pub(crate) debugger_sub_process: bool,
    store_as_project_file: bool,
    executable: String,
}

impl RunForm {
    fn new(c: &UserRunConfig, index: usize, template: Option<RunKind>) -> Self {
        Self {
            index,
            template,
            name: c.name.clone(),
            kind: c.kind,
            command: c.command.clone(),
            env: env_text(&c.env),
            env_file: c.env_file.clone(),
            working_dir: c.working_dir.clone(),
            target: c.target.clone(),
            before_launch: c.before_launch.clone(),
            debugger_init: c.debugger.init_commands.join("\n"),
            debugger_stop: c.debugger.stop_at_entry,
            debugger_env: env_text(&c.debugger.env),
            debugger_just_my_code: c.debugger.just_my_code.unwrap_or(true),
            debugger_sub_process: c.debugger.sub_process.unwrap_or(true),
            store_as_project_file: c.store_as_project_file,
            executable: c.executable.clone(),
        }
    }

    /// `runConfig.update` / `runConfig.template.update` params.
    fn params(&self) -> Value {
        let mut p = json!({
            "command": self.command,
            "env": self.env,
            "envFile": self.env_file,
            "workingDir": self.working_dir,
            "target": self.target,
            "beforeLaunch": self.before_launch,
            "debugger": {
                "initCommands": self.debugger_init, "stopAtEntry": self.debugger_stop, "env": self.debugger_env,
                "justMyCode": self.debugger_just_my_code, "subProcess": self.debugger_sub_process,
            },
            "storeAsProjectFile": self.store_as_project_file,
            "executable": self.executable,
        });
        match self.template {
            Some(kind) => p["kind"] = json!(kind.id()),
            None => {
                p["index"] = json!(self.index);
                p["name"] = json!(self.name);
                p["kind"] = json!(self.kind.id());
            }
        }
        p
    }
}

// ---------------------------------------------------------------- applying settings

/// Rebuild the editor font when the engine loaded (or dropped) one.
fn sync_fonts(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let generation = s.prefs_services.fonts.generation;
    if generation == st.prefs.font_generation {
        return;
    }
    st.prefs.font_generation = generation;
    let base = st.prefs.base_fonts.clone().unwrap_or_default();
    match &s.prefs_services.fonts.loaded {
        Some(font) => match crate::fonts::with_editor_font(&base, font) {
            Ok(defs) => {
                ctx.set_fonts(defs);
                st.mono_font_name = font.family.clone();
            }
            Err(e) => {
                s.prefs_services.fonts.error = Some(e.clone());
                s.status = e;
                ctx.set_fonts(base);
            }
        },
        None => ctx.set_fonts(base),
    }
    ctx.request_repaint();
}

/// Rebuild the theme, UI font size and key bindings when the settings they depend on change.
pub(crate) fn sync(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    sync_fonts(ctx, s, st);
    let w = &s.ws.settings;
    let sig = serde_json::to_string(&(
        &w.theme,
        &w.color_scheme,
        &w.user_schemes,
        w.ui_font_size,
        &w.keymap,
        &w.keymap_overrides,
        &w.user_keymaps,
        w.compact_mode,
    ))
    .unwrap_or_default();
    if sig == st.prefs.sig {
        return;
    }
    st.prefs.sig = sig;
    st.theme = Theme::from_settings(w);
    st.theme.apply(ctx);
    apply_ui_font_size(ctx, w.ui_font_size);
    if w.compact_mode {
        crate::appearance_ui::compact_spacing(ctx);
    }
    keymap::set_active(w);
    st.bindings = keymap::bindings_for(w);
    st.prefs.chords = keymap::chords_for(w);
    st.prefs.pending_chord = None;
    s.terminal.set_palette(crate::terminal_panel::palette(&st.theme));
    // Widgets laid out this frame still use the old style.
    ctx.request_repaint();
}

/// Scale egui's text styles so that 13 pt is the default look.
fn apply_ui_font_size(ctx: &egui::Context, size: f32) {
    let scale = if size.is_finite() { (size / 13.0).clamp(0.5, 3.0) } else { 1.0 };
    let mut style = (*ctx.global_style()).clone();
    style.text_styles = egui::Style::default()
        .text_styles
        .into_iter()
        .map(|(k, mut f)| {
            f.size *= scale;
            (k, f)
        })
        .collect();
    ctx.set_global_style(style);
}

// ---------------------------------------------------------------- keyboard

/// The Settings dialog, Edit Configurations and Quick Switch are modal: they take the keyboard.
pub(crate) fn wants_keyboard(s: &Session) -> bool {
    s.prefs.quick_switch.is_some() || matches!(s.popup, Some(Popup::Settings)) || s.run_store.dialog.is_some()
}

fn consume_keys(ctx: &egui::Context) {
    ctx.input_mut(|i| i.events.retain(|e| !matches!(e, Event::Key { .. } | Event::Text(_))));
}

pub(crate) fn handle_keys(ctx: &egui::Context, events: &[Event], s: &mut Session, st: &mut UiState) {
    // Recording a shortcut: the next key press is it (Escape alone cancels).
    if let Some(id) = st.prefs.recording {
        let press = events.iter().find_map(|e| match e {
            Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
            _ => None,
        });
        consume_keys(ctx);
        if let Some((key, m)) = press {
            if key == Key::Escape && m.is_none() {
                st.prefs.recording = None;
                st.prefs.recorded_first = None;
            } else if let Some(sc) = keymap::from_event(key, &m) {
                match (st.prefs.record_chord, st.prefs.recorded_first.take()) {
                    // Two-stroke recording: the first key waits for the second.
                    (true, None) => st.prefs.recorded_first = Some(sc),
                    (_, Some(first)) => {
                        st.prefs.recording = None;
                        add_shortcut(s, st, id, &format!("{first} {sc}"), false);
                    }
                    (false, None) => {
                        st.prefs.recording = None;
                        add_shortcut(s, st, id, &sc, false);
                    }
                }
            }
        }
        return;
    }
    if s.prefs.quick_switch.is_some() {
        let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
        if pressed(Key::Escape) || pressed(Key::ArrowLeft) || pressed(Key::Backspace) {
            run_command(s, st, "quickSwitch.back", &Value::Null);
        } else if pressed(Key::ArrowDown) {
            run_command(s, st, "quickSwitch.move", &json!({ "delta": 1 }));
        } else if pressed(Key::ArrowUp) {
            run_command(s, st, "quickSwitch.move", &json!({ "delta": -1 }));
        } else if pressed(Key::Enter) || pressed(Key::ArrowRight) {
            run_command(s, st, "quickSwitch.accept", &Value::Null);
        } else {
            let digit = events.iter().find_map(|e| match e {
                Event::Text(t) => t.chars().next().and_then(|c| c.to_digit(10)).filter(|d| *d >= 1),
                _ => None,
            });
            if let Some(d) = digit {
                run_command(s, st, "quickSwitch.accept", &json!({ "index": d - 1 }));
            }
        }
        consume_keys(ctx);
        return;
    }
    if ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
        if s.run_store.dialog.is_some() {
            run_command(s, st, "runConfig.cancel", &Value::Null);
        } else {
            run_command(s, st, "settings.dialog.cancel", &Value::Null);
        }
    }
    // Everything else belongs to the dialog's own fields; no IDE shortcuts while it is open.
}

/// The status bar's pending-chord indicator ("⌘K was pressed. Waiting for the second key…").
pub(crate) fn pending_chord_text(st: &UiState) -> Option<String> {
    st.prefs
        .pending_chord
        .as_ref()
        .map(|sc| format!("{} was pressed. Waiting for the second key of the chord…", keymap::stroke_text(sc)))
}

/// Two-stroke shortcuts: a key that starts a chord waits for the next one; the next key runs
/// the chord's command (or ends the chord). Returns true when the key was taken.
pub(crate) fn chord_key(
    ctx: &egui::Context,
    s: &mut Session,
    st: &mut UiState,
    key: Key,
    m: &Modifiers,
    editor_focused: bool,
    text_field_focused: bool,
) -> bool {
    let applies = |c: &keymap::ChordBinding| match c.context {
        codecraft_engine::Context::Editor => editor_focused,
        codecraft_engine::Context::Global => !text_field_focused || c.first.has_command_modifier(),
    };
    if let Some(first) = st.prefs.pending_chord.take() {
        ctx.input_mut(|i| i.consume_key(*m, key));
        if key == Key::Escape && m.is_none() {
            return true;
        }
        let hit = st.prefs.chords.iter().find(|c| c.first == first && c.second.matches(key, m) && applies(c)).map(|c| c.id);
        match hit {
            Some(id) => run_command(s, st, id, &crate::platform_params(id)),
            None => {
                let second = keymap::Shortcut { key, cmd: m.mac_cmd, ctrl: m.ctrl, alt: m.alt, shift: m.shift };
                s.status = format!(
                    "{}, {} is not a shortcut in this keymap",
                    keymap::stroke_text(&first),
                    keymap::stroke_text(&second)
                );
            }
        }
        return true;
    }
    let starts = st.prefs.chords.iter().any(|c| c.first.matches(key, m) && applies(c));
    if starts {
        ctx.input_mut(|i| i.consume_key(*m, key));
        st.prefs.pending_chord = Some(keymap::Shortcut { key, cmd: m.mac_cmd, ctrl: m.ctrl, alt: m.alt, shift: m.shift });
        ctx.request_repaint();
    }
    starts
}

// ---------------------------------------------------------------- windows

pub(crate) fn show(ui: &mut egui::Ui, s: &mut Session, st: &mut UiState) {
    let ctx = ui.ctx().clone();
    if matches!(s.popup, Some(Popup::Settings)) {
        if s.prefs.dialog.is_none() {
            run_command(s, st, "app.settings", &Value::Null);
        }
        settings_window(&ctx, s, st);
    } else {
        st.prefs.recording = None;
        st.prefs.recorded_first = None;
        st.prefs.conflict = None;
        st.prefs.keymap_rename = None;
    }
    if s.prefs.quick_switch.is_some() {
        quick_switch(&ctx, s, st);
    }
    if s.run_store.dialog.is_some() {
        run_configs_window(&ctx, s, st);
    } else {
        st.prefs.run_form = None;
        st.prefs.run_error = None;
    }
}

/// Commands to run after a window is painted (`id`, params).
pub(crate) type Cmds = Vec<(&'static str, Value)>;

fn settings_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(dialog) = s.prefs.dialog.clone() else { return };
    let mut open = true;
    let mut cmds: Cmds = Vec::new();
    let theme = st.theme.clone();
    crate::windows::resizable(egui::Window::new("Settings").default_size([880.0, 600.0]), st, "settings")
        .id(egui::Id::new("settings-window"))
        .open(&mut open)
        .collapsible(false)
        .show(ctx, |ui| {
            egui::Panel::bottom("settings-buttons").resizable(false).show(ui, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if let Some(e) = &dialog.error {
                        ui.label(RichText::new(e).color(theme.error));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let changed = dialog.draft != s.ws.settings || dialog.code_style.dirty();
                        if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                            cmds.push(("settings.dialog.apply", Value::Null));
                        }
                        if ui.button("Cancel").clicked() {
                            cmds.push(("settings.dialog.cancel", Value::Null));
                        }
                        if ui.button("OK").clicked() {
                            cmds.push(("settings.dialog.ok", Value::Null));
                        }
                    });
                });
            });
            egui::Panel::left("settings-tree").resizable(true).default_size(230.0).show(ui, |ui| {
                let mut query = dialog.query.clone();
                let resp = ui
                    .add(TextEdit::singleline(&mut query).hint_text("🔍 Search settings").desired_width(f32::INFINITY));
                if resp.changed() {
                    cmds.push(("settings.dialog.search", json!({ "query": query })));
                }
                ui.add_space(4.0);
                ScrollArea::vertical().id_salt("settings-tree-scroll").show(ui, |ui| {
                    page_tree(ui, &dialog.page, &dialog.query, &theme, &mut cmds);
                });
            });
            ScrollArea::vertical().id_salt("settings-page").show(ui, |ui| {
                ui.add_space(2.0);
                ui.label(RichText::new(&dialog.page).strong().size(15.0));
                ui.add_space(6.0);
                page(ui, s, st, &dialog.page, &dialog.draft, &dialog.query, &mut cmds);
            });
        });
    if !open {
        cmds.push(("settings.dialog.cancel", Value::Null));
    }
    for (id, p) in cmds {
        if id == "agent.connectExternal" {
            // Its result is shown in the Connect an External Agent window.
            crate::agent_panel::open_connect(s, st);
            continue;
        }
        run_command(s, st, id, &p);
    }
}

fn page_tree(ui: &mut egui::Ui, current: &str, query: &str, theme: &Theme, cmds: &mut Cmds) {
    let visible = settings::matching_pages(query);
    let mut last_group = "";
    for page in settings::PAGES.iter().copied().filter(|p| visible.contains(p)) {
        let (group, leaf) = page.split_once(" › ").unwrap_or(("", page));
        if !group.is_empty() && group != last_group {
            ui.label(RichText::new(group).color(theme.text_muted));
        }
        last_group = group;
        ui.horizontal(|ui| {
            if !group.is_empty() {
                ui.add_space(14.0);
            }
            if ui.add(egui::Button::selectable(page == current, leaf).frame_when_inactive(false)).clicked() {
                cmds.push(("settings.dialog.page", json!({ "page": page })));
            }
        });
    }
    if visible.is_empty() {
        ui.label(RichText::new("Nothing found").color(theme.text_muted));
    }
}

fn page(
    ui: &mut egui::Ui,
    s: &mut Session,
    st: &mut UiState,
    page: &str,
    draft: &Settings,
    query: &str,
    cmds: &mut Cmds,
) {
    use crate::settings_pages as pages;
    match page {
        settings::PAGE_KEYMAP => keymap_page(ui, st, draft, cmds),
        settings::PAGE_COLOR_SCHEME => color_scheme_page(ui, st, draft, query, cmds),
        settings::PAGE_LIVE_TEMPLATES => pages::templates_page(ui, s, st, cmds),
        settings::PAGE_TOOLS => tools_page(ui, s, st, cmds),
        settings::PAGE_AGENT_ACCESS => crate::agent_access_ui::page(ui, s, st, cmds),
        settings::PAGE_SQL => crate::database_settings_ui::page(ui, s, st, cmds),
        settings::PAGE_FONT => pages::font_page(ui, s, st, draft, cmds),
        settings::PAGE_CODE_STYLE_RUST => pages::code_style_page(ui, s, st, "rust", cmds),
        settings::PAGE_CODE_STYLE_CPP => pages::code_style_page(ui, s, st, "cpp", cmds),
        settings::PAGE_CODE_STYLE_PYTHON => {
            pages::code_style_page(ui, s, st, "python", cmds);
            ui.add_space(8.0);
            setting_grid_keys(ui, st, draft, &["python_formatter", "python_reformat_on_save"], cmds);
        }
        settings::PAGE_CODE_STYLE_OTHER => pages::code_style_other_page(ui, st, draft, cmds),
        settings::PAGE_TODO => pages::todo_page(ui, st, draft, cmds),
        settings::PAGE_INSPECTIONS => crate::inspect_ui::settings_page(ui, s, st, cmds),
        settings::PAGE_PYTHON => crate::python_settings_ui::page(ui, s, st, draft, cmds),
        _ => setting_grid(ui, st, page, draft, query, cmds),
    }
}

/// Some settings (by key) as a grid, wherever they are documented.
pub(crate) fn setting_grid_keys(ui: &mut egui::Ui, st: &UiState, draft: &Settings, keys: &[&str], cmds: &mut Cmds) {
    egui::Grid::new(("settings-grid-keys", keys.first().copied().unwrap_or(""))).num_columns(2).spacing([18.0, 8.0]).show(
        ui,
        |ui| {
            for d in keys.iter().filter_map(|k| settings::find(k)) {
                ui.label(d.label).on_hover_text(d.description);
                if let Some(v) = widget(ui, st, d, draft) {
                    cmds.push(("settings.set", json!({ "key": d.key, "value": v })));
                }
                ui.end_row();
            }
        },
    );
}

/// The page's settings as a two-column grid (only those matching a search, when some do).
pub(crate) fn setting_grid(ui: &mut egui::Ui, st: &UiState, page: &str, draft: &Settings, query: &str, cmds: &mut Cmds) {
    let defs: Vec<&SettingDef> = settings::SETTINGS
        .iter()
        .filter(|d| d.page == page && !d.kind.has_own_editor() && d.kind != Kind::FontFamily)
        // The global menu bar exists only on macOS.
        .filter(|d| d.key != "native_menu_bar" || cfg!(target_os = "macos"))
        .collect();
    let hits: Vec<&SettingDef> = defs.iter().copied().filter(|d| settings::matches(d, query)).collect();
    let shown = if hits.is_empty() { defs } else { hits };
    egui::Grid::new(("settings-grid", page)).num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
        for d in shown {
            ui.label(d.label).on_hover_text(d.description);
            if let Some(v) = widget(ui, st, d, draft) {
                cmds.push(("settings.set", json!({ "key": d.key, "value": v })));
            }
            ui.end_row();
        }
    });
}

/// One setting's editor; returns the new value when the user changed it.
fn widget(ui: &mut egui::Ui, st: &UiState, d: &SettingDef, draft: &Settings) -> Option<Value> {
    let current = settings::get_value(draft, d.key).unwrap_or(Value::Null);
    match d.kind {
        Kind::Bool => {
            let mut on = current.as_bool().unwrap_or(false);
            ui.checkbox(&mut on, "").on_hover_text(d.description).changed().then(|| json!(on))
        }
        Kind::Int { min, max } => {
            let mut n = current.as_i64().unwrap_or(min);
            let mut w = egui::DragValue::new(&mut n).range(min..=max);
            if d.key == "lsp_memory_limit_mb" {
                w = w.speed(16.0).custom_formatter(|v, _| if v <= 0.0 { "never".into() } else { format!("{v:.0}") });
            }
            ui.add(w).changed().then(|| json!(n))
        }
        Kind::Float { min, max } => {
            let mut n = current.as_f64().unwrap_or(min);
            ui.add(egui::DragValue::new(&mut n).range(min..=max).speed(0.1).max_decimals(2)).changed().then(|| json!(n))
        }
        Kind::Text => {
            let mut t = current.as_str().unwrap_or_default().to_string();
            let hint = match d.key {
                "commit_ai_model" => codecraft_engine::commit_ai::Provider::from_id(&draft.commit_ai_provider)
                    .map(|p| p.default_model())
                    .unwrap_or("provider default"),
                _ => "",
            };
            ui.add(TextEdit::singleline(&mut t).hint_text(hint).desired_width(320.0)).changed().then(|| json!(t))
        }
        Kind::OptPath => {
            let mut t = current.as_str().unwrap_or_default().to_string();
            ui.add(TextEdit::singleline(&mut t).hint_text("auto-detect").desired_width(320.0))
                .changed()
                .then(|| if t.trim().is_empty() { Value::Null } else { json!(t) })
        }
        Kind::Provider => {
            use codecraft_engine::commit_ai::Provider;
            let id = current.as_str().unwrap_or_default().to_string();
            let mut chosen = id.clone();
            let label = Provider::from_id(&id).map(|p| p.label().to_string()).unwrap_or(id.clone());
            egui::ComboBox::from_id_salt(("setting", d.key)).selected_text(label).show_ui(ui, |ui| {
                for p in Provider::ALL {
                    ui.selectable_value(&mut chosen, p.id().to_string(), p.label());
                }
            });
            (chosen != id).then(|| json!(chosen))
        }
        Kind::Theme => choice(ui, d.key, current.as_str().unwrap_or("Dark"), &settings::THEMES.map(String::from)),
        Kind::Keymap => {
            let cur = keymaps::active(draft);
            choice(ui, d.key, &cur, &keymaps::all_names(draft))
        }
        Kind::Choice(options) => {
            let cur = current.as_str().unwrap_or_default().to_string();
            let options: Vec<String> = options.iter().map(|o| (*o).to_string()).collect();
            choice(ui, d.key, &cur, &options)
        }
        Kind::FontFamily => {
            let mut t = current.as_str().unwrap_or_default().to_string();
            ui.add(TextEdit::singleline(&mut t).hint_text("default").desired_width(320.0)).changed().then(|| json!(t))
        }
        Kind::ColorScheme => {
            let cur = settings::effective_scheme(draft);
            choice(ui, d.key, &cur, &settings::scheme_names(draft))
        }
        Kind::Secrets => {
            use codecraft_engine::commit_ai::Provider;
            let provider = Provider::from_id(&draft.commit_ai_provider);
            let Some((p, var)) = provider.and_then(|p| Some((p, p.key_env()?))) else {
                ui.label(RichText::new("not needed for this provider").color(st.theme.text_muted));
                return None;
            };
            let mut keys = draft.commit_ai_keys.clone();
            let mut key = keys.get(p.id()).cloned().unwrap_or_default();
            let resp = ui.add(
                TextEdit::singleline(&mut key).password(true).hint_text(format!("or set {var}")).desired_width(320.0),
            );
            resp.changed().then(|| {
                keys.insert(p.id().to_string(), key);
                json!(keys)
            })
        }
        Kind::KeymapOverrides
        | Kind::UserSchemes
        | Kind::UserKeymaps
        | Kind::TodoPatterns
        | Kind::TodoFilters
        | Kind::IndentByLanguage => None,
    }
}

pub(crate) fn choice(ui: &mut egui::Ui, key: &str, current: &str, options: &[String]) -> Option<Value> {
    let mut chosen = current.to_string();
    egui::ComboBox::from_id_salt(("setting", key)).selected_text(current).width(220.0).show_ui(ui, |ui| {
        for o in options {
            ui.selectable_value(&mut chosen, o.clone(), o.as_str());
        }
    });
    (chosen != current).then(|| json!(chosen))
}

// ---------------------------------------------------------------- keymap page

fn add_shortcut(s: &mut Session, st: &mut UiState, id: &'static str, sc: &str, remove: bool) {
    let params = json!({ "id": id, "shortcut": sc, "removeConflicts": remove });
    match s.execute("keymap.addShortcut", &params) {
        Ok(v) => {
            let others: Vec<&'static str> = v["conflicts"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| registry().get(x.as_str()?)).map(|c| c.id).collect())
                .unwrap_or_default();
            st.prefs.conflict = (!others.is_empty()).then(|| (id, sc.to_string(), others));
        }
        Err(e) => s.status = e.to_string(),
    }
}

fn keymap_page(ui: &mut egui::Ui, st: &mut UiState, draft: &Settings, cmds: &mut Cmds) {
    let theme = st.theme.clone();
    let active = keymaps::active(draft);
    let is_user = keymaps::user_keymap(draft, &active).is_some();
    ui.horizontal(|ui| {
        ui.label("Keymap:");
        if let Some(v) = choice(ui, "keymap", &active, &keymaps::all_names(draft)) {
            cmds.push(("keymap.select", json!({ "name": v })));
            st.prefs.keymap_rename = None;
        }
        if ui.button("Duplicate…").on_hover_text("A copy of this keymap that inherits its shortcuts").clicked() {
            cmds.push(("keymap.duplicate", json!({ "from": active })));
        }
        if is_user {
            if ui.button("Rename…").clicked() {
                st.prefs.keymap_rename = Some(active.clone());
            }
            if ui.button("Delete").on_hover_text("Keymaps based on it inherit from its parent").clicked() {
                cmds.push(("keymap.delete", json!({ "name": active })));
            }
        }
        let changes = keymaps::overrides(draft, &active).map(|o| o.len()).unwrap_or(0);
        if ui
            .add_enabled(changes > 0, egui::Button::new("Reset All"))
            .on_hover_text("Drop every shortcut change in this keymap")
            .clicked()
        {
            cmds.push(("keymap.reset", Value::Null));
        }
        if changes > 0 {
            ui.label(RichText::new(format!("{changes} changed")).color(theme.text_muted));
        }
    });
    if let Some(parent) = keymaps::parent_of(draft, &active) {
        ui.label(RichText::new(format!("Based on {parent}; changes here override it.")).color(theme.text_muted).size(11.5));
    }
    if let Some(mut name) = st.prefs.keymap_rename.clone() {
        ui.horizontal(|ui| {
            ui.label("New name:");
            let resp = ui.add(TextEdit::singleline(&mut name).desired_width(220.0));
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if ui.button("Rename").clicked() || enter {
                cmds.push(("keymap.rename", json!({ "name": active, "newName": name })));
                st.prefs.keymap_rename = None;
            } else if ui.button("Cancel").clicked() {
                st.prefs.keymap_rename = None;
            } else {
                st.prefs.keymap_rename = Some(name);
            }
        });
    }
    ui.horizontal(|ui| {
        ui.checkbox(&mut st.prefs.record_chord, "Second stroke")
            .on_hover_text("Record two-stroke shortcuts such as ⌘K, ⌘C (the first key waits for the second)");
    });
    ui.add(
        TextEdit::singleline(&mut st.prefs.keymap_filter)
            .hint_text("🔍 Search actions or shortcuts")
            .desired_width(f32::INFINITY),
    );
    if let Some((id, sc, others)) = st.prefs.conflict.clone() {
        egui::Frame::new().fill(theme.hover).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
            let names: Vec<&str> = others.iter().map(|o| registry().get(o).map(|c| c.label).unwrap_or(o)).collect();
            ui.label(
                RichText::new(format!("{} is also assigned to: {}", keymap::display(&sc), names.join(", ")))
                    .color(theme.warning),
            );
            ui.horizontal(|ui| {
                if ui.button("Remove Other Assignments").clicked() {
                    cmds.push(("keymap.addShortcut", json!({ "id": id, "shortcut": sc, "removeConflicts": true })));
                    st.prefs.conflict = None;
                }
                if ui.button("Leave").clicked() {
                    st.prefs.conflict = None;
                }
            });
        });
    }
    if let Some(id) = st.prefs.recording {
        let label = registry().get(id).map(|c| c.label).unwrap_or(id);
        let text = match &st.prefs.recorded_first {
            Some(first) => format!("{}, … press the second key for “{label}” (Escape cancels)", keymap::display(first)),
            None if st.prefs.record_chord => format!("Press the first key of the chord for “{label}” (Escape cancels)…"),
            None => format!("Press the shortcut for “{label}” (Escape cancels)…"),
        };
        ui.label(RichText::new(text).color(theme.accent));
    }
    ui.add_space(4.0);
    let q = st.prefs.keymap_filter.trim().to_lowercase();
    let rows: Vec<(&'static str, &'static str, String, Vec<String>, bool)> = registry()
        .all()
        .iter()
        .filter_map(|c| {
            let shortcuts = keymaps::shortcuts(draft, c.id);
            let menu = registry().menu_path(c.id).map(|m| m.join(" › ")).unwrap_or_default();
            let hit = q.is_empty()
                || c.label.to_lowercase().contains(&q)
                || c.id.to_lowercase().contains(&q)
                || menu.to_lowercase().contains(&q)
                || shortcuts
                    .iter()
                    .any(|s| s.to_lowercase().contains(&q) || keymap::display(s).to_lowercase().contains(&q));
            hit.then(|| (c.id, c.label, menu, shortcuts, keymaps::is_overridden(draft, c.id)))
        })
        .collect();
    let row_h = 24.0;
    ScrollArea::vertical().id_salt("keymap-rows").max_height(ui.available_height().max(200.0)).show_rows(
        ui,
        row_h,
        rows.len(),
        |ui, range| {
            for (id, label, menu, shortcuts, overridden) in rows.get(range).unwrap_or_default() {
                ui.horizontal(|ui| {
                    ui.set_min_height(row_h);
                    let text = RichText::new(*label).color(if *overridden { theme.link } else { theme.text });
                    ui.allocate_ui_with_layout(vec2(260.0, row_h), Layout::left_to_right(Align::Center), |ui| {
                        ui.set_width(260.0);
                        ui.add(egui::Label::new(text).truncate()).on_hover_text(format!("{id}\n{menu}"));
                    });
                    for sc in shortcuts {
                        let chip = egui::Button::new(
                            RichText::new(format!("{}  ×", keymap::display(sc))).monospace().size(11.5),
                        );
                        if ui.add(chip).on_hover_text("Remove this shortcut").clicked() {
                            cmds.push(("keymap.removeShortcut", json!({ "id": id, "shortcut": sc })));
                        }
                    }
                    let recording = st.prefs.recording == Some(*id);
                    if ui
                        .add(egui::Button::selectable(recording, "+"))
                        .on_hover_text("Add Keyboard Shortcut: press the keys")
                        .clicked()
                    {
                        st.prefs.recording = if recording { None } else { Some(*id) };
                        st.prefs.conflict = None;
                    }
                    if *overridden && ui.small_button("Reset").on_hover_text("Restore the keymap's shortcuts").clicked()
                    {
                        cmds.push(("keymap.reset", json!({ "id": id })));
                    }
                });
            }
        },
    );
}

// ---------------------------------------------------------------- colour scheme page

fn color_scheme_page(ui: &mut egui::Ui, st: &UiState, draft: &Settings, query: &str, cmds: &mut Cmds) {
    let theme = &st.theme;
    let current = settings::effective_scheme(draft);
    let user = draft.user_schemes.iter().find(|u| u.name == current);
    ui.horizontal(|ui| {
        ui.label("Scheme:");
        if let Some(v) = choice(ui, "color_scheme", &current, &settings::scheme_names(draft)) {
            cmds.push(("colorScheme.select", json!({ "name": v })));
        }
        if ui.button("Duplicate…").on_hover_text("Copy this scheme into an editable one").clicked() {
            cmds.push(("colorScheme.duplicate", json!({ "from": current })));
        }
        if user.is_some() && ui.button("Delete").clicked() {
            cmds.push(("colorScheme.delete", json!({ "name": current })));
        }
        if ui.button("Export…").on_hover_text("Save this scheme as a CodeCraft JSON file").clicked() {
            cmds.push(("colorScheme.export", json!({ "name": current })));
        }
        if ui
            .button("Import…")
            .on_hover_text("Add a scheme from a CodeCraft JSON file or an .icls file you have")
            .clicked()
        {
            cmds.push(("colorScheme.import", Value::Null));
        }
    });
    let preview = Theme::from_settings(&Settings { color_scheme: current.clone(), ..draft.clone() });
    ui.add_space(6.0);
    egui::Frame::new().fill(preview.editor_bg).inner_margin(8.0).corner_radius(4.0).show(ui, |ui| {
        let font = FontId::monospace(13.0);
        let line = |ui: &mut egui::Ui, parts: &[(&str, Color32)]| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                for (t, c) in parts {
                    ui.label(RichText::new(*t).font(font.clone()).color(*c));
                }
            });
        };
        line(ui, &[("/// Adds one.", preview.doc_comment)]);
        line(ui, &[("#[inline]", preview.attribute)]);
        line(
            ui,
            &[
                ("pub fn ", preview.keyword),
                ("add_one", preview.function),
                ("<'a>(x: ", preview.text_editor),
                ("u32", preview.type_),
                (") -> ", preview.text_editor),
                ("u32", preview.type_),
                (" {", preview.text_editor),
            ],
        );
        line(
            ui,
            &[
                ("    ", preview.text_editor),
                ("println!", preview.macro_),
                ("(", preview.text_editor),
                ("\"x = {x}\\n\"", preview.string),
                (");", preview.text_editor),
                ("  // note", preview.comment),
            ],
        );
        line(
            ui,
            &[
                ("    x + ", preview.text_editor),
                ("1", preview.number),
                ("  ", preview.text_editor),
                ("MAX", preview.constant),
            ],
        );
        line(ui, &[("}", preview.text_editor)]);
    });
    ui.add_space(6.0);
    let Some(user) = user else {
        ui.label(
            RichText::new("Built-in schemes are read-only: Duplicate one to change its colours.")
                .color(theme.text_muted),
        );
        return;
    };
    let q = query.trim().to_lowercase();
    egui::Grid::new("scheme-colors").num_columns(3).spacing([14.0, 4.0]).striped(true).show(ui, |ui| {
        for token in settings::COLOR_TOKENS.iter().filter(|t| q.is_empty() || t.contains(&q)) {
            let Some(mut c) = preview.token(token) else { continue };
            let changed = user.colors.contains_key(*token);
            ui.label(RichText::new(*token).color(if changed { theme.link } else { theme.text }));
            if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque).changed() {
                cmds.push(("colorScheme.setColor", json!({ "token": token, "color": crate::theme::to_hex(c) })));
            }
            if changed && ui.small_button("Reset").clicked() {
                cmds.push(("colorScheme.resetColor", json!({ "token": token })));
            } else if !changed {
                ui.label("");
            }
            ui.end_row();
        }
    });
}

// ---------------------------------------------------------------- live templates and tools

fn tools_page(ui: &mut egui::Ui, s: &mut Session, st: &UiState, cmds: &mut Cmds) {
    let theme = &st.theme;
    ui.label(RichText::new("These apply immediately.").color(theme.text_muted));
    ui.add_space(4.0);
    egui::Grid::new("tools-grid").num_columns(2).spacing([18.0, 8.0]).show(ui, |ui| {
        ui.label("Claude Code IDE integration");
        let mut on = s.ide.is_enabled();
        if ui.checkbox(&mut on, "").changed() {
            cmds.push((if on { "ide.enable" } else { "ide.disable" }, Value::Null));
        }
        ui.end_row();
        ui.label("Use the Claude subscription").on_hover_text("Remove ANTHROPIC_API_KEY from agents' environment");
        let mut scrub = s.agents.settings.scrub_api_keys;
        if ui.checkbox(&mut scrub, "").changed() {
            cmds.push(("agent.settings", json!({ "scrubApiKeys": scrub })));
        }
        ui.end_row();
        ui.label("Agent worktrees");
        let mut dir = s.agents.settings.worktrees_dir.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        if ui.add(TextEdit::singleline(&mut dir).hint_text("~/.codecraft/worktrees").desired_width(320.0)).lost_focus()
        {
            let v = if dir.trim().is_empty() { Value::Null } else { json!(dir.trim()) };
            cmds.push(("agent.settings", json!({ "worktreesDir": v })));
        }
        ui.end_row();
    });
}

// ---------------------------------------------------------------- quick switch

fn quick_switch(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(q) = s.prefs.quick_switch.clone() else { return };
    let items = settings::quick_items(&s.ws.settings, q.level);
    let title = match q.level {
        QuickLevel::Root => "Switch…",
        QuickLevel::ColorScheme => "Editor Color Scheme",
        QuickLevel::Keymap => "Keymap",
        QuickLevel::Theme => "Theme",
    };
    let theme = st.theme.clone();
    let mut clicked: Option<usize> = None;
    let resp = egui::Area::new(egui::Id::new("quick-switch"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 120.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme.popup_bg)
                .stroke(egui::Stroke::new(1.0, theme.separator))
                .corner_radius(8.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_min_width(300.0);
                    ui.label(RichText::new(title).strong());
                    ui.separator();
                    for (i, item) in items.iter().enumerate() {
                        let mark = if item.current { "✓ " } else { "   " };
                        let more = if q.level == QuickLevel::Root { "  ›" } else { "" };
                        let text = format!("{mark}{}. {}{more}", i + 1, item.label);
                        if ui.add(egui::Button::selectable(i == q.selected, text).frame_when_inactive(false)).clicked()
                        {
                            clicked = Some(i);
                        }
                    }
                });
        });
    if let Some(i) = clicked {
        run_command(s, st, "quickSwitch.accept", &json!({ "index": i }));
    } else if ctx.input(|i| i.pointer.any_pressed()) && !resp.response.contains_pointer() {
        run_command(s, st, "quickSwitch.close", &Value::Null);
    }
}

// ---------------------------------------------------------------- edit configurations

fn run_configs_window(ctx: &egui::Context, s: &mut Session, st: &mut UiState) {
    let Some(dialog) = s.run_store.dialog.clone() else { return };
    let root = s.run_store.root().map(std::path::Path::to_path_buf).unwrap_or_default();
    let derived: Vec<(String, String)> =
        s.run_store.derived.iter().map(|c| (c.name.clone(), c.command_line())).collect();
    // The form follows the selected user configuration (or the template shown).
    let selected_user = dialog.selected.filter(|sel| !sel.derived).map(|sel| sel.index);
    let wanted = match dialog.template {
        Some(k) => Some((Some(k), 0)),
        None => selected_user.map(|i| (None, i)),
    };
    match (wanted, &st.prefs.run_form) {
        (Some((t, i)), Some(f)) if f.template == t && f.index == i => {}
        (Some((Some(k), _)), _) => {
            let t = codecraft_engine::run_config_store::template_in(&dialog.templates, k);
            st.prefs.run_form = Some(RunForm::new(&t, 0, Some(k)));
            st.prefs.run_error = None;
        }
        (Some((None, i)), _) => {
            st.prefs.run_form = dialog.draft.get(i).map(|c| RunForm::new(c, i, None));
            st.prefs.run_error = None;
        }
        (None, _) => st.prefs.run_form = None,
    }
    let config_names: Vec<String> =
        dialog.draft.iter().map(|c| c.name.clone()).chain(derived.iter().map(|(n, _)| n.clone())).collect();
    let theme = st.theme.clone();
    let mut open = true;
    let mut cmds: Cmds = Vec::new();
    let mut form = st.prefs.run_form.clone();
    let generated = s.run_store.generate.view.clone();
    let agents: Vec<(&'static str, &'static str, Option<&'static str>, Option<String>)> = s
        .agents
        .backends()
        .iter()
        .map(|b| {
            // Ask mode when the agent has it (anything beyond reading needs approval), else Auto.
            let mode = if b.ask_unavailable().is_none() {
                Some("ask")
            } else if b.installed() {
                Some("auto")
            } else {
                None
            };
            let why = mode.is_none().then(|| b.ask_unavailable().unwrap_or_default());
            (b.backend.id(), b.backend.label(), mode, why)
        })
        .collect();
    crate::windows::resizable(
        egui::Window::new("Run/Debug Configurations").default_size([820.0, 480.0]),
        st,
        "run-configs",
    )
    .id(egui::Id::new("run-configs-window"))
    .open(&mut open)
    .collapsible(false)
    .show(ctx, |ui| {
        egui::Panel::bottom("run-configs-buttons").resizable(false).show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if let Some(e) = &dialog.error {
                    ui.label(RichText::new(e).color(theme.error));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let changed = dialog.draft != s.run_store.user;
                    if ui.add_enabled(changed, egui::Button::new("Apply")).clicked() {
                        cmds.push(("runConfig.apply", Value::Null));
                    }
                    if ui.button("Cancel").clicked() {
                        cmds.push(("runConfig.cancel", Value::Null));
                    }
                    if ui.button("OK").clicked() {
                        cmds.push(("runConfig.ok", Value::Null));
                    }
                });
            });
        });
        egui::Panel::left("run-configs-list").resizable(true).default_size(260.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.menu_button("+ Add", |ui| {
                    for kind in RunKind::ALL {
                        if ui.button(kind.label()).on_hover_text(format!("New {} configuration (from its template)", kind.label())).clicked() {
                            cmds.push(("runConfig.create", json!({ "name": kind.label(), "kind": kind.id() })));
                            ui.close();
                        }
                    }
                });
                let can_copy = dialog.selected.is_some();
                if ui.add_enabled(can_copy, egui::Button::new("Copy")).on_hover_text("Copy Configuration").clicked()
                    && let Some(sel) = dialog.selected
                {
                    cmds.push(("runConfig.copy", json!({ "index": sel.index, "derived": sel.derived })));
                }
                if ui
                    .add_enabled(selected_user.is_some(), egui::Button::new("Delete"))
                    .on_hover_text("Delete Configuration")
                    .clicked()
                    && let Some(i) = selected_user
                {
                    cmds.push(("runConfig.delete", json!({ "index": i })));
                }
                ui.menu_button("Generate", |ui| {
                    if ui
                        .button("Detect from Project")
                        .on_hover_text("Propose configurations for the project's entry points")
                        .clicked()
                    {
                        cmds.push(("runConfig.generate", Value::Null));
                        ui.close();
                    }
                    ui.separator();
                    ui.label(RichText::new("Ask an agent").color(theme.text_muted));
                    for (id, label, mode, why) in &agents {
                        let text = if *mode == Some("auto") { format!("{label} (Auto)") } else { label.to_string() };
                        let hover = match (mode, why) {
                            (Some("ask"), _) => format!("{label} reads the project and proposes configurations; anything else needs your approval"),
                            (Some(_), _) => format!("{label} has no Ask mode here, so it runs without approvals; it is told to only read the project"),
                            (None, why) => why.clone().unwrap_or_default(),
                        };
                        let button = ui.add_enabled(mode.is_some(), egui::Button::new(text));
                        if button.on_hover_text(hover).clicked() {
                            cmds.push(("runConfig.generate", json!({ "agent": id, "mode": mode })));
                            ui.close();
                        }
                    }
                });
            });
            ui.separator();
            ScrollArea::vertical().id_salt("run-configs-scroll").show(ui, |ui| {
                ui.label(RichText::new("Configurations").color(theme.text_muted));
                if dialog.draft.is_empty() {
                    ui.label(RichText::new("  none yet").color(theme.text_disabled));
                }
                for (i, c) in dialog.draft.iter().enumerate() {
                    let sel = dialog.selected == Some(Selection { derived: false, index: i });
                    let icon = if c.kind == RunKind::Cargo { "◆" } else { "▶" };
                    if ui
                        .add(egui::Button::selectable(sel, format!("{icon} {}", c.name)).frame_when_inactive(false))
                        .clicked()
                    {
                        cmds.push(("runConfig.select", json!({ "index": i })));
                    }
                }
                ui.add_space(6.0);
                ui.label(RichText::new("From the project").color(theme.text_muted));
                for (i, (name, _)) in derived.iter().enumerate() {
                    let sel = dialog.selected == Some(Selection { derived: true, index: i });
                    let text = RichText::new(format!("⚙ {name}")).color(theme.text_muted);
                    if ui.add(egui::Button::selectable(sel, text).frame_when_inactive(false)).clicked() {
                        cmds.push(("runConfig.select", json!({ "index": i, "derived": true })));
                    }
                }
                // Templates: the defaults of new configurations (M13 track RC).
                ui.add_space(6.0);
                egui::CollapsingHeader::new(RichText::new("Edit configuration templates…").color(theme.text_muted))
                    .id_salt("run-config-templates")
                    .default_open(dialog.template.is_some())
                    .show(ui, |ui| {
                        for kind in RunKind::ALL {
                            let sel = dialog.template == Some(kind);
                            let custom = dialog.templates.iter().any(|t| t.kind == kind);
                            let label = if custom { format!("🔧 {} *", kind.label()) } else { format!("🔧 {}", kind.label()) };
                            if ui.add(egui::Button::selectable(sel, label).frame_when_inactive(false)).clicked() {
                                cmds.push(("runConfig.template.select", json!({ "kind": kind.id() })));
                            }
                        }
                    });
            });
        });
        ui.add_space(4.0);
        if let Some(view) = &generated {
            generate_view(ui, &theme, view, &mut cmds);
            return;
        }
        match (dialog.selected, form.as_mut()) {
            (_, Some(f)) if f.template.is_some() => {
                ui.label(
                    RichText::new(format!("Template: {}  (new {} configurations start from it)", f.kind.label(), f.kind.label()))
                        .strong(),
                );
                if dialog.templates.iter().any(|t| Some(t.kind) == f.template)
                    && ui.small_button("Reset to Default").clicked()
                {
                    cmds.push(("runConfig.template.update", json!({ "kind": f.kind.id(), "reset": true })));
                }
                ui.add_space(4.0);
                run_form(ui, &theme, f, &root, st.prefs.run_error.as_deref(), &config_names);
            }
            (Some(Selection { derived: false, .. }), Some(f)) => {
                run_form(ui, &theme, f, &root, st.prefs.run_error.as_deref(), &config_names);
            }
            (Some(Selection { derived: true, index }), _) => {
                if let Some((name, line)) = derived.get(index) {
                    ui.label(RichText::new(name).strong().size(15.0));
                    ui.add_space(4.0);
                    ui.label(RichText::new(line).monospace());
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("Derived from the project's Cargo targets and build files; copy it to edit.")
                            .color(theme.text_muted),
                    );
                    if ui.button("Copy to Editable Configuration").clicked() {
                        cmds.push(("runConfig.copy", json!({ "index": index, "derived": true })));
                    }
                }
            }
            _ => {
                ui.label(
                    RichText::new("Add a configuration with + Add (Cargo, Command, Python, C/C++, Zig, Npm), copy one from the project, or let Generate propose them.")
                        .color(theme.text_muted),
                );
            }
        }
    });
    // Field edits go to the draft as they are typed; an invalid value stays in the form with its
    // error until it is fixed.
    if let (Some(new), Some(old)) = (&form, &st.prefs.run_form)
        && new != old
    {
        let id = if new.template.is_some() { "runConfig.template.update" } else { "runConfig.update" };
        st.prefs.run_error = s.execute(id, &new.params()).err().map(|e| e.to_string());
        st.prefs.run_form = form;
    }
    if !open {
        cmds.push(("runConfig.cancel", Value::Null));
    }
    for (id, p) in cmds {
        run_command(s, st, id, &p);
    }
}

/// Edit Configurations › Generate: the proposed configurations, grouped by ecosystem, with a
/// tick box each.
fn generate_view(
    ui: &mut egui::Ui,
    theme: &Theme,
    view: &codecraft_engine::run_config_gen::GenView,
    cmds: &mut Cmds,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Generated configurations").strong().size(15.0));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Close").clicked() {
                cmds.push(("runConfig.generate.cancel", Value::Null));
            }
            let fresh = view.proposals.iter().any(|p| !p.exists);
            if ui.add_enabled(fresh, egui::Button::new("None")).clicked() {
                cmds.push(("runConfig.generate.select", json!({ "all": false })));
            }
            if ui.add_enabled(fresh, egui::Button::new("All")).clicked() {
                cmds.push(("runConfig.generate.select", json!({ "all": true })));
            }
        });
    });
    if let Some(busy) = &view.busy {
        ui.horizontal(|ui| {
            ui.add(egui::Spinner::new().size(12.0));
            ui.label(RichText::new(busy).color(theme.text_muted));
            if let Some(job) = view.agent_job {
                ui.label(RichText::new(format!("(agent job {job}; its progress is in the Agents tool window)")).color(theme.text_disabled));
            }
        });
    }
    if let Some(e) = &view.error {
        ui.label(RichText::new(e).color(theme.error));
    }
    ui.add_space(4.0);
    let picked = view.proposals.iter().filter(|p| p.selected && !p.exists).count();
    egui::Panel::bottom("run-configs-generate-buttons").resizable(false).show(ui, |ui| {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let label = if picked == 1 { "Add 1 Configuration".to_string() } else { format!("Add {picked} Configurations") };
            if ui.add_enabled(picked > 0, egui::Button::new(label)).clicked() {
                cmds.push(("runConfig.generate.add", Value::Null));
            }
            ui.label(RichText::new("Added ones are saved with Apply or OK.").color(theme.text_muted));
        });
    });
    ScrollArea::vertical().id_salt("run-configs-generated").show(ui, |ui| {
        if view.proposals.is_empty() && view.busy.is_none() {
            ui.label(RichText::new("No entry points found. Add a configuration with + Add.").color(theme.text_muted));
        }
        let mut group = "";
        for (i, p) in view.proposals.iter().enumerate() {
            if p.group != group {
                group = &p.group;
                ui.add_space(6.0);
                ui.label(RichText::new(group).color(theme.text_muted).strong());
            }
            ui.horizontal(|ui| {
                let mut on = p.selected && !p.exists;
                let name = RichText::new(&p.config.name);
                let changed = ui.add_enabled(!p.exists, egui::Checkbox::new(&mut on, name)).changed();
                if changed {
                    cmds.push(("runConfig.generate.select", json!({ "index": i, "selected": on })));
                }
                if p.exists {
                    ui.label(RichText::new("already configured").color(theme.text_disabled));
                }
            });
            ui.indent(("generated", i), |ui| {
                let dir = if p.config.working_dir.is_empty() {
                    String::new()
                } else {
                    format!("   in {}", p.config.working_dir)
                };
                ui.label(RichText::new(format!("{}: {}{dir}", p.config.kind.label(), p.config.command)).monospace().size(12.0));
                if !p.reason.is_empty() {
                    ui.label(RichText::new(&p.reason).color(theme.text_muted).size(12.0));
                }
            });
        }
        if !view.notes.is_empty() {
            ui.add_space(8.0);
            ui.collapsing(format!("Skipped from the agent's reply ({})", view.notes.len()), |ui| {
                for n in &view.notes {
                    ui.label(RichText::new(n).color(theme.text_muted).size(12.0));
                }
            });
        }
    });
}

/// "Run on:" Local / Docker (M11 track D; track S adds SSH here) and the target's fields.
fn run_target_row(ui: &mut egui::Ui, target: &mut codecraft_engine::run_targets::RunTarget) {
    use codecraft_engine::run_targets::RunTarget;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Run on:");
        let current = match target {
            RunTarget::Local => "Local machine",
            RunTarget::Docker(_) => "Docker",
            RunTarget::Ssh(_) => "SSH",
        };
        egui::ComboBox::from_id_salt("run-config-target").selected_text(current).show_ui(ui, |ui| {
            if ui.selectable_label(target.is_local(), "Local machine").clicked() {
                *target = RunTarget::Local;
            }
            if ui.selectable_label(matches!(target, RunTarget::Docker(_)), "Docker").clicked()
                && !matches!(target, RunTarget::Docker(_))
            {
                *target = RunTarget::Docker(codecraft_engine::docker_target::DockerTarget {
                    image: "rust:1".into(),
                    ..Default::default()
                });
            }
            if ui.selectable_label(matches!(target, RunTarget::Ssh(_)), "SSH").clicked() && !matches!(target, RunTarget::Ssh(_)) {
                *target = RunTarget::Ssh(Default::default());
            }
        });
    });
    if let RunTarget::Docker(t) = target {
        crate::docker_ui::target_fields(ui, t);
    }
    if let RunTarget::Ssh(t) = target {
        crate::remote_ui::ssh_target_fields(ui, t);
    }
}

fn run_form(
    ui: &mut egui::Ui,
    theme: &Theme,
    f: &mut RunForm,
    root: &std::path::Path,
    error: Option<&str>,
    config_names: &[String],
) {
    let is_template = f.template.is_some();
    egui::Grid::new("run-config-form").num_columns(2).spacing([14.0, 8.0]).show(ui, |ui| {
        ui.label("Name:");
        ui.horizontal(|ui| {
            ui.add_enabled(!is_template, TextEdit::singleline(&mut f.name).desired_width(ui.available_width() - 170.0));
            ui.checkbox(&mut f.store_as_project_file, "Store as project file").on_hover_text(format!(
                "Save it as .codecraft/runConfigurations/{} to share it (commit it with the project)",
                codecraft_engine::run_config_store::project_file_name(&f.name)
            ));
        });
        ui.end_row();
        ui.label("Type:");
        ui.add_enabled_ui(!is_template, |ui| {
            egui::ComboBox::from_id_salt("run-config-kind").selected_text(f.kind.label()).show_ui(ui, |ui| {
                for kind in RunKind::ALL {
                    ui.selectable_value(&mut f.kind, kind, kind.label());
                }
            });
        });
        ui.end_row();
        ui.label("Command:");
        let hint = f.kind.hint();
        ui.add(
            TextEdit::singleline(&mut f.command)
                .hint_text(hint)
                .font(FontId::monospace(13.0))
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("Environment variables:");
        ui.add(TextEdit::singleline(&mut f.env).hint_text("RUST_LOG=debug;PORT=8080").desired_width(f32::INFINITY));
        ui.end_row();
        ui.label("Environment file:");
        ui.add(
            TextEdit::singleline(&mut f.env_file)
                .hint_text(".env   (variables loaded when running; the fields above win)")
                .desired_width(f32::INFINITY),
        );
        ui.end_row();
        ui.label("Working directory:");
        ui.add(
            TextEdit::singleline(&mut f.working_dir).hint_text(root.display().to_string()).desired_width(f32::INFINITY),
        );
        ui.end_row();
        if f.kind == RunKind::CMake {
            ui.label("Executable:");
            ui.add(
                TextEdit::singleline(&mut f.executable)
                    .hint_text("the target's own (from the CMake File API)")
                    .desired_width(f32::INFINITY),
            );
            ui.end_row();
        }
    });
    run_target_row(ui, &mut f.target);
    crate::rc_ui::before_launch_section(ui, theme, f, config_names);
    crate::rc_ui::debugger_section(ui, theme, f);
    ui.add_space(6.0);
    let probe =
        UserRunConfig { name: "preview".into(), kind: f.kind, command: f.command.clone(), ..Default::default() };
    let preview = match probe.to_run_config(root, root) {
        Ok(c) if f.kind != RunKind::Cpp => c.command_line(),
        _ => f.command.trim().to_string(),
    };
    ui.label(RichText::new(preview).monospace().color(theme.text_muted));
    if let Some(e) = error {
        ui.label(RichText::new(e).color(theme.error));
    }
}
