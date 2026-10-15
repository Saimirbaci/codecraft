//! Settings dialog, keymaps, colour schemes and Quick Switch Scheme commands (M8).
//!
//! While the Settings dialog is open, `settings.set`, `settings.reset`, `keymap.*` and
//! `colorScheme.*` edit its draft (Apply/OK commit it, Cancel drops it); otherwise they change
//! the live settings at once.

use serde_json::{Value, json};

use codecraft_workspace::{ColorScheme, Settings};

use crate::commands::{CmdResult, CommandSpec, Context, always, ok, simple, spec};
use crate::params::{bool_or, opt_str, req_str};
use crate::settings::{self, QuickLevel, QuickSwitch, SettingsDialog};
use crate::{CommandError, Popup, Session, keymaps, registry};

pub(crate) fn specs() -> Vec<CommandSpec> {
    vec![
        spec(
            "app.settings",
            "Settings…",
            None,
            "{page?: string} — open the Settings dialog (on a page such as `Keymap` or `Editor › Font`)",
            Context::Global,
            always,
            open_dialog,
        ),
        spec(
            "settings.get",
            "Get Setting",
            None,
            "{key?: string, draft?: bool} → one setting's value, or all of them (API keys are masked); `draft` reads the open dialog's draft",
            Context::Global,
            always,
            get,
        ),
        spec(
            "settings.set",
            "Change Setting",
            None,
            "{key: string, value: any} — set one setting (see `settings.list`); unknown keys and bad values are errors. Edits the dialog's draft while Settings is open.",
            Context::Global,
            always,
            set,
        ),
        spec(
            "settings.reset",
            "Reset Setting",
            None,
            "{key?: string} — restore one setting (or all) to its default",
            Context::Global,
            always,
            reset,
        ),
        simple("settings.list", "List Settings", always, |s, _| {
            let source = dialog(s).map(|d| &d.draft).unwrap_or(&s.ws.settings);
            Ok(json!({ "pages": settings::PAGES, "settings": settings::schema(source) }))
        }),
        spec(
            "settings.dialog.page",
            "Settings: Show Page",
            None,
            "{page: string} — one of `settings.list` pages",
            Context::Global,
            has_dialog,
            |s, p| {
                let page = req_str(p, "page")?;
                let page = settings::PAGES
                    .iter()
                    .find(|x| x.eq_ignore_ascii_case(page.trim()))
                    .ok_or_else(|| CommandError::BadParams(format!("unknown settings page `{page}`")))?;
                if let Some(d) = dialog_mut(s) {
                    d.page = (*page).to_string();
                }
                ok()
            },
        ),
        spec(
            "settings.dialog.search",
            "Settings: Search",
            None,
            "{query: string} → the pages that match",
            Context::Global,
            has_dialog,
            |s, p| {
                let query = opt_str(p, "query").unwrap_or_default().chars().take(200).collect::<String>();
                let pages = settings::matching_pages(&query);
                if let Some(d) = dialog_mut(s) {
                    if let Some(first) = pages.first().filter(|_| !pages.iter().any(|x| *x == d.page)) {
                        d.page = (*first).to_string();
                    }
                    d.query = query;
                }
                Ok(json!({ "pages": pages }))
            },
        ),
        simple("settings.dialog.apply", "Settings: Apply", has_dialog, |s, _| {
            apply(s);
            ok()
        }),
        simple("settings.dialog.ok", "Settings: OK", has_dialog, |s, _| {
            apply(s);
            close_dialog(s);
            ok()
        }),
        simple("settings.dialog.cancel", "Settings: Cancel", always, |s, _| {
            close_dialog(s);
            ok()
        }),
        // ------------------------------------------------------------ keymaps
        spec(
            "keymap.list",
            "List Keymap",
            None,
            "{query?: string} → {keymap, keymaps, actions: [{id, label, menu, shortcuts, defaults, overridden}]}",
            Context::Global,
            always,
            keymap_list,
        ),
        spec(
            "keymap.select",
            "Select Keymap",
            None,
            "{name: string} — `macOS`, `Windows/Linux`, `VS Code (macOS)`, `VS Code`, `Emacs` or a user keymap (empty = the platform default)",
            Context::Global,
            always,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                edit(s, |st| settings::set_value(st, "keymap", &json!(name)).map(|_| ()))?;
                ok()
            },
        ),
        spec(
            "keymap.setShortcuts",
            "Set Shortcuts",
            None,
            "{id: string, shortcuts: [string]} — replace a command's shortcuts in the active keymap → {shortcuts, conflicts}",
            Context::Global,
            always,
            |s, p| {
                let id = req_str(p, "id")?.to_string();
                let list: Vec<String> = match p.get("shortcuts") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .map(|v| v.as_str().map(String::from))
                        .collect::<Option<_>>()
                        .ok_or_else(|| CommandError::BadParams("`shortcuts` must be strings".into()))?,
                    _ => return Err(CommandError::BadParams("missing array param `shortcuts`".into())),
                };
                let (set, conflicts) = edit(s, |st| {
                    let set = keymaps::set_shortcuts(st, &id, &list)?;
                    let conflicts: Vec<&str> = set.iter().flat_map(|sc| keymaps::conflicts(st, sc, &id)).collect();
                    Ok((set, conflicts))
                })?;
                Ok(json!({ "shortcuts": set, "conflicts": conflicts }))
            },
        ),
        spec(
            "keymap.addShortcut",
            "Add Keyboard Shortcut",
            None,
            "{id: string, shortcut: string, removeConflicts?: bool} — e.g. `Shift+Cmd+K` → {conflicts: [command id]}",
            Context::Global,
            always,
            |s, p| {
                let id = req_str(p, "id")?.to_string();
                let sc = req_str(p, "shortcut")?.to_string();
                let remove = bool_or(p, "removeConflicts", false);
                let conflicts = edit(s, |st| keymaps::add_shortcut(st, &id, &sc, remove))?;
                if !conflicts.is_empty() {
                    s.status = format!("{sc} is also assigned to {}", labels(&conflicts));
                }
                Ok(json!({ "conflicts": conflicts }))
            },
        ),
        spec(
            "keymap.removeShortcut",
            "Remove Keyboard Shortcut",
            None,
            "{id: string, shortcut: string}",
            Context::Global,
            always,
            |s, p| {
                let id = req_str(p, "id")?.to_string();
                let sc = req_str(p, "shortcut")?.to_string();
                edit(s, |st| keymaps::remove_shortcut(st, &id, &sc))?;
                ok()
            },
        ),
        spec(
            "keymap.reset",
            "Reset Shortcuts",
            None,
            "{id?: string} — drop the changes to one command (or all) in the active keymap",
            Context::Global,
            always,
            |s, p| {
                let id = opt_str(p, "id").map(String::from);
                if let Some(id) = &id
                    && !registry().contains(id)
                {
                    return Err(CommandError::BadParams(format!("unknown command `{id}`")));
                }
                edit(s, |st| {
                    keymaps::reset(st, id.as_deref());
                    Ok(())
                })?;
                ok()
            },
        ),
        spec(
            "keymap.conflicts",
            "Shortcut Conflicts",
            None,
            "{shortcut: string, id?: string} → the commands (other than `id`) that use the shortcut",
            Context::Global,
            always,
            |s, p| {
                let sc = keymaps::normalize(req_str(p, "shortcut")?).map_err(CommandError::BadParams)?;
                let except = opt_str(p, "id").unwrap_or_default();
                let source = dialog(s).map(|d| &d.draft).unwrap_or(&s.ws.settings);
                Ok(json!({ "shortcut": sc, "conflicts": keymaps::conflicts(source, &sc, except) }))
            },
        ),
        // ------------------------------------------------------------ colour schemes
        simple("colorScheme.list", "List Color Schemes", always, |s, _| {
            let st = dialog(s).map(|d| &d.draft).unwrap_or(&s.ws.settings);
            let mut schemes: Vec<Value> = settings::BUILTIN_SCHEMES
                .iter()
                .map(|n| json!({ "name": n, "builtin": true, "base": n, "colors": {} }))
                .collect();
            schemes.extend(
                st.user_schemes
                    .iter()
                    .map(|u| json!({ "name": u.name, "builtin": false, "base": u.base, "colors": u.colors })),
            );
            Ok(json!({
                "current": settings::effective_scheme(st),
                "theme": st.theme,
                "schemes": schemes,
                "tokens": settings::COLOR_TOKENS,
            }))
        }),
        spec(
            "colorScheme.select",
            "Select Color Scheme",
            None,
            "{name: string} — a built-in (Dark, Light, High Contrast) or user scheme; empty = the theme's",
            Context::Global,
            always,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                edit(s, |st| settings::set_value(st, "color_scheme", &json!(name)).map(|_| ()))?;
                ok()
            },
        ),
        spec(
            "colorScheme.duplicate",
            "Duplicate Color Scheme",
            None,
            "{name?: string, from?: string} — copy a scheme (default: the current one) into a new user scheme and select it → {name}",
            Context::Global,
            always,
            duplicate_scheme,
        ),
        spec(
            "colorScheme.setColor",
            "Set Scheme Color",
            None,
            "{token: string, color: \"#RRGGBB\", scheme?: string} — change one colour of a user scheme (default: the current scheme)",
            Context::Global,
            always,
            |s, p| {
                let token = req_str(p, "token")?.to_string();
                let color = settings::parse_color(req_str(p, "color")?).map_err(CommandError::BadParams)?;
                let scheme = opt_str(p, "scheme").map(String::from);
                edit(s, |st| {
                    if !settings::COLOR_TOKENS.contains(&token.as_str()) {
                        return Err(format!("unknown colour token `{token}`"));
                    }
                    user_scheme_mut(st, scheme.as_deref())?.colors.insert(token, color);
                    Ok(())
                })?;
                ok()
            },
        ),
        spec(
            "colorScheme.resetColor",
            "Reset Scheme Color",
            None,
            "{token?: string, scheme?: string} — restore one colour (or all) of a user scheme to its base",
            Context::Global,
            always,
            |s, p| {
                let token = opt_str(p, "token").map(String::from);
                let scheme = opt_str(p, "scheme").map(String::from);
                edit(s, |st| {
                    let u = user_scheme_mut(st, scheme.as_deref())?;
                    match &token {
                        Some(t) => {
                            u.colors.remove(t);
                        }
                        None => u.colors.clear(),
                    }
                    Ok(())
                })?;
                ok()
            },
        ),
        spec(
            "colorScheme.delete",
            "Delete Color Scheme",
            None,
            "{name: string} — delete a user scheme",
            Context::Global,
            always,
            |s, p| {
                let name = req_str(p, "name")?.to_string();
                edit(s, |st| {
                    let before = st.user_schemes.len();
                    let rest: Vec<ColorScheme> = st.user_schemes.iter().filter(|u| u.name != name).cloned().collect();
                    if rest.len() == before {
                        return Err(format!("no user colour scheme `{name}`"));
                    }
                    settings::set_value(st, "user_schemes", &json!(rest)).map(|_| ())
                })?;
                ok()
            },
        ),
        // ------------------------------------------------------------ quick switch
        simple("view.quickSwitchScheme", "Quick Switch Scheme…", always, |s, _| {
            s.prefs.quick_switch = Some(QuickSwitch { level: QuickLevel::Root, selected: 0 });
            ok()
        }),
        spec(
            "quickSwitch.move",
            "Quick Switch: Move",
            None,
            "{delta: int} — move the selection (wraps)",
            Context::Global,
            has_quick_switch,
            |s, p| {
                let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(1).clamp(-1000, 1000);
                let Some(q) = s.prefs.quick_switch.clone() else { return ok() };
                let n = settings::quick_items(&s.ws.settings, q.level).len() as i64;
                if n > 0 {
                    let sel = (q.selected as i64).saturating_add(delta).rem_euclid(n);
                    if let Some(qs) = &mut s.prefs.quick_switch {
                        qs.selected = usize::try_from(sel).unwrap_or(0);
                    }
                }
                ok()
            },
        ),
        spec(
            "quickSwitch.accept",
            "Quick Switch: Choose",
            None,
            "{index?: int} — open a submenu (Editor Color Scheme, Keymap, Theme) or apply the choice",
            Context::Global,
            has_quick_switch,
            quick_accept,
        ),
        simple("quickSwitch.back", "Quick Switch: Back", has_quick_switch, |s, _| {
            if let Some(q) = &mut s.prefs.quick_switch {
                if q.level == QuickLevel::Root {
                    s.prefs.quick_switch = None;
                } else {
                    *q = QuickSwitch { level: QuickLevel::Root, selected: 0 };
                }
            }
            ok()
        }),
        simple("quickSwitch.close", "Quick Switch: Close", always, |s, _| {
            s.prefs.quick_switch = None;
            ok()
        }),
        simple("quickSwitch.items", "Quick Switch: Items", has_quick_switch, |s, _| {
            let Some(q) = &s.prefs.quick_switch else { return ok() };
            Ok(
                json!({ "level": q.level, "selected": q.selected, "items": settings::quick_items(&s.ws.settings, q.level) }),
            )
        }),
    ]
}

/// Settings › Rust › Run external linter on save: `cargo check` (or Clippy) after Save All in a
/// Cargo project, unless a Cargo process is already running.
pub(crate) fn check_on_save(s: &mut Session) {
    if !s.ws.settings.check_on_save || s.power_save() || s.runs.any_running() {
        return;
    }
    let Some(root) = s.ws.project.as_ref().and_then(|p| p.cargo.as_ref()).map(|m| m.workspace_root.clone()) else {
        return;
    };
    let (cmd, title) = if s.ws.settings.use_clippy {
        (codecraft_cargo::CargoCommand::Clippy, "Clippy")
    } else {
        (codecraft_cargo::CargoCommand::Check, "Check")
    };
    let inv = codecraft_cargo::CargoInvocation::new(cmd, root);
    if let Err(e) = crate::runs::start_chain(s, inv, None, title, true) {
        s.status = format!("{title} on save: {e}");
    }
}

// ---------------------------------------------------------------- helpers

pub(crate) fn dialog(s: &Session) -> Option<&SettingsDialog> {
    if matches!(s.popup, Some(Popup::Settings)) { s.prefs.dialog.as_ref() } else { None }
}

pub(crate) fn dialog_mut(s: &mut Session) -> Option<&mut SettingsDialog> {
    if matches!(s.popup, Some(Popup::Settings)) { s.prefs.dialog.as_mut() } else { None }
}

fn has_dialog(s: &Session) -> Result<(), String> {
    dialog(s).map(|_| ()).ok_or_else(|| "the Settings dialog is not open".into())
}

fn has_quick_switch(s: &Session) -> Result<(), String> {
    s.prefs.quick_switch.as_ref().map(|_| ()).ok_or_else(|| "Quick Switch Scheme is not open".into())
}

/// Run `f` on the dialog's draft (Settings open) or the live settings; nothing changes when it
/// fails.
pub(crate) fn edit<T>(s: &mut Session, f: impl FnOnce(&mut Settings) -> Result<T, String>) -> Result<T, CommandError> {
    if let Some(d) = dialog_mut(s) {
        let mut next = d.draft.clone();
        let out = f(&mut next).map_err(CommandError::BadParams)?;
        d.draft = next;
        d.error = None;
        return Ok(out);
    }
    let old = s.ws.settings.clone();
    let mut next = old.clone();
    let out = f(&mut next).map_err(CommandError::BadParams)?;
    s.ws.settings = next;
    after_change(s, &old);
    Ok(out)
}

/// Side effects of a change to the live settings.
fn after_change(s: &mut Session, old: &Settings) {
    let change = settings::diff(old, &s.ws.settings);
    if change.restart_lsp
        && let Some(root) = s.ws.project.as_ref().map(|p| p.root.clone())
    {
        s.start_lsp(&root);
    }
    // Languages & Frameworks › Python › Language server and type checking (M15 EA).
    if old.python_language_server != s.ws.settings.python_language_server
        || old.python_type_checking != s.ws.settings.python_type_checking
    {
        s.restart_python_server();
    }
    // Languages & Frameworks › Rust › Crates.io lookups (only where the host allows the network).
    if old.crates_io_lookups != s.ws.settings.crates_io_lookups {
        apply_crates_io_lookups(s);
    }
}

/// Make crates.io lookups follow the setting, when the host allows network lookups at all.
pub(crate) fn apply_crates_io_lookups(s: &mut Session) {
    let on = s.ws.settings.crates_io_lookups && s.prefs_services.network_lookups;
    s.rust.deps.online = on;
    s.rust.deps.auto = on;
}

fn open_dialog(s: &mut Session, p: &Value) -> CmdResult {
    let mut d = SettingsDialog::new(&s.ws.settings);
    if let Some(page) = opt_str(p, "page") {
        d.page = settings::PAGES
            .iter()
            .find(|x| x.eq_ignore_ascii_case(page.trim()))
            .ok_or_else(|| CommandError::BadParams(format!("unknown settings page `{page}`")))?
            .to_string();
    }
    s.prefs.dialog = Some(d);
    s.popup = Some(Popup::Settings);
    ok()
}

fn apply(s: &mut Session) {
    let Some(draft) = dialog(s).map(|d| d.draft.clone()) else { return };
    let old = std::mem::replace(&mut s.ws.settings, draft);
    after_change(s, &old);
    s.status = "Settings applied".into();
    // Code Style formatter options go to the project's files.
    if let Err(e) = crate::prefs_cmds::write_code_style(s) {
        s.status = e.clone();
        if let Some(d) = dialog_mut(s) {
            d.error = Some(e);
        }
    }
}

fn close_dialog(s: &mut Session) {
    s.prefs.dialog = None;
    if matches!(s.popup, Some(Popup::Settings)) {
        s.popup = None;
    }
}

fn get(s: &mut Session, p: &Value) -> CmdResult {
    let source = if bool_or(p, "draft", false) {
        &dialog(s).ok_or_else(|| CommandError::Disabled("the Settings dialog is not open".into()))?.draft
    } else {
        &s.ws.settings
    };
    match opt_str(p, "key") {
        Some(key) => settings::get_value(source, key).map_err(CommandError::BadParams),
        None => Ok(settings::all_values(source)),
    }
}

fn set(s: &mut Session, p: &Value) -> CmdResult {
    let key = req_str(p, "key")?.to_string();
    let value = p.get("value").cloned().unwrap_or(Value::Null);
    edit(s, |st| settings::set_value(st, &key, &value).map(|_| ()))?;
    ok()
}

fn reset(s: &mut Session, p: &Value) -> CmdResult {
    let defaults = Settings::default();
    match opt_str(p, "key").map(String::from) {
        Some(key) => {
            let v = settings::get_value(&defaults, &key).map_err(CommandError::BadParams)?;
            edit(s, |st| settings::set_value(st, &key, &v).map(|_| ()))?;
        }
        None => edit(s, |st| {
            *st = defaults;
            Ok(())
        })?,
    }
    ok()
}

fn labels(ids: &[&str]) -> String {
    ids.iter().map(|id| registry().get(id).map(|c| c.label).unwrap_or(id)).collect::<Vec<_>>().join(", ")
}

fn keymap_list(s: &mut Session, p: &Value) -> CmdResult {
    let st = dialog(s).map(|d| &d.draft).unwrap_or(&s.ws.settings);
    let query = opt_str(p, "query").unwrap_or_default().trim().to_lowercase();
    let keymap = keymaps::active(st);
    let reg = registry();
    let actions: Vec<Value> = reg
        .all()
        .iter()
        .filter_map(|c| {
            let shortcuts = keymaps::shortcuts(st, c.id);
            let menu = reg.menu_path(c.id).map(|m| m.join(" › ")).unwrap_or_default();
            let hit = query.is_empty()
                || c.label.to_lowercase().contains(&query)
                || c.id.to_lowercase().contains(&query)
                || menu.to_lowercase().contains(&query)
                || shortcuts.iter().any(|sc| sc.to_lowercase().contains(&query));
            hit.then(|| {
                json!({
                    "id": c.id,
                    "label": c.label,
                    "menu": menu,
                    "shortcuts": shortcuts,
                    "defaults": keymaps::inherited(st, c.id),
                    "overridden": keymaps::is_overridden(st, c.id),
                })
            })
        })
        .collect();
    let user: Vec<Value> = st.user_keymaps.iter().map(|u| json!({ "name": u.name, "parent": u.parent })).collect();
    Ok(json!({
        "keymap": keymap,
        "keymaps": keymaps::all_names(st),
        "builtin": keymaps::NAMES,
        "user": user,
        "parent": keymaps::parent_of(st, &keymap),
        "actions": actions,
    }))
}

/// The user scheme `name` (default: the current scheme); built-in schemes are read-only.
fn user_scheme_mut<'a>(st: &'a mut Settings, name: Option<&str>) -> Result<&'a mut ColorScheme, String> {
    let name = name.map(String::from).unwrap_or_else(|| settings::effective_scheme(st));
    if settings::canonical_theme(&name).is_some() {
        return Err(format!("`{name}` is a built-in scheme; duplicate it to change colours"));
    }
    st.user_schemes.iter_mut().find(|u| u.name == name).ok_or_else(|| format!("no user colour scheme `{name}`"))
}

fn duplicate_scheme(s: &mut Session, p: &Value) -> CmdResult {
    let from = opt_str(p, "from").map(String::from);
    let wanted = opt_str(p, "name").map(String::from);
    let name = edit(s, |st| {
        let from = from.unwrap_or_else(|| settings::effective_scheme(st));
        let source = match settings::canonical_theme(&from) {
            Some(b) => ColorScheme { name: b.to_string(), base: b.to_string(), colors: Default::default() },
            None => st
                .user_schemes
                .iter()
                .find(|u| u.name == from)
                .cloned()
                .ok_or_else(|| format!("unknown colour scheme `{from}`"))?,
        };
        let taken = settings::scheme_names(st);
        let name = match wanted.map(|w| w.trim().to_string()).filter(|w| !w.is_empty()) {
            Some(w) => w,
            None => (1..)
                .map(|n| if n == 1 { format!("{} copy", source.name) } else { format!("{} copy {n}", source.name) })
                .find(|n| !taken.contains(n))
                .unwrap_or_default(),
        };
        let mut list = st.user_schemes.clone();
        list.push(ColorScheme { name: name.clone(), base: source.base, colors: source.colors });
        settings::set_value(st, "user_schemes", &json!(list))?;
        settings::set_value(st, "color_scheme", &json!(name))?;
        Ok(name)
    })?;
    Ok(json!({ "name": name }))
}

fn quick_accept(s: &mut Session, p: &Value) -> CmdResult {
    let Some(q) = s.prefs.quick_switch.clone() else { return ok() };
    let items = settings::quick_items(&s.ws.settings, q.level);
    let index = crate::params::opt_usize(p, "index")?.unwrap_or(q.selected);
    let item = items.get(index).ok_or_else(|| CommandError::BadParams(format!("no item {index}")))?.label.clone();
    let (key, label) = match q.level {
        QuickLevel::Root => {
            let level = match index {
                0 => QuickLevel::ColorScheme,
                1 => QuickLevel::Keymap,
                _ => QuickLevel::Theme,
            };
            let selected = settings::quick_items(&s.ws.settings, level).iter().position(|i| i.current).unwrap_or(0);
            s.prefs.quick_switch = Some(QuickSwitch { level, selected });
            return ok();
        }
        QuickLevel::ColorScheme => ("color_scheme", "Color scheme"),
        QuickLevel::Keymap => ("keymap", "Keymap"),
        QuickLevel::Theme => ("theme", "Theme"),
    };
    edit(s, |st| settings::set_value(st, key, &json!(item)).map(|_| ()))?;
    s.prefs.quick_switch = None;
    s.status = format!("{label}: {item}");
    Ok(json!({ key: item }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        let mut s = Session::new();
        s.lsp_enabled = false;
        s
    }

    #[test]
    fn settings_set_get_and_errors() {
        let mut s = session();
        s.execute("settings.set", &json!({ "key": "tab_size", "value": 2 })).unwrap();
        assert_eq!(s.ws.settings.tab_size, 2);
        assert_eq!(s.execute("settings.get", &json!({ "key": "tab_size" })).unwrap(), json!(2));
        assert!(matches!(
            s.execute("settings.set", &json!({ "key": "nope", "value": 1 })),
            Err(CommandError::BadParams(_))
        ));
        assert!(matches!(
            s.execute("settings.set", &json!({ "key": "tab_size", "value": 99 })),
            Err(CommandError::BadParams(_))
        ));
        assert!(s.execute("settings.get", &json!({ "key": "nope" })).is_err());
        s.execute("settings.reset", &json!({ "key": "tab_size" })).unwrap();
        assert_eq!(s.ws.settings.tab_size, 4);
        let list = s.execute("settings.list", &Value::Null).unwrap();
        assert!(list["settings"].as_array().unwrap().iter().any(|d| d["key"] == "keymap"));
    }

    #[test]
    fn dialog_apply_ok_cancel() {
        let mut s = session();
        s.execute("app.settings", &json!({ "page": "editor › font" })).unwrap();
        assert_eq!(s.prefs.dialog.as_ref().unwrap().page, settings::PAGE_FONT);
        assert!(s.execute("app.settings", &json!({ "page": "Nope" })).is_err());
        s.execute("app.settings", &Value::Null).unwrap();
        s.execute("settings.set", &json!({ "key": "editor_font_size", "value": 20 })).unwrap();
        // The draft changed, the live settings did not.
        assert_eq!(s.ws.settings.editor_font_size, 13.0);
        assert_eq!(
            s.execute("settings.get", &json!({ "key": "editor_font_size", "draft": true })).unwrap(),
            json!(20.0)
        );
        s.execute("settings.dialog.apply", &Value::Null).unwrap();
        assert_eq!(s.ws.settings.editor_font_size, 20.0);
        s.execute("settings.set", &json!({ "key": "tab_size", "value": 8 })).unwrap();
        s.execute("settings.dialog.cancel", &Value::Null).unwrap();
        assert_eq!(s.ws.settings.tab_size, 4);
        assert!(s.popup.is_none() && s.prefs.dialog.is_none());
        s.execute("app.settings", &Value::Null).unwrap();
        s.execute("keymap.select", &json!({ "name": "Windows/Linux" })).unwrap();
        s.execute("settings.dialog.ok", &Value::Null).unwrap();
        assert_eq!(s.ws.settings.keymap, keymaps::WINDOWS_LINUX);
        assert!(s.popup.is_none());
        assert!(s.execute("settings.dialog.apply", &Value::Null).is_err());
        // Search moves to a matching page.
        s.execute("app.settings", &Value::Null).unwrap();
        let r = s.execute("settings.dialog.search", &json!({ "query": "clippy" })).unwrap();
        assert_eq!(r["pages"], json!([settings::PAGE_RUST, settings::PAGE_KEYMAP]));
        assert_eq!(s.prefs.dialog.as_ref().unwrap().page, settings::PAGE_RUST);
    }

    #[test]
    fn keymap_commands() {
        let mut s = session();
        s.execute("keymap.select", &json!({ "name": "macOS" })).unwrap();
        let r = s.execute("keymap.addShortcut", &json!({ "id": "help.findAction", "shortcut": "cmd+l" })).unwrap();
        assert_eq!(r["conflicts"], json!(["navigate.lineColumn"]));
        let list = s.execute("keymap.list", &json!({ "query": "find action" })).unwrap();
        let fa = list["actions"].as_array().unwrap().iter().find(|a| a["id"] == "help.findAction").unwrap();
        assert_eq!(fa["shortcuts"], json!(["Shift+Cmd+A", "Cmd+L"]));
        assert_eq!(fa["overridden"], json!(true));
        let c = s.execute("keymap.conflicts", &json!({ "shortcut": "Cmd+L" })).unwrap();
        assert_eq!(c["conflicts"].as_array().unwrap().len(), 2);
        s.execute("keymap.removeShortcut", &json!({ "id": "help.findAction", "shortcut": "Cmd+L" })).unwrap();
        s.execute("keymap.setShortcuts", &json!({ "id": "help.findAction", "shortcuts": ["F12"] })).unwrap();
        assert_eq!(keymaps::shortcuts(&s.ws.settings, "help.findAction"), vec!["F12"]);
        s.execute("keymap.reset", &json!({ "id": "help.findAction" })).unwrap();
        assert!(s.ws.settings.keymap_overrides.is_empty());
        for bad in [
            json!({ "id": "nope", "shortcut": "F1" }),
            json!({ "id": "help.findAction", "shortcut": "Cmd+Nope" }),
            json!({ "id": "help.findAction" }),
        ] {
            assert!(matches!(s.execute("keymap.addShortcut", &bad), Err(CommandError::BadParams(_))), "{bad}");
        }
        assert!(s.execute("keymap.select", &json!({ "name": "teco" })).is_err());
        s.execute("keymap.select", &json!({ "name": "emacs" })).unwrap();
        assert_eq!(s.ws.settings.keymap, keymaps::EMACS);
    }

    #[test]
    fn color_scheme_commands() {
        let mut s = session();
        assert!(s.execute("colorScheme.setColor", &json!({ "token": "keyword", "color": "#ff0000" })).is_err());
        let r = s.execute("colorScheme.duplicate", &Value::Null).unwrap();
        assert_eq!(r["name"], "Dark copy");
        assert_eq!(s.ws.settings.color_scheme, "Dark copy");
        s.execute("colorScheme.setColor", &json!({ "token": "keyword", "color": "#ff0000" })).unwrap();
        assert!(s.execute("colorScheme.setColor", &json!({ "token": "bogus", "color": "#ff0000" })).is_err());
        assert!(s.execute("colorScheme.setColor", &json!({ "token": "keyword", "color": "nope" })).is_err());
        let list = s.execute("colorScheme.list", &Value::Null).unwrap();
        assert_eq!(list["current"], "Dark copy");
        assert_eq!(list["schemes"][3]["colors"]["keyword"], "#FF0000");
        s.execute("colorScheme.select", &json!({ "name": "Light" })).unwrap();
        s.execute("colorScheme.delete", &json!({ "name": "Dark copy" })).unwrap();
        assert!(s.ws.settings.user_schemes.is_empty());
        assert!(s.execute("colorScheme.delete", &json!({ "name": "Dark copy" })).is_err());
    }

    #[test]
    fn quick_switch_scheme_switches_theme() {
        let mut s = session();
        assert!(s.execute("quickSwitch.accept", &Value::Null).is_err());
        s.execute("view.quickSwitchScheme", &Value::Null).unwrap();
        s.execute("quickSwitch.move", &json!({ "delta": 2 })).unwrap();
        s.execute("quickSwitch.accept", &Value::Null).unwrap();
        let items = s.execute("quickSwitch.items", &Value::Null).unwrap();
        assert_eq!(items["level"], "Theme");
        s.execute("quickSwitch.accept", &json!({ "index": 1 })).unwrap();
        assert_eq!(s.ws.settings.theme, "Light");
        assert!(s.prefs.quick_switch.is_none());
        // Keymap through the popup.
        s.execute("view.quickSwitchScheme", &Value::Null).unwrap();
        s.execute("quickSwitch.accept", &json!({ "index": 1 })).unwrap();
        s.execute("quickSwitch.accept", &json!({ "index": 1 })).unwrap();
        assert_eq!(s.ws.settings.keymap, keymaps::WINDOWS_LINUX);
        s.execute("view.quickSwitchScheme", &Value::Null).unwrap();
        s.execute("quickSwitch.back", &Value::Null).unwrap();
        assert!(s.prefs.quick_switch.is_none());
    }
}
