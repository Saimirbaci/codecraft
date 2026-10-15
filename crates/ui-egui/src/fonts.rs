//! Fonts: the platform's UI and monospace fonts when available (never fonts from other apps'
//! bundles), falling back to egui's built-ins, plus the editor font chosen in Settings › Editor ›
//! Font, put first in the monospace family so missing glyphs fall back to the others.
//!
//! The renderer aborts on font data it cannot parse, so every font is checked first (the same
//! parse the renderer does); an unusable file is skipped and reported, never handed over.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{FontData, FontDefinitions, FontFamily};

use codecraft_engine::settings::fonts::LoadedFont;

/// The name the editor font is registered under.
pub const EDITOR_FONT: &str = "codecraft-editor-font";

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

/// Whether the renderer can use these bytes as the font at `index`.
pub fn usable(bytes: &[u8], index: u32) -> bool {
    bytes.len() >= 12 && skrifa::FontRef::from_index(bytes, index).is_ok()
}

fn load(path: &Path, index: u32) -> Option<FontData> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 1024 || !usable(&bytes, index) {
        return None;
    }
    let mut data = FontData::from_owned(bytes);
    data.index = index;
    Some(data)
}

/// The built-in font setup (platform UI and monospace fonts over egui's) and the monospace
/// font's name for the About box.
pub fn base_definitions() -> (FontDefinitions, String) {
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
    add_symbol_fallbacks(&mut defs, &mono_name);
    (defs, mono_name)
}

/// The platform's symbol font (arrows, ⊶, ⤺…), searched after everything else.
fn symbol_candidates() -> Vec<(PathBuf, u32)> {
    vec![
        ("/System/Library/Fonts/Apple Symbols.ttf".into(), 0),
        ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf".into(), 0),
        ("C:\\Windows\\Fonts\\seguisym.ttf".into(), 0),
    ]
}

/// Glyphs the UI uses for buttons and icons (`⇄`, `↶`, `↵`, `◆`, `⊘`, `⊶`, `⤺`) are in the
/// platform's monospace and symbol fonts but not in its UI font or egui's: without these
/// fallbacks they were drawn as empty boxes (QA M15). The monospace font goes at the end of the
/// proportional family, the symbol font at the end of both.
fn add_symbol_fallbacks(defs: &mut FontDefinitions, mono_name: &str) {
    if defs.font_data.contains_key(mono_name) {
        let prop = defs.families.entry(FontFamily::Proportional).or_default();
        if !prop.iter().any(|n| n == mono_name) {
            prop.push(mono_name.to_string());
        }
    }
    if let Some((path, data)) = symbol_candidates().into_iter().find_map(|(p, i)| load(&p, i).map(|d| (p, d))) {
        let name = format!("symbols-{}", path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        defs.font_data.insert(name.clone(), Arc::new(data));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            let list = defs.families.entry(family).or_default();
            if !list.contains(&name) {
                list.push(name.clone());
            }
        }
    }
}

/// Install the built-in fonts; returns them and the monospace font's name.
pub fn install(ctx: &egui::Context) -> (FontDefinitions, String) {
    let (defs, mono_name) = base_definitions();
    ctx.set_fonts(defs.clone());
    (defs, mono_name)
}

/// `base` with the editor font first in the monospace family (the others stay as fallbacks).
pub fn with_editor_font(base: &FontDefinitions, font: &LoadedFont) -> Result<FontDefinitions, String> {
    if !usable(&font.data, font.index) {
        return Err(format!("{} cannot be used as a font; using the default font", font.path.display()));
    }
    let mut defs = base.clone();
    let mut data = FontData::from_owned(font.data.as_ref().clone());
    data.index = font.index;
    defs.font_data.insert(EDITOR_FONT.into(), Arc::new(data));
    let mono = defs.families.entry(FontFamily::Monospace).or_default();
    mono.retain(|n| n != EDITOR_FONT);
    mono.insert(0, EDITOR_FONT.into());
    Ok(defs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loaded(bytes: Vec<u8>) -> LoadedFont {
        LoadedFont { family: "X".into(), path: "x.ttf".into(), index: 0, data: Arc::new(bytes) }
    }

    #[test]
    fn broken_fonts_are_refused_and_good_ones_go_first() {
        let base = FontDefinitions::default();
        assert!(with_editor_font(&base, &loaded(b"definitely not a font".to_vec())).is_err());
        assert!(with_editor_font(&base, &loaded(Vec::new())).is_err());
        let hack = base.font_data.get("Hack").map(|d| d.font.to_vec()).unwrap();
        let defs = with_editor_font(&base, &loaded(hack)).unwrap();
        assert_eq!(defs.families[&FontFamily::Monospace][0], EDITOR_FONT);
        assert!(defs.families[&FontFamily::Monospace].len() > 1, "the defaults stay as fallbacks");
        let ctx = egui::Context::default();
        ctx.set_fonts(defs);
        let mut out = ctx.run_ui(Default::default(), |ui| {
            ui.label(egui::RichText::new("fn main() {} ✓ 🦀").monospace());
        });
        out.textures_delta.clear();
    }
}
