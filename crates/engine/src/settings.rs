//! Settings: the schema behind the Settings dialog (pages, labels, value kinds), validated
//! get/set by key, the dialog's draft (Apply / OK / Cancel), colour scheme names and the Quick
//! Switch Scheme popup. Commands are in `settings_cmds.rs`.
//!
//! While the Settings dialog is open, setting commands change its draft (what the dialog shows);
//! Apply/OK copy the draft to the live settings, Cancel drops it. Otherwise they change the live
//! settings directly.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use codecraft_workspace::{ColorScheme, Settings};

use crate::keymaps;

/// What kind of value a setting holds (validation and the dialog's widget).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
    },
    Text,
    /// A path or `null` (empty text clears it).
    OptPath,
    /// A provider id; free text so new providers can be tried before the dialog knows them.
    Provider,
    /// UI theme: one of [`THEMES`].
    Theme,
    /// Built-in keymap name, or empty for the platform default.
    Keymap,
    /// Built-in or user colour scheme name, or empty for the theme's.
    ColorScheme,
    /// `{provider: key}`; values are masked when read.
    Secrets,
    /// `{keymap: {command: [shortcut…]}}`.
    KeymapOverrides,
    /// `[ColorScheme…]`.
    UserSchemes,
}

/// One documented setting.
#[derive(Clone, Copy, Debug)]
pub struct SettingDef {
    pub key: &'static str,
    pub label: &'static str,
    /// Settings tree page (`Editor › General`).
    pub page: &'static str,
    pub description: &'static str,
    pub kind: Kind,
}

pub const PAGE_APPEARANCE: &str = "Appearance & Behavior › Appearance";
pub const PAGE_SYSTEM: &str = "Appearance & Behavior › System Settings";
pub const PAGE_KEYMAP: &str = "Keymap";
pub const PAGE_EDITOR_GENERAL: &str = "Editor › General";
pub const PAGE_EDITOR_TABS: &str = "Editor › Editor Tabs";
pub const PAGE_CODE_COMPLETION: &str = "Editor › Code Completion";
pub const PAGE_FONT: &str = "Editor › Font";
pub const PAGE_COLOR_SCHEME: &str = "Editor › Color Scheme";
pub const PAGE_CODE_STYLE: &str = "Editor › Code Style";
pub const PAGE_LIVE_TEMPLATES: &str = "Editor › Live Templates";
pub const PAGE_RUST: &str = "Languages & Frameworks › Rust";
pub const PAGE_VCS: &str = "Version Control › Commit Message Generation";
pub const PAGE_TOOLS: &str = "Tools › AI Agents";
pub const PAGE_OTHER: &str = "Other";

/// The Settings tree, in display order (`›` separates levels).
pub const PAGES: &[&str] = &[
    PAGE_APPEARANCE,
    PAGE_SYSTEM,
    PAGE_KEYMAP,
    PAGE_EDITOR_GENERAL,
    PAGE_EDITOR_TABS,
    PAGE_CODE_COMPLETION,
    PAGE_FONT,
    PAGE_COLOR_SCHEME,
    PAGE_CODE_STYLE,
    PAGE_LIVE_TEMPLATES,
    PAGE_RUST,
    PAGE_VCS,
    PAGE_TOOLS,
];

pub const THEMES: [&str; 3] = ["Dark", "Light", "High Contrast"];
/// Built-in editor colour schemes (one per theme).
pub const BUILTIN_SCHEMES: [&str; 3] = THEMES;

/// Colour tokens a user scheme may override (the UI maps them onto its theme).
pub const COLOR_TOKENS: &[&str] = &[
    "editor_bg",
    "panel_bg",
    "header_bg",
    "popup_bg",
    "border",
    "separator",
    "text",
    "text_editor",
    "text_muted",
    "text_disabled",
    "accent",
    "list_selection",
    "list_selection_inactive",
    "hover",
    "selection",
    "caret",
    "caret_row",
    "line_number",
    "line_number_active",
    "indent_guide",
    "whitespace",
    "right_margin",
    "find_match",
    "find_match_current",
    "occurrence",
    "bracket_match",
    "error",
    "warning",
    "weak",
    "success",
    "link",
    "console_stderr",
    "console_system",
    "breakpoint",
    "breakpoint_disabled",
    "execution_line",
    "inline_value",
    "inlay_text",
    "inlay_bg",
    "diff_added",
    "diff_removed",
    "diff_added_bg",
    "diff_removed_bg",
    "diff_modified_bg",
    "annotation",
    "vcs_modified",
    "vcs_added",
    "vcs_deleted",
    "vcs_untracked",
    "vcs_ignored",
    "vcs_conflict",
    "vcs_gutter_added",
    "vcs_gutter_modified",
    "vcs_gutter_deleted",
    "vcs_annotation_bg",
    "keyword",
    "string",
    "number",
    "comment",
    "doc_comment",
    "function",
    "macro",
    "type",
    "lifetime",
    "attribute",
    "constant",
    "field",
    "escape",
];

const fn def(
    key: &'static str,
    label: &'static str,
    page: &'static str,
    description: &'static str,
    kind: Kind,
) -> SettingDef {
    SettingDef { key, label, page, description, kind }
}

/// Every documented setting. Fields of [`Settings`] not listed here are still settable by key
/// (validated by their type) and show on the `Other` page.
pub const SETTINGS: &[SettingDef] = &[
    def("theme", "Theme", PAGE_APPEARANCE, "UI theme: Dark, Light or High Contrast", Kind::Theme),
    def(
        "ui_font_size",
        "UI font size",
        PAGE_APPEARANCE,
        "Size of menus, tool windows and dialogs",
        Kind::Float { min: 8.0, max: 32.0 },
    ),
    def(
        "native_menu_bar",
        "Use the macOS global menu bar",
        PAGE_APPEARANCE,
        "Show the menus in the macOS menu bar instead of inside the window (macOS only)",
        Kind::Bool,
    ),
    def(
        "reopen_last_project",
        "Reopen projects on startup",
        PAGE_SYSTEM,
        "Reopen the last project(s) with their windows, tabs and splits when CodeCraft starts",
        Kind::Bool,
    ),
    def("keymap", "Keymap", PAGE_KEYMAP, "macOS or Windows/Linux; empty = the platform default", Kind::Keymap),
    def(
        "keymap_overrides",
        "Shortcut changes",
        PAGE_KEYMAP,
        "{keymap: {command id: [shortcut…]}}",
        Kind::KeymapOverrides,
    ),
    def("soft_wrap", "Soft-wrap files", PAGE_EDITOR_GENERAL, "Wrap long lines at the editor width", Kind::Bool),
    def("show_whitespace", "Show whitespaces", PAGE_EDITOR_GENERAL, "Render spaces and tabs", Kind::Bool),
    def(
        "whitespace_mode",
        "Whitespaces shown",
        PAGE_EDITOR_GENERAL,
        "all, leading_trailing (indentation and line ends) or trailing",
        Kind::Text,
    ),
    def("show_line_numbers", "Show line numbers", PAGE_EDITOR_GENERAL, "Line numbers in the gutter", Kind::Bool),
    def(
        "show_inlay_hints",
        "Show inlay hints",
        PAGE_EDITOR_GENERAL,
        "Type and parameter name hints from the language server",
        Kind::Bool,
    ),
    def(
        "find_scroll_to_results",
        "Scroll to results during typing",
        PAGE_EDITOR_GENERAL,
        "Find selects the first match as you type",
        Kind::Bool,
    ),
    def("show_editor_tabs", "Show tabs", PAGE_EDITOR_TABS, "Show the tab bar above each editor", Kind::Bool),
    def(
        "preview_tab",
        "Enable preview tab",
        PAGE_EDITOR_TABS,
        "A single click in the Project view opens a preview tab that the next one replaces",
        Kind::Bool,
    ),
    def(
        "editor_tab_limit",
        "Tab limit",
        PAGE_EDITOR_TABS,
        "Close unmodified tabs beyond this many per editor group; 0 = no limit",
        Kind::Int { min: 0, max: 100 },
    ),
    def(
        "rename_in_place",
        "Rename in the editor",
        PAGE_EDITOR_GENERAL,
        "Refactor › Rename edits the name in place (Enter refactors, Escape cancels); off = the rename dialog",
        Kind::Bool,
    ),
    def(
        "completion_auto_popup",
        "Show suggestions as you type",
        PAGE_CODE_COMPLETION,
        "Open the completion list automatically after an identifier start or a trigger character (`.`, `::`)",
        Kind::Bool,
    ),
    def(
        "completion_auto_popup_delay_ms",
        "Auto-popup delay (ms)",
        PAGE_CODE_COMPLETION,
        "Wait this long after typing before the list appears",
        Kind::Int { min: 0, max: 2000 },
    ),
    def(
        "completion_argument_placeholders",
        "Insert argument placeholders",
        PAGE_CODE_COMPLETION,
        "Complete functions as `name(a, b)` with Tab stops for the arguments; off = `name()` with the caret inside",
        Kind::Bool,
    ),
    def("editor_font_size", "Size", PAGE_FONT, "Editor font size", Kind::Float { min: 6.0, max: 72.0 }),
    def(
        "line_spacing",
        "Line height",
        PAGE_FONT,
        "Line height as a multiple of the font size",
        Kind::Float { min: 0.8, max: 3.0 },
    ),
    def("color_scheme", "Scheme", PAGE_COLOR_SCHEME, "Editor colour scheme; empty = the theme's", Kind::ColorScheme),
    def(
        "user_schemes",
        "User schemes",
        PAGE_COLOR_SCHEME,
        "[{name, base, colors: {token: #RRGGBB}}]",
        Kind::UserSchemes,
    ),
    def("tab_size", "Tab size", PAGE_CODE_STYLE, "Columns per tab and indent", Kind::Int { min: 1, max: 16 }),
    def(
        "right_margin",
        "Hard wrap at",
        PAGE_CODE_STYLE,
        "Right margin (columns) for Fill Paragraph",
        Kind::Int { min: 10, max: 1000 },
    ),
    def(
        "rust_analyzer_path",
        "rust-analyzer",
        PAGE_RUST,
        "Path to the rust-analyzer binary; empty = detect",
        Kind::OptPath,
    ),
    def(
        "rust_analyzer_low_memory",
        "Low-memory mode",
        PAGE_RUST,
        "Smaller rust-analyzer caches, no eager dependency indexing",
        Kind::Bool,
    ),
    def(
        "lsp_memory_limit_mb",
        "Memory limit (MiB)",
        PAGE_RUST,
        "Restart a language server above this; 0 = never",
        Kind::Int { min: 0, max: 1_048_576 },
    ),
    def("check_on_save", "Run external linter on save", PAGE_RUST, "cargo check (or Clippy) after saving", Kind::Bool),
    def(
        "use_clippy",
        "External linter: Clippy",
        PAGE_RUST,
        "Use Clippy instead of cargo check for diagnostics",
        Kind::Bool,
    ),
    def("commit_ai_provider", "Provider", PAGE_VCS, "openrouter, ollama, claude or openai", Kind::Provider),
    def("commit_ai_model", "Model", PAGE_VCS, "Model name; empty = the provider's default", Kind::Text),
    def("ollama_url", "Ollama URL", PAGE_VCS, "Base URL of the Ollama server", Kind::Text),
    def(
        "commit_ai_keys",
        "API keys",
        PAGE_VCS,
        "{provider: key}; empty = the provider's environment variable",
        Kind::Secrets,
    ),
];

/// The definition of a documented setting.
pub fn find(key: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|d| d.key == key)
}

/// What [`set_value`] changed, for side effects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    /// The language server must restart (linter or rust-analyzer options).
    pub restart_lsp: bool,
}

fn bad(key: &str, msg: impl std::fmt::Display) -> String {
    format!("{key}: {msg}")
}

/// The scheme names valid for `settings` (built-ins, then user schemes).
pub fn scheme_names(settings: &Settings) -> Vec<String> {
    BUILTIN_SCHEMES
        .iter()
        .map(|s| (*s).to_string())
        .chain(settings.user_schemes.iter().map(|s| s.name.clone()))
        .collect()
}

/// The colour scheme the editor uses: `color_scheme`, else the theme's.
pub fn effective_scheme(settings: &Settings) -> String {
    let names = scheme_names(settings);
    if let Some(n) = names.iter().find(|n| **n == settings.color_scheme) {
        return n.clone();
    }
    canonical_theme(&settings.theme).unwrap_or("Dark").to_string()
}

/// The built-in theme called `name` (case-insensitive).
pub fn canonical_theme(name: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase().replace(['-', '_'], " ");
    THEMES.iter().copied().find(|t| t.to_ascii_lowercase() == n || (n == "darcula" && *t == "Dark"))
}

/// `#RRGGBB` (or `RRGGBB`) → canonical `#RRGGBB`.
pub fn parse_color(s: &str) -> Result<String, String> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("`{s}` is not a #RRGGBB colour"));
    }
    Ok(format!("#{}", hex.to_ascii_uppercase()))
}

/// Validate a list of user schemes: unique non-empty names that do not shadow built-ins, a
/// built-in base, known tokens and `#RRGGBB` colours.
pub fn validate_schemes(list: &[ColorScheme]) -> Result<Vec<ColorScheme>, String> {
    let mut out: Vec<ColorScheme> = Vec::new();
    for s in list {
        let name = s.name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err("a colour scheme needs a name of 1–80 characters".into());
        }
        if BUILTIN_SCHEMES.iter().any(|b| b.eq_ignore_ascii_case(name)) || out.iter().any(|o| o.name == name) {
            return Err(format!("a colour scheme named `{name}` already exists"));
        }
        let base = canonical_theme(&s.base).ok_or_else(|| format!("unknown base scheme `{}`", s.base))?;
        let mut colors = std::collections::BTreeMap::new();
        for (token, color) in &s.colors {
            if !COLOR_TOKENS.contains(&token.as_str()) {
                return Err(format!("unknown colour token `{token}`"));
            }
            colors.insert(token.clone(), parse_color(color)?);
        }
        out.push(ColorScheme { name: name.to_string(), base: base.to_string(), colors });
    }
    Ok(out)
}

fn number(key: &str, v: &Value) -> Result<f64, String> {
    v.as_f64().filter(|f| f.is_finite()).ok_or_else(|| bad(key, format!("expected a number, got {v}")))
}

/// Validate `value` for `key` and store it in `settings`. Unknown keys and bad values are errors
/// (nothing changes then).
pub fn set_value(settings: &mut Settings, key: &str, value: &Value) -> Result<Change, String> {
    let mut obj = match serde_json::to_value(&*settings) {
        Ok(Value::Object(m)) => m,
        _ => return Err("settings cannot be serialized".into()),
    };
    if !obj.contains_key(key) {
        return Err(format!("unknown setting `{key}`"));
    }
    let checked: Value = match find(key).map(|d| d.kind) {
        Some(Kind::Bool) => {
            Value::Bool(value.as_bool().ok_or_else(|| bad(key, format!("expected true or false, got {value}")))?)
        }
        Some(Kind::Int { min, max }) => {
            let n = value.as_i64().ok_or_else(|| bad(key, format!("expected an integer, got {value}")))?;
            if !(min..=max).contains(&n) {
                return Err(bad(key, format!("{n} is out of range {min}–{max}")));
            }
            json!(n)
        }
        Some(Kind::Float { min, max }) => {
            let n = number(key, value)?;
            if !(min..=max).contains(&n) {
                return Err(bad(key, format!("{n} is out of range {min}–{max}")));
            }
            json!(n)
        }
        Some(Kind::Text) => {
            let s = value.as_str().ok_or_else(|| bad(key, format!("expected text, got {value}")))?;
            if s.len() > 4096 {
                return Err(bad(key, "too long"));
            }
            json!(s)
        }
        Some(Kind::Provider) => {
            let s = value.as_str().ok_or_else(|| bad(key, format!("expected a provider id, got {value}")))?;
            json!(s.trim().to_ascii_lowercase())
        }
        Some(Kind::OptPath) => match value {
            Value::Null => Value::Null,
            Value::String(s) if s.trim().is_empty() => Value::Null,
            Value::String(s) => json!(s.trim()),
            other => return Err(bad(key, format!("expected a path or null, got {other}"))),
        },
        Some(Kind::Theme) => {
            let s = value.as_str().unwrap_or_default();
            json!(
                canonical_theme(s)
                    .ok_or_else(|| bad(key, format!("unknown theme {value}; use {}", THEMES.join(", "))))?
            )
        }
        Some(Kind::Keymap) => {
            let s = value.as_str().ok_or_else(|| bad(key, format!("expected a keymap name, got {value}")))?;
            if s.trim().is_empty() {
                json!("")
            } else {
                json!(
                    keymaps::canonical_name(s).ok_or_else(|| bad(
                        key,
                        format!("unknown keymap `{s}`; use {}", keymaps::NAMES.join(" or "))
                    ))?
                )
            }
        }
        Some(Kind::ColorScheme) => {
            let s = value.as_str().ok_or_else(|| bad(key, format!("expected a scheme name, got {value}")))?;
            let s = s.trim();
            if s.is_empty() {
                json!("")
            } else if let Some(b) = canonical_theme(s) {
                json!(b)
            } else if settings.user_schemes.iter().any(|u| u.name == s) {
                json!(s)
            } else {
                return Err(bad(key, format!("unknown colour scheme `{s}`")));
            }
        }
        Some(Kind::Secrets) => {
            let m = value.as_object().ok_or_else(|| bad(key, format!("expected {{provider: key}}, got {value}")))?;
            let mut out = Map::new();
            for (k, v) in m {
                let s = v.as_str().ok_or_else(|| bad(key, format!("the key for `{k}` must be text")))?;
                out.insert(k.trim().to_ascii_lowercase(), json!(s.trim()));
            }
            Value::Object(out)
        }
        Some(Kind::KeymapOverrides) => {
            let parsed = serde_json::from_value(value.clone()).map_err(|e| bad(key, e))?;
            serde_json::to_value(keymaps::validate_overrides(&parsed).map_err(|e| bad(key, e))?)
                .map_err(|e| bad(key, e))?
        }
        Some(Kind::UserSchemes) => {
            let parsed: Vec<ColorScheme> = serde_json::from_value(value.clone()).map_err(|e| bad(key, e))?;
            serde_json::to_value(validate_schemes(&parsed).map_err(|e| bad(key, e))?).map_err(|e| bad(key, e))?
        }
        // Undocumented fields: their type decides.
        None => value.clone(),
    };
    obj.insert(key.to_string(), checked);
    let mut next: Settings = serde_json::from_value(Value::Object(obj)).map_err(|e| bad(key, e))?;
    // A removed user scheme cannot stay selected.
    if !scheme_names(&next).contains(&next.color_scheme) {
        next.color_scheme.clear();
    }
    let change = diff(settings, &next);
    *settings = next;
    Ok(change)
}

/// Side effects of going from `old` to `new`.
pub fn diff(old: &Settings, new: &Settings) -> Change {
    Change {
        restart_lsp: old.use_clippy != new.use_clippy
            || old.rust_analyzer_path != new.rust_analyzer_path
            || old.rust_analyzer_low_memory != new.rust_analyzer_low_memory
            || old.completion_argument_placeholders != new.completion_argument_placeholders,
    }
}

/// A setting's value as JSON; API keys are masked.
pub fn get_value(settings: &Settings, key: &str) -> Result<Value, String> {
    let obj = match serde_json::to_value(settings) {
        Ok(Value::Object(m)) => m,
        _ => return Err("settings cannot be serialized".into()),
    };
    let v = obj.get(key).cloned().ok_or_else(|| format!("unknown setting `{key}`"))?;
    Ok(if find(key).is_some_and(|d| d.kind == Kind::Secrets) { mask(&v) } else { v })
}

/// Every setting (masked).
pub fn all_values(settings: &Settings) -> Value {
    let mut obj = match serde_json::to_value(settings) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    for d in SETTINGS.iter().filter(|d| d.kind == Kind::Secrets) {
        if let Some(v) = obj.get_mut(d.key) {
            *v = mask(v);
        }
    }
    Value::Object(obj)
}

fn mask(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    let set = v.as_str().is_some_and(|s| !s.is_empty());
                    (k.clone(), json!(if set { "********" } else { "" }))
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The schema: every setting with its page, label, kind and current value (masked).
pub fn schema(settings: &Settings) -> Vec<Value> {
    let values = all_values(settings);
    let mut out: Vec<Value> = SETTINGS
        .iter()
        .map(|d| {
            json!({
                "key": d.key,
                "label": d.label,
                "page": d.page,
                "description": d.description,
                "kind": format!("{:?}", d.kind),
                "value": values.get(d.key).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    if let Value::Object(m) = &values {
        for (k, v) in m.iter().filter(|(k, _)| find(k).is_none()) {
            out.push(json!({ "key": k, "label": k, "page": PAGE_OTHER, "description": "", "kind": "Raw", "value": v }));
        }
    }
    out
}

/// Whether a setting matches a Settings search (label, key, page or description).
pub fn matches(d: &SettingDef, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    q.is_empty() || [d.key, d.label, d.page, d.description].iter().any(|t| t.to_lowercase().contains(&q))
}

/// Pages that match a search: by name, or because one of their settings does, then the Keymap
/// page when only command names match (its list filters by the same query).
pub fn matching_pages(query: &str) -> Vec<&'static str> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<&'static str> = PAGES
        .iter()
        .copied()
        .filter(|p| {
            q.is_empty() || p.to_lowercase().contains(&q) || SETTINGS.iter().any(|d| d.page == *p && matches(d, &q))
        })
        .collect();
    if !out.contains(&PAGE_KEYMAP) && crate::registry().all().iter().any(|c| c.label.to_lowercase().contains(&q)) {
        out.push(PAGE_KEYMAP);
    }
    out
}

// ---------------------------------------------------------------- dialog and quick switch

/// The Settings dialog's state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingsDialog {
    /// The settings as edited; Apply/OK copy them to the live settings.
    pub draft: Settings,
    pub page: String,
    pub query: String,
    /// The last error from Apply (shown in the dialog).
    pub error: Option<String>,
}

impl SettingsDialog {
    pub fn new(settings: &Settings) -> Self {
        Self { draft: settings.clone(), page: PAGE_APPEARANCE.into(), query: String::new(), error: None }
    }
}

/// What the Quick Switch Scheme popup lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuickLevel {
    /// Editor Color Scheme, Keymap, Theme.
    Root,
    ColorScheme,
    Keymap,
    Theme,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickSwitch {
    pub level: QuickLevel,
    pub selected: usize,
}

/// One row of the Quick Switch popup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuickItem {
    pub label: String,
    /// The current choice (checked).
    pub current: bool,
}

/// The rows at `level`.
pub fn quick_items(settings: &Settings, level: QuickLevel) -> Vec<QuickItem> {
    let item = |label: &str, current: bool| QuickItem { label: label.to_string(), current };
    match level {
        QuickLevel::Root => vec![item("Editor Color Scheme", false), item("Keymap", false), item("Theme", false)],
        QuickLevel::ColorScheme => {
            let cur = effective_scheme(settings);
            scheme_names(settings).iter().map(|n| item(n, *n == cur)).collect()
        }
        QuickLevel::Keymap => {
            let cur = keymaps::active(settings);
            keymaps::NAMES.iter().map(|n| item(n, *n == cur)).collect()
        }
        QuickLevel::Theme => {
            let cur = canonical_theme(&settings.theme).unwrap_or("Dark");
            THEMES.iter().map(|n| item(n, *n == cur)).collect()
        }
    }
}

/// Engine-side state of the settings UI.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PrefsState {
    pub dialog: Option<SettingsDialog>,
    pub quick_switch: Option<QuickSwitch>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_settings_exist_and_have_pages() {
        let obj = serde_json::to_value(Settings::default()).unwrap();
        for d in SETTINGS {
            assert!(obj.get(d.key).is_some(), "{} is not a Settings field", d.key);
            assert!(PAGES.contains(&d.page), "{} has unknown page {}", d.key, d.page);
        }
    }

    #[test]
    fn set_validates_types_and_ranges() {
        let mut s = Settings::default();
        assert!(set_value(&mut s, "no_such", &json!(1)).unwrap_err().contains("unknown setting"));
        assert!(set_value(&mut s, "tab_size", &json!(0)).is_err());
        assert!(set_value(&mut s, "tab_size", &json!("4")).is_err());
        assert!(set_value(&mut s, "editor_font_size", &json!("huge")).is_err());
        assert!(set_value(&mut s, "editor_font_size", &json!(500)).is_err());
        assert!(set_value(&mut s, "soft_wrap", &json!(1)).is_err());
        assert!(set_value(&mut s, "theme", &json!("Neon")).is_err());
        assert!(set_value(&mut s, "keymap", &json!("emacs")).is_err());
        assert!(set_value(&mut s, "color_scheme", &json!("Nope")).is_err());
        assert_eq!(s, Settings::default(), "failed sets change nothing");

        set_value(&mut s, "tab_size", &json!(2)).unwrap();
        set_value(&mut s, "editor_font_size", &json!(15.5)).unwrap();
        set_value(&mut s, "theme", &json!("light")).unwrap();
        set_value(&mut s, "keymap", &json!("windows")).unwrap();
        set_value(&mut s, "color_scheme", &json!("high contrast")).unwrap();
        assert_eq!((s.tab_size, s.editor_font_size), (2, 15.5));
        assert_eq!(
            (s.theme.as_str(), s.keymap.as_str(), s.color_scheme.as_str()),
            ("Light", "Windows/Linux", "High Contrast")
        );
        assert!(set_value(&mut s, "use_clippy", &json!(true)).unwrap().restart_lsp);
        assert!(!set_value(&mut s, "soft_wrap", &json!(true)).unwrap().restart_lsp);
        set_value(&mut s, "rust_analyzer_path", &json!("  ")).unwrap();
        assert_eq!(s.rust_analyzer_path, None);
        set_value(&mut s, "ui_font_size", &json!(14)).unwrap();
        assert_eq!(s.ui_font_size, 14.0);
    }

    #[test]
    fn keys_are_masked_when_read() {
        let mut s = Settings::default();
        set_value(&mut s, "commit_ai_keys", &json!({ "OpenAI": " sk-123 ", "claude": "" })).unwrap();
        assert_eq!(s.commit_ai_keys.get("openai").map(String::as_str), Some("sk-123"));
        let v = get_value(&s, "commit_ai_keys").unwrap();
        assert_eq!(v, json!({ "claude": "", "openai": "********" }));
        assert!(!all_values(&s).to_string().contains("sk-123"));
        assert!(!serde_json::to_string(&schema(&s)).unwrap().contains("sk-123"));
        assert!(set_value(&mut s, "commit_ai_keys", &json!({ "x": 1 })).is_err());
    }

    #[test]
    fn user_schemes_validate_and_select() {
        let mut s = Settings::default();
        let schemes = json!([{ "name": "Mine", "base": "light", "colors": { "keyword": "ff0000" } }]);
        set_value(&mut s, "user_schemes", &schemes).unwrap();
        assert_eq!(s.user_schemes[0].base, "Light");
        assert_eq!(s.user_schemes[0].colors.get("keyword").map(String::as_str), Some("#FF0000"));
        set_value(&mut s, "color_scheme", &json!("Mine")).unwrap();
        assert_eq!(effective_scheme(&s), "Mine");
        for bad in [
            json!([{ "name": "Dark", "base": "Dark" }]),
            json!([{ "name": "A", "base": "Sepia" }]),
            json!([{ "name": "A", "base": "Dark", "colors": { "nope": "#000000" } }]),
            json!([{ "name": "A", "base": "Dark", "colors": { "keyword": "red" } }]),
            json!([{ "name": "A", "base": "Dark" }, { "name": "A", "base": "Dark" }]),
            json!([{ "name": "", "base": "Dark" }]),
        ] {
            assert!(set_value(&mut s, "user_schemes", &bad).is_err(), "{bad}");
        }
        // Removing the selected scheme falls back to the theme's.
        set_value(&mut s, "user_schemes", &json!([])).unwrap();
        assert_eq!(s.color_scheme, "");
        assert_eq!(effective_scheme(&s), "Dark");
    }

    #[test]
    fn search_finds_pages_by_setting_and_command() {
        assert_eq!(matching_pages("").len(), PAGES.len());
        assert!(matching_pages("tab size").contains(&PAGE_CODE_STYLE));
        assert!(matching_pages("clippy").contains(&PAGE_RUST));
        assert!(matching_pages("Find Action").contains(&PAGE_KEYMAP));
        assert!(matching_pages("zzzz-nothing").is_empty());
    }

    #[test]
    fn quick_items_mark_the_current_choice() {
        let s = Settings { keymap: "macOS".into(), theme: "Light".into(), ..Default::default() };
        let cur = |level| quick_items(&s, level).into_iter().find(|i| i.current).map(|i| i.label);
        assert_eq!(cur(QuickLevel::Keymap).as_deref(), Some("macOS"));
        assert_eq!(cur(QuickLevel::Theme).as_deref(), Some("Light"));
        assert_eq!(cur(QuickLevel::ColorScheme).as_deref(), Some("Light"));
        assert_eq!(quick_items(&s, QuickLevel::Root).len(), 3);
    }
}
