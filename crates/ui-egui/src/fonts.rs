//! Fonts: the platform's UI and monospace fonts when available (never fonts from other apps'
//! bundles), falling back to egui's built-ins.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// User-installed coding fonts first (e.g. JetBrains Mono from its own OFL release), then the
/// system monospace fonts.
fn mono_candidates() -> Vec<(PathBuf, u32)> {
    let mut v = Vec::new();
    for dir in [home().map(|h| h.join("Library/Fonts")), Some(PathBuf::from("/Library/Fonts"))].into_iter().flatten() {
        for name in ["JetBrainsMono-Regular.ttf", "JetBrainsMonoNL-Regular.ttf", "FiraCode-Regular.ttf"] {
            v.push((dir.join(name), 0));
        }
    }
    v.push(("/System/Library/Fonts/SFNSMono.ttf".into(), 0));
    v.push(("/System/Library/Fonts/Menlo.ttc".into(), 0));
    v.push(("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf".into(), 0));
    v.push(("C:\\Windows\\Fonts\\consola.ttf".into(), 0));
    v
}

fn ui_candidates() -> Vec<(PathBuf, u32)> {
    vec![
        ("/System/Library/Fonts/SFNS.ttf".into(), 0),
        ("/System/Library/Fonts/Helvetica.ttc".into(), 0),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into(), 0),
        ("C:\\Windows\\Fonts\\segoeui.ttf".into(), 0),
    ]
}

fn load(path: &Path, index: u32) -> Option<FontData> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 1024 {
        return None;
    }
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    Some(data)
}

/// Install fonts; returns the monospace font's name for the About box.
pub fn install(ctx: &egui::Context) -> String {
    let mut defs = FontDefinitions::default();
    let mut mono_name = "Hack (built-in)".to_string();
    if let Some((path, data)) = mono_candidates().into_iter().find_map(|(p, i)| load(&p, i).map(|d| (p, d))) {
        let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "mono".into());
        defs.font_data.insert(name.clone(), Arc::new(data));
        defs.families.entry(FontFamily::Monospace).or_default().insert(0, name.clone());
        mono_name = name;
    }
    if let Some((path, data)) = ui_candidates().into_iter().find_map(|(p, i)| load(&p, i).map(|d| (p, d))) {
        let name = format!("ui-{}", path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        defs.font_data.insert(name.clone(), Arc::new(data));
        defs.families.entry(FontFamily::Proportional).or_default().insert(0, name);
    }
    ctx.set_fonts(defs);
    mono_name
}
