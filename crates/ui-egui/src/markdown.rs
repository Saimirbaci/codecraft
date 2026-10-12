//! Minimal Markdown rendering for documentation popups (headings, code blocks, bold, rules).

use egui::{FontId, RichText};

use crate::theme::Theme;

pub fn render(ui: &mut egui::Ui, text: &str, theme: &Theme, mono_size: f32) {
    let mut in_code = false;
    let mut code = String::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                let block = std::mem::take(&mut code);
                egui::Frame::new().fill(theme.editor_bg).inner_margin(6).corner_radius(4).show(ui, |ui| {
                    ui.label(
                        RichText::new(block.trim_end()).font(FontId::monospace(mono_size)).color(theme.text_editor),
                    );
                });
            }
            in_code = !in_code;
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        let t = line.trim();
        if t == "---" || t == "___" {
            ui.separator();
        } else if let Some(h) =
            t.strip_prefix("# ").or_else(|| t.strip_prefix("## ")).or_else(|| t.strip_prefix("### "))
        {
            ui.label(RichText::new(h).strong().size(14.0));
        } else if t.is_empty() {
            ui.add_space(4.0);
        } else {
            let plain = t.replace("**", "").replace('`', "");
            ui.label(RichText::new(plain).color(theme.text));
        }
    }
    if in_code && !code.is_empty() {
        ui.label(RichText::new(code.trim_end()).font(FontId::monospace(mono_size)).color(theme.text_editor));
    }
}
