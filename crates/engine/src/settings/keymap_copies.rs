//! User keymaps (Settings › Keymap › Duplicate…, Rename…, Delete): copies of a keymap that
//! inherit its shortcuts and keep their own changes. Their overrides live in
//! [`Settings::keymap_overrides`] under the keymap's name, like a built-in keymap's.

use codecraft_workspace::{Settings, UserKeymap};

use crate::keymaps::{self, MAX_DEPTH};

/// Longest keymap name.
pub const MAX_NAME: usize = 60;
/// Most user keymaps.
pub const MAX_KEYMAPS: usize = 64;

fn check_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.chars().any(char::is_control) {
        return Err(format!("a keymap needs a name of 1–{MAX_NAME} characters"));
    }
    if keymaps::canonical_name(name).is_some() {
        return Err(format!("`{name}` is the name of a built-in keymap"));
    }
    Ok(name.to_string())
}

/// Validate a list of user keymaps: unique names that are not built-in names, parents that
/// exist (a built-in or another user keymap in the list), no cycles and at most
/// [`MAX_DEPTH`] levels. Parent names are canonicalised.
pub fn validate(list: &[UserKeymap]) -> Result<Vec<UserKeymap>, String> {
    if list.len() > MAX_KEYMAPS {
        return Err(format!("at most {MAX_KEYMAPS} user keymaps"));
    }
    let mut out: Vec<UserKeymap> = Vec::new();
    for k in list {
        let name = check_name(&k.name)?;
        if out.iter().any(|o| o.name == name) {
            return Err(format!("a keymap named `{name}` already exists"));
        }
        out.push(UserKeymap { name, parent: k.parent.trim().to_string() });
    }
    let names: Vec<String> = out.iter().map(|k| k.name.clone()).collect();
    for k in &mut out {
        if names.contains(&k.parent) {
            continue;
        }
        k.parent = keymaps::canonical_name(&k.parent)
            .ok_or_else(|| format!("keymap `{}`: unknown parent keymap `{}`", k.name, k.parent))?
            .to_string();
    }
    for k in &out {
        let mut cur = k.parent.clone();
        let mut depth = 1;
        while let Some(p) = out.iter().find(|o| o.name == cur) {
            if p.name == k.name {
                return Err(format!("keymap `{}` inherits from itself", k.name));
            }
            depth += 1;
            if depth > MAX_DEPTH {
                return Err(format!("keymap `{}` has more than {MAX_DEPTH} parent levels", k.name));
            }
            cur = p.parent.clone();
        }
    }
    Ok(out)
}

/// After the user keymaps changed: drop overrides of keymaps that no longer exist and fall back
/// to the platform keymap if the active one is gone.
pub fn tidy(settings: &mut Settings) {
    let users: Vec<String> = settings.user_keymaps.iter().map(|u| u.name.clone()).collect();
    settings.keymap_overrides.retain(|k, _| users.contains(k) || keymaps::canonical_name(k).is_some());
    if !settings.keymap.trim().is_empty() && keymaps::resolve_name(settings, &settings.keymap).is_none() {
        settings.keymap.clear();
    }
}

/// A free name like `macOS copy`, `macOS copy 2`, ….
pub fn free_name(settings: &Settings, base: &str) -> String {
    let taken = keymaps::all_names(settings);
    (1..=1000)
        .map(|n| if n == 1 { format!("{base} copy") } else { format!("{base} copy {n}") })
        .find(|n| !taken.iter().any(|t| t.eq_ignore_ascii_case(n)))
        .unwrap_or_else(|| format!("{base} copy"))
}

/// Duplicate a keymap (default: the active one) into a new user keymap that inherits from it,
/// and make it active. Returns its name.
pub fn duplicate(settings: &mut Settings, from: Option<&str>, name: Option<&str>) -> Result<String, String> {
    let from = match from.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => keymaps::resolve_name(settings, f).ok_or_else(|| format!("unknown keymap `{f}`"))?,
        None => keymaps::active(settings),
    };
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.to_string(),
        None => free_name(settings, &from),
    };
    let mut list = settings.user_keymaps.clone();
    list.push(UserKeymap { name: name.clone(), parent: from });
    settings.user_keymaps = validate(&list)?;
    settings.keymap = check_name(&name)?;
    Ok(settings.keymap.clone())
}

/// Rename a user keymap (its children and overrides follow).
pub fn rename(settings: &mut Settings, old: &str, new: &str) -> Result<String, String> {
    let old = keymaps::user_keymap(settings, old).map(|u| u.name.clone()).ok_or_else(|| format!("no user keymap `{old}`"))?;
    let new = check_name(new)?;
    if new == old {
        return Ok(new);
    }
    let mut list = settings.user_keymaps.clone();
    for k in &mut list {
        if k.name == old {
            k.name = new.clone();
        }
        if k.parent == old {
            k.parent = new.clone();
        }
    }
    settings.user_keymaps = validate(&list)?;
    if let Some(o) = settings.keymap_overrides.remove(&old) {
        settings.keymap_overrides.insert(new.clone(), o);
    }
    if settings.keymap.trim() == old {
        settings.keymap = new.clone();
    }
    Ok(new)
}

/// Delete a user keymap: keymaps that inherited from it inherit from its parent, and the active
/// keymap falls back to its parent.
pub fn delete(settings: &mut Settings, name: &str) -> Result<(), String> {
    let gone = keymaps::user_keymap(settings, name).cloned().ok_or_else(|| format!("no user keymap `{name}`"))?;
    let list: Vec<UserKeymap> = settings
        .user_keymaps
        .iter()
        .filter(|k| k.name != gone.name)
        .map(|k| UserKeymap {
            name: k.name.clone(),
            parent: if k.parent == gone.name { gone.parent.clone() } else { k.parent.clone() },
        })
        .collect();
    settings.user_keymaps = validate(&list)?;
    settings.keymap_overrides.remove(&gone.name);
    if settings.keymap.trim() == gone.name {
        settings.keymap = gone.parent;
    }
    tidy(settings);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn km(name: &str, parent: &str) -> UserKeymap {
        UserKeymap { name: name.into(), parent: parent.into() }
    }

    #[test]
    fn validation_rejects_bad_names_parents_and_cycles() {
        let ok = validate(&[km(" Mine ", "mac"), km("Child", "Mine")]).unwrap();
        assert_eq!(ok[0], km("Mine", keymaps::MACOS));
        assert_eq!(ok[1].parent, "Mine");
        for bad in [
            vec![km("macOS", "macOS")],
            vec![km("", "macOS")],
            vec![km("A", "Nope")],
            vec![km("A", "macOS"), km("A", "macOS")],
            vec![km("A", "B"), km("B", "A")],
            vec![km("A", "A")],
            vec![km(&"x".repeat(61), "macOS")],
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
        let deep: Vec<UserKeymap> =
            (0..12).map(|i| km(&format!("K{i}"), &if i == 0 { "macOS".into() } else { format!("K{}", i - 1) })).collect();
        assert!(validate(&deep).is_err());
    }

    #[test]
    fn duplicate_rename_delete() {
        let mut s = Settings { keymap: "macOS".into(), ..Default::default() };
        let name = duplicate(&mut s, None, None).unwrap();
        assert_eq!(name, "macOS copy");
        assert_eq!(keymaps::active(&s), "macOS copy");
        let child = duplicate(&mut s, Some("macOS copy"), Some("Child")).unwrap();
        assert_eq!(child, "Child");
        assert_eq!(duplicate(&mut s, Some("macOS"), None).unwrap(), "macOS copy 2");
        assert!(duplicate(&mut s, Some("nope"), None).is_err());
        assert!(duplicate(&mut s, None, Some("Child")).is_err());
        s.keymap = "macOS copy".into();
        keymaps::set_shortcuts(&mut s, "help.findAction", &["F12".into()]).unwrap();
        rename(&mut s, "macOS copy", "Renamed").unwrap();
        assert_eq!(s.keymap, "Renamed");
        assert!(s.keymap_overrides.contains_key("Renamed"));
        assert_eq!(keymaps::user_keymap(&s, "Child").unwrap().parent, "Renamed");
        assert!(rename(&mut s, "Renamed", "Emacs").is_err());
        delete(&mut s, "Renamed").unwrap();
        assert_eq!(s.keymap, keymaps::MACOS);
        assert_eq!(keymaps::user_keymap(&s, "Child").unwrap().parent, keymaps::MACOS);
        assert!(!s.keymap_overrides.contains_key("Renamed"));
        assert!(delete(&mut s, "Renamed").is_err());
    }
}
