//! Keymaps: named shortcut sets plus the user's changes (Settings › Keymap).
//!
//! `macOS` is RustRover's macOS keymap: each command's own shortcut, else the observed catalog
//! shortcut. `Windows/Linux` is RustRover's default (Ctrl-based) keymap: the bindings JetBrains
//! documents publicly for Windows/Linux where they differ from macOS, and `Cmd` → `Ctrl` for the
//! rest. User overrides (per keymap, per command) live in [`Settings::keymap_overrides`] and
//! replace a command's whole shortcut list.
//!
//! Shortcuts are text in RustRover's notation (`Shift+Cmd+F`, `Ctrl+Alt+L`, `F2`, `Shift Shift`
//! for a double press); [`normalize`] canonicalises them (modifiers in the order Ctrl, Alt,
//! Shift, Cmd), so equal shortcuts compare equal.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::OnceLock;

use codecraft_workspace::Settings;

use crate::commands::{Context, registry};

pub const MACOS: &str = "macOS";
pub const WINDOWS_LINUX: &str = "Windows/Linux";
/// Every built-in keymap.
pub const NAMES: [&str; 2] = [MACOS, WINDOWS_LINUX];
/// The double-Shift gesture (Search Everywhere).
pub const DOUBLE_SHIFT: &str = "Shift Shift";

/// The keymap a fresh install uses on this platform.
pub fn platform_default() -> &'static str {
    if cfg!(target_os = "macos") { MACOS } else { WINDOWS_LINUX }
}

/// The built-in keymap called `name` (case-insensitive; `mac`, `windows`, `linux`, `default` work).
pub fn canonical_name(name: &str) -> Option<&'static str> {
    let n = name.trim().to_ascii_lowercase();
    match n.as_str() {
        "macos" | "mac" | "mac os x" | "macos keymap" => Some(MACOS),
        "windows/linux" | "windows" | "linux" | "default" | "windows / linux" | "pc" => Some(WINDOWS_LINUX),
        _ => None,
    }
}

/// The active keymap of `settings` (an empty or unknown name means the platform default).
pub fn active(settings: &Settings) -> &'static str {
    canonical_name(&settings.keymap).unwrap_or_else(platform_default)
}

// ---------------------------------------------------------------- shortcut text

/// Key names [`normalize`] accepts besides letters, digits and `F1`–`F12`.
const NAMED_KEYS: &[(&str, &str)] = &[
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
    ("home", "Home"),
    ("end", "End"),
    ("pageup", "PageUp"),
    ("pagedown", "PageDown"),
    ("backspace", "Backspace"),
    ("delete", "Delete"),
    ("del", "Delete"),
    ("enter", "Enter"),
    ("return", "Enter"),
    ("tab", "Tab"),
    ("escape", "Escape"),
    ("esc", "Escape"),
    ("space", "Space"),
    ("insert", "Insert"),
];

const PUNCTUATION: &str = "/\\[],.=-+`;'?";

fn canonical_key(key: &str) -> Option<String> {
    let mut chars = key.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Some(c.to_ascii_uppercase().to_string());
        }
        if PUNCTUATION.contains(c) {
            return Some(c.to_string());
        }
        return None;
    }
    let lower = key.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return (1..=12).contains(&n).then(|| format!("F{n}"));
    }
    NAMED_KEYS.iter().find(|(k, _)| *k == lower).map(|(_, v)| (*v).to_string())
}

/// The parts of a shortcut: `(ctrl, alt, shift, cmd, key)`.
fn parts(s: &str) -> Result<(bool, bool, bool, bool, String), String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty shortcut".into());
    }
    let (mods, key) = match t.strip_suffix("++") {
        Some(m) => (m, "+"),
        None => t.rsplit_once('+').unwrap_or(("", t)),
    };
    let key = canonical_key(key.trim()).ok_or_else(|| format!("unknown key `{}` in `{t}`", key.trim()))?;
    let (mut ctrl, mut alt, mut shift, mut cmd) = (false, false, false, false);
    for m in mods.split('+').map(str::trim).filter(|m| !m.is_empty()) {
        let flag = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => &mut ctrl,
            "alt" | "option" | "opt" => &mut alt,
            "shift" => &mut shift,
            "cmd" | "meta" | "command" => &mut cmd,
            _ => return Err(format!("unknown modifier `{m}` in `{t}`")),
        };
        *flag = true;
    }
    Ok((ctrl, alt, shift, cmd, key))
}

fn join(ctrl: bool, alt: bool, shift: bool, cmd: bool, key: &str) -> String {
    let mut out = String::new();
    for (on, name) in [(ctrl, "Ctrl+"), (alt, "Alt+"), (shift, "Shift+"), (cmd, "Cmd+")] {
        if on {
            out.push_str(name);
        }
    }
    out.push_str(key);
    out
}

/// Canonical form of a shortcut (`cmd+shift+f` → `Shift+Cmd+F`), or why it is not one.
pub fn normalize(s: &str) -> Result<String, String> {
    if s.split_whitespace().collect::<Vec<_>>() == ["Shift", "Shift"] {
        return Ok(DOUBLE_SHIFT.into());
    }
    if s.trim().contains(' ') {
        return Err(format!("`{}` is not a shortcut (use `+` between keys)", s.trim()));
    }
    let (ctrl, alt, shift, cmd, key) = parts(s)?;
    Ok(join(ctrl, alt, shift, cmd, &key))
}

/// The Windows/Linux equivalent of a macOS shortcut when it is a mechanical `Cmd` → `Ctrl`
/// swap. Shortcuts that already use `Ctrl` on macOS have no mechanical equivalent (they would
/// collide with the `Cmd` ones), so they need an explicit [`WINDOWS`] entry.
fn translate(mac: &str) -> Option<String> {
    if mac == DOUBLE_SHIFT {
        return Some(DOUBLE_SHIFT.into());
    }
    let (ctrl, alt, shift, cmd, key) = parts(mac).ok()?;
    if ctrl {
        return None;
    }
    Some(join(cmd, alt, shift, false, &key))
}

// ---------------------------------------------------------------- built-in keymaps

/// RustRover's default Windows/Linux bindings where they are not a `Cmd` → `Ctrl` swap of the
/// macOS ones (from JetBrains' public keymap reference). An empty list means "no shortcut".
pub const WINDOWS: &[(&str, &[&str])] = &[
    // RustRover menu / File
    ("app.settings", &["Ctrl+Alt+S"]),
    ("app.quitRustRover", &[]),
    ("app.quitAndKeepWindows", &[]),
    ("file.saveAll", &["Ctrl+S"]),
    ("file.reloadAllFromDisk", &["Ctrl+Alt+Y"]),
    // Edit
    ("edit.find.findNextOccurrence", &["F3"]),
    ("edit.find.findPreviousOccurrence", &["Shift+F3"]),
    ("edit.find.selectAllOccurrences", &["Ctrl+Alt+Shift+J"]),
    ("edit.find.addSelectionForNextOccurrence", &["Alt+J"]),
    ("edit.find.unselectOccurrence", &["Alt+Shift+J"]),
    ("edit.extendSelection", &["Ctrl+W"]),
    ("edit.shrinkSelection", &["Ctrl+Shift+W"]),
    ("edit.joinLines", &["Ctrl+Shift+J"]),
    ("edit.columnSelectionMode", &["Alt+Shift+Insert"]),
    ("edit.searchInSelection", &[]),
    ("edit.find.searchInSelection", &[]),
    // Editor caret and deletion
    ("editor.deleteWordBackward", &["Ctrl+Backspace"]),
    ("editor.deleteWordForward", &["Ctrl+Delete"]),
    ("editor.deleteLine", &["Ctrl+Y"]),
    ("editor.previousWord", &["Ctrl+Left"]),
    ("editor.nextWord", &["Ctrl+Right"]),
    ("editor.previousWordWithSelection", &["Ctrl+Shift+Left"]),
    ("editor.nextWordWithSelection", &["Ctrl+Shift+Right"]),
    ("editor.lineStart", &[]),
    ("editor.lineEnd", &[]),
    ("editor.lineStartWithSelection", &[]),
    ("editor.lineEndWithSelection", &[]),
    // View
    ("view.toolWindows.project", &["Alt+1"]),
    ("view.toolWindows.bookmarks", &["Alt+2"]),
    ("view.toolWindows.find", &["Alt+3"]),
    ("view.toolWindows.run", &["Alt+4"]),
    ("view.toolWindows.debug", &["Alt+5"]),
    ("view.toolWindows.problems", &["Alt+6"]),
    ("view.toolWindows.structure", &["Alt+7"]),
    ("view.toolWindows.services", &["Alt+8"]),
    ("view.toolWindows.git", &["Alt+9"]),
    ("view.toolWindows.commit", &["Alt+0"]),
    ("view.quickDocumentation", &["Ctrl+Q"]),
    ("view.quickDefinition", &["Ctrl+Shift+I"]),
    ("view.increaseFontSizeInAllEditors", &["Alt+Shift+."]),
    ("view.decreaseFontSizeInAllEditors", &["Alt+Shift+,"]),
    ("view.quickSwitchScheme", &["Ctrl+`"]),
    // Navigate
    ("navigate.back", &["Ctrl+Alt+Left"]),
    ("navigate.forward", &["Ctrl+Alt+Right"]),
    ("navigate.type", &["Ctrl+N"]),
    ("navigate.file", &["Ctrl+Shift+N"]),
    ("navigate.symbol", &["Ctrl+Alt+Shift+N"]),
    ("navigate.lineColumn", &["Ctrl+G"]),
    ("navigate.navigateInFile.moveCaretToMatchingBrace", &["Ctrl+Shift+M"]),
    ("navigate.navigateInFile.nextMethod", &["Alt+Down"]),
    ("navigate.navigateInFile.previousMethod", &["Alt+Up"]),
    ("navigate.jumpToNavigationBar", &["Alt+Home"]),
    ("navigate.callHierarchy", &["Ctrl+Alt+H"]),
    ("navigate.typeHierarchy", &["Ctrl+H"]),
    ("navigate.relatedSymbol", &["Ctrl+Alt+Home"]),
    // Code
    ("code.commentWithBlockComment", &["Ctrl+Shift+/"]),
    ("code.codeCompletion.basic", &["Ctrl+Space"]),
    ("code.autoIndentLines", &["Ctrl+Alt+I"]),
    ("code.optimizeImports", &["Ctrl+Alt+O"]),
    ("code.generate", &["Alt+Insert"]),
    ("code.implementMethods", &["Ctrl+I"]),
    ("code.overrideMethods", &["Ctrl+O"]),
    // Refactor
    ("refactor.refactorThis", &["Ctrl+Alt+Shift+T"]),
    ("refactor.safeDelete", &["Alt+Delete"]),
    // Build / Run / Debug
    ("build.buildProject", &["Ctrl+F9"]),
    ("run.run", &["Shift+F10"]),
    ("run.chooseAndRun", &["Alt+Shift+F10"]),
    ("run.contextConfiguration", &["Ctrl+Shift+F10"]),
    ("run.rerun", &["Ctrl+F5"]),
    ("run.debug", &["Shift+F9"]),
    ("run.chooseAndDebug", &["Alt+Shift+F9"]),
    ("debug.atCaret", &["Ctrl+Shift+F9"]),
    ("run.debuggingActions.resume", &["F9"]),
    // Window
    ("window.editorTabs.closeTab", &["Ctrl+F4"]),
    ("window.editorTabs.selectNextTab", &["Alt+Right"]),
    ("window.editorTabs.selectPreviousTab", &["Alt+Left"]),
    // Git
    ("git.vcsOperationsPopup", &["Alt+`"]),
    ("git.newBranch", &[]),
    ("vcs.nextChange", &["Ctrl+Alt+Shift+Down"]),
    ("vcs.previousChange", &["Ctrl+Alt+Shift+Up"]),
    // Database
    ("db.complete", &["Ctrl+Space"]),
    // Bookmarks (F3 is Find Next here). Ctrl+0…9 are the same as on macOS, but macOS Ctrl
    // shortcuts are never translated mechanically, so they are listed.
    ("edit.bookmarks.toggle", &["F11"]),
    ("edit.bookmarks.toggleWithMnemonic", &["Ctrl+F11"]),
    ("edit.bookmarks.show", &["Shift+F11"]),
    ("edit.bookmarks.goto0", &["Ctrl+0"]),
    ("edit.bookmarks.goto1", &["Ctrl+1"]),
    ("edit.bookmarks.goto2", &["Ctrl+2"]),
    ("edit.bookmarks.goto3", &["Ctrl+3"]),
    ("edit.bookmarks.goto4", &["Ctrl+4"]),
    ("edit.bookmarks.goto5", &["Ctrl+5"]),
    ("edit.bookmarks.goto6", &["Ctrl+6"]),
    ("edit.bookmarks.goto7", &["Ctrl+7"]),
    ("edit.bookmarks.goto8", &["Ctrl+8"]),
    ("edit.bookmarks.goto9", &["Ctrl+9"]),
    // Local history and scratch files
    ("file.new.scratchFile", &["Ctrl+Alt+Shift+Insert"]),
];

fn windows_table() -> &'static HashMap<&'static str, &'static [&'static str]> {
    static T: OnceLock<HashMap<&'static str, &'static [&'static str]>> = OnceLock::new();
    T.get_or_init(|| WINDOWS.iter().copied().collect())
}

/// Every shortcut the explicit Windows/Linux table assigns (mechanical translations that collide
/// with one of these are dropped).
fn windows_explicit() -> &'static HashSet<String> {
    static T: OnceLock<HashSet<String>> = OnceLock::new();
    T.get_or_init(|| WINDOWS.iter().flat_map(|(_, scs)| scs.iter()).filter_map(|s| normalize(s).ok()).collect())
}

/// The built-in shortcuts of a command in `keymap` (no user overrides).
pub fn defaults(keymap: &str, id: &str) -> Vec<String> {
    let mac = registry().shortcuts(id);
    if canonical_name(keymap) == Some(MACOS) {
        return dedup(mac.iter().filter_map(|s| normalize(s).ok()));
    }
    if let Some(explicit) = windows_table().get(id) {
        return dedup(explicit.iter().filter_map(|s| normalize(s).ok()));
    }
    let taken = windows_explicit();
    dedup(mac.iter().filter_map(|s| translate(s)).filter(|s| !taken.contains(s)))
}

fn dedup(it: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in it {
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// The user's overrides for `keymap`, if any.
pub fn overrides<'a>(settings: &'a Settings, keymap: &str) -> Option<&'a BTreeMap<String, Vec<String>>> {
    settings.keymap_overrides.get(keymap)
}

/// The effective shortcuts of a command in the active keymap (overrides applied).
pub fn shortcuts(settings: &Settings, id: &str) -> Vec<String> {
    let keymap = active(settings);
    match overrides(settings, keymap).and_then(|o| o.get(id)) {
        Some(list) => dedup(list.iter().filter_map(|s| normalize(s).ok())),
        None => defaults(keymap, id),
    }
}

/// Whether the user changed this command's shortcuts in the active keymap.
pub fn is_overridden(settings: &Settings, id: &str) -> bool {
    overrides(settings, active(settings)).is_some_and(|o| o.contains_key(id))
}

/// One bound shortcut.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyBinding {
    pub shortcut: String,
    pub id: &'static str,
    pub context: Context,
}

/// Every binding of the active keymap, in match order. For one key in one context the first
/// wins: user overrides, then commands' own shortcuts (and the explicit Windows/Linux table),
/// then catalog shortcuts (so e.g. Backspace is the editor's backspace, not Edit › Delete).
pub fn bindings(settings: &Settings) -> Vec<KeyBinding> {
    let keymap = active(settings);
    let user = overrides(settings, keymap);
    let windows = keymap == WINDOWS_LINUX;
    let rank = |spec: &crate::CommandSpec| {
        if user.is_some_and(|o| o.contains_key(spec.id)) {
            0
        } else if spec.shortcut.is_some()
            || !spec.alt_shortcuts.is_empty()
            || (windows && windows_table().contains_key(spec.id))
        {
            1
        } else {
            2
        }
    };
    let mut specs: Vec<(u8, &crate::CommandSpec)> = registry().all().iter().map(|s| (rank(s), s)).collect();
    specs.sort_by_key(|(r, _)| *r);
    let mut out: Vec<KeyBinding> = Vec::new();
    let mut seen: HashSet<(String, bool)> = HashSet::new();
    for (_, spec) in specs {
        for sc in shortcuts(settings, spec.id) {
            if seen.insert((sc.clone(), spec.context == Context::Editor)) {
                out.push(KeyBinding { shortcut: sc, id: spec.id, context: spec.context });
            }
        }
    }
    out
}

/// Commands other than `except` that have `shortcut` in the active keymap.
pub fn conflicts(settings: &Settings, shortcut: &str, except: &str) -> Vec<&'static str> {
    let Ok(sc) = normalize(shortcut) else { return Vec::new() };
    registry()
        .all()
        .iter()
        .filter(|spec| spec.id != except && shortcuts(settings, spec.id).contains(&sc))
        .map(|spec| spec.id)
        .collect()
}

/// Every command's shortcut list in the active keymap (only commands that have one).
pub fn table(settings: &Settings) -> HashMap<&'static str, Vec<String>> {
    registry()
        .all()
        .iter()
        .filter_map(|spec| {
            let list = shortcuts(settings, spec.id);
            (!list.is_empty()).then_some((spec.id, list))
        })
        .collect()
}

// ---------------------------------------------------------------- editing overrides

/// Replace a command's shortcuts in the active keymap (validated; an override equal to the
/// defaults is dropped). Unknown commands and malformed shortcuts are errors.
pub fn set_shortcuts(settings: &mut Settings, id: &str, list: &[String]) -> Result<Vec<String>, String> {
    if !registry().contains(id) {
        return Err(format!("unknown command `{id}`"));
    }
    let list = list.iter().map(|s| normalize(s)).collect::<Result<Vec<_>, _>>()?;
    let list = dedup(list.into_iter());
    let keymap = active(settings);
    let entry = settings.keymap_overrides.entry(keymap.to_string()).or_default();
    if list == defaults(keymap, id) {
        entry.remove(id);
    } else {
        entry.insert(id.to_string(), list.clone());
    }
    if entry.is_empty() {
        settings.keymap_overrides.remove(keymap);
    }
    Ok(list)
}

/// Add a shortcut to a command; with `remove_conflicts`, take it away from every other command.
/// Returns the commands that (still) share it.
pub fn add_shortcut(
    settings: &mut Settings,
    id: &str,
    shortcut: &str,
    remove_conflicts: bool,
) -> Result<Vec<&'static str>, String> {
    let sc = normalize(shortcut)?;
    let mut list = shortcuts(settings, id);
    if !list.contains(&sc) {
        list.push(sc.clone());
    }
    set_shortcuts(settings, id, &list)?;
    let others = conflicts(settings, &sc, id);
    if !remove_conflicts {
        return Ok(others);
    }
    for other in others {
        let rest: Vec<String> = shortcuts(settings, other).into_iter().filter(|s| *s != sc).collect();
        set_shortcuts(settings, other, &rest)?;
    }
    Ok(Vec::new())
}

/// Remove one shortcut from a command (an error if it does not have it).
pub fn remove_shortcut(settings: &mut Settings, id: &str, shortcut: &str) -> Result<(), String> {
    let sc = normalize(shortcut)?;
    let list = shortcuts(settings, id);
    if !list.contains(&sc) {
        return Err(format!("`{id}` has no shortcut {sc}"));
    }
    let rest: Vec<String> = list.into_iter().filter(|s| *s != sc).collect();
    set_shortcuts(settings, id, &rest).map(|_| ())
}

/// Drop the user's changes for one command (or all of them) in the active keymap.
pub fn reset(settings: &mut Settings, id: Option<&str>) {
    let keymap = active(settings);
    match id {
        Some(id) => {
            if let Some(o) = settings.keymap_overrides.get_mut(keymap) {
                o.remove(id);
                if o.is_empty() {
                    settings.keymap_overrides.remove(keymap);
                }
            }
        }
        None => {
            settings.keymap_overrides.remove(keymap);
        }
    }
}

/// Validate a whole overrides map (from `settings.set keymap_overrides` or a settings file):
/// keymap names and shortcuts are canonicalised, unknown commands are errors.
pub fn validate_overrides(
    map: &BTreeMap<String, BTreeMap<String, Vec<String>>>,
) -> Result<BTreeMap<String, BTreeMap<String, Vec<String>>>, String> {
    let mut out = BTreeMap::new();
    for (keymap, cmds) in map {
        let name = canonical_name(keymap).ok_or_else(|| format!("unknown keymap `{keymap}`"))?;
        let mut checked = BTreeMap::new();
        for (id, list) in cmds {
            if !registry().contains(id) {
                return Err(format!("unknown command `{id}`"));
            }
            let list = list.iter().map(|s| normalize(s)).collect::<Result<Vec<_>, _>>()?;
            checked.insert(id.clone(), dedup(list.into_iter()));
        }
        out.insert(name.to_string(), checked);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(keymap: &str) -> Settings {
        Settings { keymap: keymap.into(), ..Default::default() }
    }

    #[test]
    fn normalize_canonicalises_and_rejects_garbage() {
        assert_eq!(normalize("cmd+shift+f").unwrap(), "Shift+Cmd+F");
        assert_eq!(normalize("Alt+Shift+Cmd+E").unwrap(), "Alt+Shift+Cmd+E");
        assert_eq!(normalize("Shift+Alt+Cmd+E").unwrap(), "Alt+Shift+Cmd+E");
        assert_eq!(normalize("Option+Return").unwrap(), "Alt+Enter");
        assert_eq!(normalize("Ctrl++").unwrap(), "Ctrl++");
        assert_eq!(normalize("f12").unwrap(), "F12");
        assert_eq!(normalize("Shift  Shift").unwrap(), DOUBLE_SHIFT);
        for bad in ["", "Cmd+", "Hyper+X", "Cmd+F13", "Cmd+ü", "Cmd+Shift Shift", "Ctrl+Bogus", "+"] {
            assert!(normalize(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn every_registered_shortcut_normalizes() {
        for spec in registry().all() {
            for s in registry().shortcuts(spec.id) {
                assert!(normalize(s).is_ok(), "{} `{s}`", spec.id);
            }
        }
    }

    #[test]
    fn explicit_windows_entries_name_real_commands() {
        let mut seen = HashSet::new();
        for (id, scs) in WINDOWS {
            assert!(registry().contains(id) || registry().catalog_item(id).is_some(), "unknown id {id}");
            assert!(seen.insert(*id), "duplicate {id}");
            for s in *scs {
                assert!(normalize(s).is_ok(), "{id} `{s}`");
            }
        }
    }

    #[test]
    fn macos_keymap_is_the_registry() {
        let s = with(MACOS);
        assert_eq!(shortcuts(&s, "help.findAction"), vec!["Shift+Cmd+A"]);
        assert_eq!(shortcuts(&s, "app.settings"), vec!["Cmd+,"]);
        assert_eq!(shortcuts(&s, "navigate.searchEverywhere"), vec![DOUBLE_SHIFT]);
    }

    #[test]
    fn windows_keymap_uses_rustrover_defaults() {
        let s = with("windows");
        let one = |id: &str| shortcuts(&s, id);
        assert_eq!(one("help.findAction"), vec!["Ctrl+Shift+A"]);
        assert_eq!(one("navigate.searchEverywhere"), vec![DOUBLE_SHIFT]);
        assert_eq!(one("navigate.type"), vec!["Ctrl+N"]);
        assert_eq!(one("navigate.file"), vec!["Ctrl+Shift+N"]);
        assert_eq!(one("view.recentFiles"), vec!["Ctrl+E"]);
        assert_eq!(one("code.reformatCode"), vec!["Ctrl+Alt+L"]);
        assert_eq!(one("run.run"), vec!["Shift+F10"]);
        assert_eq!(one("run.debug"), vec!["Shift+F9"]);
        assert_eq!(one("code.commentWithLineComment"), vec!["Ctrl+/"]);
        assert_eq!(one("edit.duplicateSelection"), vec!["Ctrl+D"]);
        assert_eq!(one("editor.deleteLine"), vec!["Ctrl+Y"]);
        assert_eq!(one("app.settings"), vec!["Ctrl+Alt+S"]);
        assert_eq!(one("view.quickDocumentation"), vec!["Ctrl+Q"]);
        assert_eq!(one("navigate.lineColumn"), vec!["Ctrl+G"]);
        assert_eq!(one("refactor.rename"), vec!["Shift+F6"]);
        assert_eq!(one("edit.copy"), vec!["Ctrl+C"]);
        assert!(one("app.quitRustRover").is_empty());
        // macOS Ctrl shortcuts never leak into the Ctrl-based keymap (Ctrl+V is Paste there).
        assert_eq!(one("git.vcsOperationsPopup"), vec!["Alt+`"]);
        assert!(one("edit.paste").contains(&"Ctrl+V".to_string()));
        assert!(shortcuts(&s, "navigate.declarationOrUsages").contains(&"Ctrl+B".to_string()));
        assert_eq!(one("edit.bookmarks.toggle"), vec!["F11"]);
        assert_eq!(one("edit.bookmarks.show"), vec!["Shift+F11"]);
        assert_eq!(one("edit.bookmarks.goto3"), vec!["Ctrl+3"]);
        assert_eq!(one("edit.find.findNextOccurrence"), vec!["F3"]);
    }

    #[test]
    fn windows_keymap_has_no_global_collisions_for_core_actions() {
        let s = with(WINDOWS_LINUX);
        let b = bindings(&s);
        for sc in ["Ctrl+Shift+A", "Ctrl+N", "Ctrl+E", "Ctrl+Alt+L", "Ctrl+D", "Ctrl+Y", "Ctrl+V", "Ctrl+G", "Ctrl+Q"] {
            let hits: Vec<_> = registry()
                .all()
                .iter()
                .filter(|spec| spec.context == Context::Global && shortcuts(&s, spec.id).contains(&sc.to_string()))
                .map(|spec| spec.id)
                .collect();
            assert!(hits.len() <= 1, "{sc} is bound to {hits:?}");
            assert!(b.iter().any(|k| k.shortcut == sc), "{sc} unbound");
        }
    }

    #[test]
    fn overrides_win_and_reset() {
        let mut s = with(MACOS);
        set_shortcuts(&mut s, "navigate.lineColumn", &["Cmd+Shift+L".into()]).unwrap();
        assert_eq!(shortcuts(&s, "navigate.lineColumn"), vec!["Shift+Cmd+L"]);
        assert!(is_overridden(&s, "navigate.lineColumn"));
        // Overrides belong to their keymap.
        s.keymap = WINDOWS_LINUX.into();
        assert_eq!(shortcuts(&s, "navigate.lineColumn"), vec!["Ctrl+G"]);
        s.keymap = MACOS.into();
        // Setting the defaults back drops the override.
        set_shortcuts(&mut s, "navigate.lineColumn", &["Cmd+L".into()]).unwrap();
        assert!(!is_overridden(&s, "navigate.lineColumn"));
        assert!(s.keymap_overrides.is_empty());
        set_shortcuts(&mut s, "navigate.lineColumn", &[]).unwrap();
        assert!(shortcuts(&s, "navigate.lineColumn").is_empty());
        reset(&mut s, Some("navigate.lineColumn"));
        assert_eq!(shortcuts(&s, "navigate.lineColumn"), vec!["Cmd+L"]);
        assert!(set_shortcuts(&mut s, "no.such.command", &[]).is_err());
        assert!(set_shortcuts(&mut s, "navigate.lineColumn", &["Cmd+Nope".into()]).is_err());
    }

    #[test]
    fn override_takes_the_key_in_bindings() {
        let mut s = with(MACOS);
        // Cmd+L is Go to Line; give it to Find Action too.
        let conflicts = add_shortcut(&mut s, "help.findAction", "Cmd+L", false).unwrap();
        assert_eq!(conflicts, vec!["navigate.lineColumn"]);
        let b = bindings(&s);
        let hit = b.iter().find(|k| k.shortcut == "Cmd+L" && k.context == Context::Global).unwrap();
        assert_eq!(hit.id, "help.findAction");
        // Removing conflicts takes it away from Go to Line.
        let left = add_shortcut(&mut s, "help.findAction", "Cmd+L", true).unwrap();
        assert!(left.is_empty());
        assert!(shortcuts(&s, "navigate.lineColumn").is_empty());
        assert_eq!(shortcuts(&s, "help.findAction"), vec!["Shift+Cmd+A", "Cmd+L"]);
        remove_shortcut(&mut s, "help.findAction", "cmd+l").unwrap();
        assert!(remove_shortcut(&mut s, "help.findAction", "Cmd+L").is_err());
    }

    #[test]
    fn editor_backspace_beats_edit_delete_in_both_keymaps() {
        for keymap in NAMES {
            let b = bindings(&with(keymap));
            let hit = b.iter().find(|k| k.shortcut == "Backspace" && k.context == Context::Editor).unwrap();
            assert_eq!(hit.id, "editor.backspace", "{keymap}");
        }
    }

    #[test]
    fn overrides_validation() {
        let mut m = BTreeMap::new();
        m.insert("mac".to_string(), BTreeMap::from([("help.findAction".to_string(), vec!["cmd+k".to_string()])]));
        let v = validate_overrides(&m).unwrap();
        assert_eq!(v.get(MACOS).and_then(|o| o.get("help.findAction")), Some(&vec!["Cmd+K".to_string()]));
        m.insert("emacs".into(), BTreeMap::new());
        assert!(validate_overrides(&m).is_err());
    }

    #[test]
    fn platform_default_applies_to_empty_and_unknown_names() {
        assert_eq!(active(&Settings::default()), platform_default());
        assert_eq!(active(&with("bogus")), platform_default());
        assert_eq!(active(&with("Windows/Linux")), WINDOWS_LINUX);
    }
}
