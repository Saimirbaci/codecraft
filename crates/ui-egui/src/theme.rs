//! Design tokens. Colours approximate RustRover's "New UI" Dark and Light looks by eye, plus a
//! High Contrast variant (clean-room: no scheme files are copied). Every colour the UI paints
//! comes from here. The UI theme comes from `Settings::theme`; the editor's colours from the
//! effective colour scheme (a built-in one or a user scheme with token overrides).

use egui::Color32;

use codecraft_engine::settings;
use codecraft_syntax::HighlightKind;
use codecraft_workspace::Settings;

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb(((rgb >> 16) & 0xff) as u8, ((rgb >> 8) & 0xff) as u8, (rgb & 0xff) as u8)
}

#[derive(Clone, Debug)]
pub struct Theme {
    /// Dark (egui's dark visuals) or light.
    pub dark: bool,
    pub editor_bg: Color32,
    pub panel_bg: Color32,
    pub header_bg: Color32,
    pub popup_bg: Color32,
    pub border: Color32,
    pub separator: Color32,
    pub text: Color32,
    pub text_editor: Color32,
    pub text_muted: Color32,
    pub text_disabled: Color32,
    pub accent: Color32,
    pub list_selection: Color32,
    pub list_selection_inactive: Color32,
    pub hover: Color32,
    pub selection: Color32,
    pub caret: Color32,
    pub caret_row: Color32,
    pub line_number: Color32,
    pub line_number_active: Color32,
    pub indent_guide: Color32,
    /// Show Whitespaces marks (· for spaces, → for tabs).
    pub whitespace: Color32,
    pub right_margin: Color32,
    pub find_match: Color32,
    pub find_match_current: Color32,
    pub occurrence: Color32,
    pub bracket_match: Color32,
    pub error: Color32,
    pub warning: Color32,
    pub weak: Color32,
    pub success: Color32,
    pub link: Color32,
    pub console_stderr: Color32,
    pub console_system: Color32,
    /// Terminal ANSI colours 0–15 (black, red, green, yellow, blue, magenta, cyan, white, then
    /// the bright variants); the terminal's default colours are `editor_bg`/`text_editor`.
    pub terminal_ansi: [Color32; 16],
    // debugger
    pub breakpoint: Color32,
    pub breakpoint_disabled: Color32,
    pub execution_line: Color32,
    pub inline_value: Color32,
    // inlay hints (`: i32`, parameter names)
    pub inlay_text: Color32,
    pub inlay_bg: Color32,
    // diffs (agent review, VCS)
    pub diff_added: Color32,
    pub diff_removed: Color32,
    pub diff_added_bg: Color32,
    pub diff_removed_bg: Color32,
    pub diff_modified_bg: Color32,
    // annotation overlay
    pub annotation: Color32,
    // version control: file status colours (project tree, tabs, change lists) and gutter markers
    pub vcs_modified: Color32,
    pub vcs_added: Color32,
    pub vcs_deleted: Color32,
    pub vcs_untracked: Color32,
    pub vcs_ignored: Color32,
    pub vcs_conflict: Color32,
    pub vcs_gutter_added: Color32,
    pub vcs_gutter_modified: Color32,
    pub vcs_gutter_deleted: Color32,
    pub vcs_annotation_bg: Color32,
    // syntax
    pub keyword: Color32,
    pub string: Color32,
    pub number: Color32,
    pub comment: Color32,
    pub doc_comment: Color32,
    pub function: Color32,
    pub macro_: Color32,
    pub type_: Color32,
    pub lifetime: Color32,
    pub attribute: Color32,
    pub constant: Color32,
    pub field: Color32,
    pub escape: Color32,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            dark: true,
            editor_bg: hex(0x1E1F22),
            panel_bg: hex(0x2B2D30),
            header_bg: hex(0x2B2D30),
            popup_bg: hex(0x2B2D30),
            border: hex(0x1E1F22),
            separator: hex(0x393B40),
            text: hex(0xDFE1E5),
            text_editor: hex(0xBCBEC4),
            text_muted: hex(0x868A91),
            text_disabled: hex(0x5A5D63),
            accent: hex(0x3574F0),
            list_selection: hex(0x2E436E),
            list_selection_inactive: hex(0x43454A),
            hover: hex(0x393B40),
            selection: hex(0x214283),
            caret: hex(0xCED0D6),
            caret_row: hex(0x26282E),
            line_number: hex(0x4B5059),
            line_number_active: hex(0xA1A3AB),
            indent_guide: hex(0x313438),
            whitespace: hex(0x4A4D52),
            right_margin: hex(0x2B2D30),
            find_match: hex(0x32593D),
            find_match_current: hex(0x4C7A55),
            occurrence: hex(0x373B4A),
            bracket_match: hex(0x43454A),
            error: hex(0xF75464),
            warning: hex(0xE0A23C),
            weak: hex(0x6F737A),
            success: hex(0x5FB865),
            link: hex(0x548AF7),
            console_stderr: hex(0xF77B7B),
            console_system: hex(0x868A91),
            terminal_ansi: [
                hex(0x45484D),
                hex(0xF0524F),
                hex(0x5C962C),
                hex(0xC9A227),
                hex(0x3993D4),
                hex(0xA771BF),
                hex(0x00A3A3),
                hex(0xBCBEC4),
                hex(0x6F737A),
                hex(0xFF6B68),
                hex(0x6AAB73),
                hex(0xE5BF00),
                hex(0x56A8F5),
                hex(0xC77DBB),
                hex(0x2AACB8),
                hex(0xDFE1E5),
            ],
            breakpoint: hex(0xDB5C5C),
            breakpoint_disabled: hex(0x6F737A),
            execution_line: hex(0x2D3A5C),
            inline_value: hex(0x7EA1D8),
            inlay_text: hex(0x868A91),
            inlay_bg: hex(0x2B2D30),
            diff_added: hex(0x5FB865),
            diff_removed: hex(0xF75464),
            diff_added_bg: hex(0x294436),
            diff_removed_bg: hex(0x4C2B2E),
            diff_modified_bg: hex(0x2B3A55),
            annotation: hex(0xEF4444),
            vcs_modified: hex(0x6CA0F5),
            vcs_added: hex(0x73BD79),
            vcs_deleted: hex(0x7B7E85),
            vcs_untracked: hex(0xD1675A),
            vcs_ignored: hex(0x6F737A),
            vcs_conflict: hex(0xF0616D),
            vcs_gutter_added: hex(0x549159),
            vcs_gutter_modified: hex(0x4A73C2),
            vcs_gutter_deleted: hex(0xC9525C),
            vcs_annotation_bg: hex(0x25272B),
            keyword: hex(0xCF8E6D),
            string: hex(0x6AAB73),
            number: hex(0x2AACB8),
            comment: hex(0x7A7E85),
            doc_comment: hex(0x5F826B),
            function: hex(0x56A8F5),
            macro_: hex(0x4EB7C9),
            type_: hex(0xB5B6E3),
            lifetime: hex(0x20999D),
            attribute: hex(0xB3AE60),
            constant: hex(0xC77DBB),
            field: hex(0xC77DBB),
            escape: hex(0xCF8E6D),
        }
    }

    pub fn light() -> Self {
        Self {
            dark: false,
            editor_bg: hex(0xFFFFFF),
            panel_bg: hex(0xF7F8FA),
            header_bg: hex(0xF7F8FA),
            popup_bg: hex(0xFFFFFF),
            border: hex(0xEBECF0),
            separator: hex(0xDFE1E5),
            text: hex(0x1E1F22),
            text_editor: hex(0x080808),
            text_muted: hex(0x6C707E),
            text_disabled: hex(0xA8ADBD),
            accent: hex(0x3574F0),
            list_selection: hex(0xD4E2FF),
            list_selection_inactive: hex(0xDFE1E5),
            hover: hex(0xE9EAEE),
            selection: hex(0xA6D2FF),
            caret: hex(0x000000),
            caret_row: hex(0xF5F8FE),
            line_number: hex(0xAEB3C2),
            line_number_active: hex(0x767A8A),
            indent_guide: hex(0xEBECF0),
            whitespace: hex(0xC9CCD6),
            right_margin: hex(0xEBECF0),
            find_match: hex(0xC5E8CE),
            find_match_current: hex(0x8FD3A6),
            occurrence: hex(0xEDEBFC),
            bracket_match: hex(0x99CCFF),
            error: hex(0xE55765),
            warning: hex(0xB58A00),
            weak: hex(0x8C8C8C),
            success: hex(0x208A3C),
            link: hex(0x2A7DE1),
            console_stderr: hex(0xDB3B4B),
            console_system: hex(0x6C707E),
            terminal_ansi: [
                hex(0x000000),
                hex(0xC91B00),
                hex(0x00A600),
                hex(0x8F7300),
                hex(0x0225C7),
                hex(0xB12FB1),
                hex(0x00A2B8),
                hex(0x8C8C8C),
                hex(0x686868),
                hex(0xE55765),
                hex(0x208A3C),
                hex(0xB58A00),
                hex(0x3574F0),
                hex(0xC23BC2),
                hex(0x00A2B8),
                hex(0x1E1F22),
            ],
            breakpoint: hex(0xE55765),
            breakpoint_disabled: hex(0xA8ADBD),
            execution_line: hex(0xD6E4FF),
            inline_value: hex(0x4A6FBF),
            inlay_text: hex(0x8C8C8C),
            inlay_bg: hex(0xEDEEF0),
            diff_added: hex(0x369650),
            diff_removed: hex(0xE55765),
            diff_added_bg: hex(0xE6F4E9),
            diff_removed_bg: hex(0xFCE8E9),
            diff_modified_bg: hex(0xE6EEFC),
            annotation: hex(0xEF4444),
            vcs_modified: hex(0x3574F0),
            vcs_added: hex(0x208A3C),
            vcs_deleted: hex(0x6C707E),
            vcs_untracked: hex(0xC94F4F),
            vcs_ignored: hex(0x8C8C8C),
            vcs_conflict: hex(0xD02F3A),
            vcs_gutter_added: hex(0xA6D7AE),
            vcs_gutter_modified: hex(0xA7C4F4),
            vcs_gutter_deleted: hex(0xE8A2A6),
            vcs_annotation_bg: hex(0xF2F3F5),
            keyword: hex(0x0033B3),
            string: hex(0x067D17),
            number: hex(0x1750EB),
            comment: hex(0x8C8C8C),
            doc_comment: hex(0x6A8759),
            function: hex(0x00627A),
            macro_: hex(0x1F542E),
            type_: hex(0x000080),
            lifetime: hex(0x20999D),
            attribute: hex(0x9E880D),
            constant: hex(0x871094),
            field: hex(0x871094),
            escape: hex(0x0037A6),
        }
    }

    /// Black background, white text, saturated accents.
    pub fn high_contrast() -> Self {
        Self {
            dark: true,
            editor_bg: hex(0x000000),
            panel_bg: hex(0x000000),
            header_bg: hex(0x000000),
            popup_bg: hex(0x0F0F0F),
            border: hex(0x6B6B6B),
            separator: hex(0x6B6B6B),
            text: hex(0xFFFFFF),
            text_editor: hex(0xFFFFFF),
            text_muted: hex(0xC0C0C0),
            text_disabled: hex(0x808080),
            accent: hex(0x1AEBFF),
            list_selection: hex(0x0050C8),
            list_selection_inactive: hex(0x3A3A3A),
            hover: hex(0x2E2E2E),
            selection: hex(0x0047B3),
            caret: hex(0xFFFFFF),
            caret_row: hex(0x1A1A1A),
            line_number: hex(0xA0A0A0),
            line_number_active: hex(0xFFFFFF),
            indent_guide: hex(0x4D4D4D),
            whitespace: hex(0x6E6E6E),
            right_margin: hex(0x4D4D4D),
            find_match: hex(0x005F00),
            find_match_current: hex(0x008F00),
            occurrence: hex(0x3D3D7A),
            bracket_match: hex(0x5C5C00),
            error: hex(0xFF6B6B),
            warning: hex(0xFFD700),
            weak: hex(0xA0A0A0),
            success: hex(0x4CFF4C),
            link: hex(0x1AEBFF),
            console_stderr: hex(0xFF6B6B),
            console_system: hex(0xC0C0C0),
            terminal_ansi: [
                hex(0x000000),
                hex(0xFF5555),
                hex(0x55FF55),
                hex(0xFFFF55),
                hex(0x5C9DFF),
                hex(0xFF55FF),
                hex(0x55FFFF),
                hex(0xE0E0E0),
                hex(0x808080),
                hex(0xFF8080),
                hex(0x80FF80),
                hex(0xFFFF80),
                hex(0x8AB4FF),
                hex(0xFF80FF),
                hex(0x80FFFF),
                hex(0xFFFFFF),
            ],
            breakpoint: hex(0xFF4040),
            breakpoint_disabled: hex(0x808080),
            execution_line: hex(0x00337A),
            inline_value: hex(0x8AB4FF),
            inlay_text: hex(0xC0C0C0),
            inlay_bg: hex(0x1A1A1A),
            diff_added: hex(0x4CFF4C),
            diff_removed: hex(0xFF6B6B),
            diff_added_bg: hex(0x003300),
            diff_removed_bg: hex(0x4D0000),
            diff_modified_bg: hex(0x00264D),
            annotation: hex(0xFF4040),
            vcs_modified: hex(0x6FB7FF),
            vcs_added: hex(0x4CFF4C),
            vcs_deleted: hex(0xA0A0A0),
            vcs_untracked: hex(0xFF8A80),
            vcs_ignored: hex(0xA0A0A0),
            vcs_conflict: hex(0xFF4040),
            vcs_gutter_added: hex(0x2EBF2E),
            vcs_gutter_modified: hex(0x3D8BFF),
            vcs_gutter_deleted: hex(0xFF4040),
            vcs_annotation_bg: hex(0x121212),
            keyword: hex(0xFF9E3D),
            string: hex(0x9FEF00),
            number: hex(0x00E5FF),
            comment: hex(0xA0A0A0),
            doc_comment: hex(0x66CC99),
            function: hex(0xFFEE58),
            macro_: hex(0x00FFD0),
            type_: hex(0xC8A2FF),
            lifetime: hex(0x00E5C0),
            attribute: hex(0xE6E600),
            constant: hex(0xFF80FF),
            field: hex(0xFF80FF),
            escape: hex(0xFF9E3D),
        }
    }

    /// The built-in theme/scheme called `name` (Dark for unknown names).
    pub fn by_name(name: &str) -> Self {
        match settings::canonical_theme(name) {
            Some("Light") => Self::light(),
            Some("High Contrast") => Self::high_contrast(),
            _ => Self::dark(),
        }
    }

    /// The UI theme of `st.theme` with the editor colours of the effective colour scheme (its
    /// base scheme plus the user's token overrides).
    pub fn from_settings(st: &Settings) -> Self {
        let ui = settings::canonical_theme(&st.theme).unwrap_or("Dark");
        let mut t = Self::by_name(ui);
        let scheme = settings::effective_scheme(st);
        let user = st.user_schemes.iter().find(|u| u.name == scheme);
        let base = user.map(|u| u.base.as_str()).unwrap_or(scheme.as_str());
        if settings::canonical_theme(base) != Some(ui) {
            let b = Self::by_name(base);
            for token in EDITOR_TOKENS {
                if let (Some(c), Some(slot)) = (b.token(token), t.token_mut(token)) {
                    *slot = c;
                }
            }
            t.terminal_ansi = b.terminal_ansi;
        }
        for (token, color) in user.map(|u| &u.colors).into_iter().flatten() {
            if let (Some(c), Some(slot)) = (parse_hex(color), t.token_mut(token)) {
                *slot = c;
            }
        }
        t
    }

    pub fn syntax(&self, kind: HighlightKind) -> Color32 {
        match kind {
            HighlightKind::Keyword | HighlightKind::SelfKeyword | HighlightKind::Boolean => self.keyword,
            HighlightKind::String | HighlightKind::Char | HighlightKind::Code => self.string,
            HighlightKind::Number => self.number,
            HighlightKind::Comment => self.comment,
            HighlightKind::DocComment => self.doc_comment,
            HighlightKind::Function => self.function,
            HighlightKind::Macro => self.macro_,
            HighlightKind::Type => self.type_,
            HighlightKind::Lifetime => self.lifetime,
            HighlightKind::Attribute => self.attribute,
            HighlightKind::Constant => self.constant,
            HighlightKind::Field | HighlightKind::Key => self.field,
            HighlightKind::Heading => self.keyword,
            HighlightKind::Link => self.link,
            HighlightKind::Invalid => self.error,
            HighlightKind::Emphasis | HighlightKind::Plain | HighlightKind::Ident => self.text_editor,
            HighlightKind::Operator | HighlightKind::Punctuation | HighlightKind::Bracket => self.text_editor,
        }
    }

    /// Apply to egui's widget visuals.
    pub fn apply(&self, ctx: &egui::Context) {
        let mut style = (*ctx.global_style()).clone();
        let v = &mut style.visuals;
        *v = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        v.panel_fill = self.panel_bg;
        v.window_fill = self.popup_bg;
        v.extreme_bg_color = self.editor_bg;
        v.faint_bg_color = self.caret_row;
        v.override_text_color = Some(self.text);
        v.hyperlink_color = self.link;
        v.selection.bg_fill = self.list_selection;
        v.selection.stroke = egui::Stroke::new(1.0, self.text);
        v.window_stroke = egui::Stroke::new(1.0, self.separator);
        v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, self.separator);
        v.widgets.noninteractive.bg_fill = self.panel_bg;
        v.widgets.inactive.bg_fill = self.hover;
        v.widgets.inactive.weak_bg_fill = self.panel_bg;
        v.widgets.hovered.weak_bg_fill = self.hover;
        v.widgets.hovered.bg_fill = self.hover;
        v.widgets.active.weak_bg_fill = self.list_selection;
        v.text_cursor.stroke = egui::Stroke::new(2.0, self.caret);
        if self.panel_bg == self.editor_bg {
            // High contrast: panels and the editor share one background, so widgets get outlines.
            v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, self.separator);
        }
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        style.spacing.button_padding = egui::vec2(6.0, 2.0);
        style.spacing.menu_margin = egui::Margin::same(4);
        style.interaction.tooltip_delay = 0.4;
        // Panel and window edges are easy to grab.
        style.interaction.resize_grab_radius_side = 8.0;
        style.interaction.resize_grab_radius_corner = 12.0;
        ctx.set_global_style(style);
    }
}

/// `#RRGGBB` → colour.
pub fn parse_hex(s: &str) -> Option<Color32> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    u32::from_str_radix(h, 16).ok().map(hex)
}

/// `#RRGGBB` of a colour.
pub fn to_hex(c: Color32) -> String {
    format!("#{:02X}{:02X}{:02X}", c.r(), c.g(), c.b())
}

/// Tokens that belong to the editor colour scheme (the rest follow the UI theme).
pub const EDITOR_TOKENS: &[&str] = &[
    "editor_bg",
    "text_editor",
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

macro_rules! tokens {
    ($($name:literal => $field:ident),* $(,)?) => {
        impl Theme {
            /// Every colour token by name (as user schemes and `colorScheme.setColor` name them).
            pub const TOKENS: &'static [&'static str] = &[$($name),*];

            pub fn token(&self, name: &str) -> Option<Color32> {
                match name {
                    $($name => Some(self.$field),)*
                    _ => None,
                }
            }

            pub fn token_mut(&mut self, name: &str) -> Option<&mut Color32> {
                match name {
                    $($name => Some(&mut self.$field),)*
                    _ => None,
                }
            }
        }
    };
}

tokens! {
    "editor_bg" => editor_bg,
    "panel_bg" => panel_bg,
    "header_bg" => header_bg,
    "popup_bg" => popup_bg,
    "border" => border,
    "separator" => separator,
    "text" => text,
    "text_editor" => text_editor,
    "text_muted" => text_muted,
    "text_disabled" => text_disabled,
    "accent" => accent,
    "list_selection" => list_selection,
    "list_selection_inactive" => list_selection_inactive,
    "hover" => hover,
    "selection" => selection,
    "caret" => caret,
    "caret_row" => caret_row,
    "line_number" => line_number,
    "line_number_active" => line_number_active,
    "indent_guide" => indent_guide,
    "whitespace" => whitespace,
    "right_margin" => right_margin,
    "find_match" => find_match,
    "find_match_current" => find_match_current,
    "occurrence" => occurrence,
    "bracket_match" => bracket_match,
    "error" => error,
    "warning" => warning,
    "weak" => weak,
    "success" => success,
    "link" => link,
    "console_stderr" => console_stderr,
    "console_system" => console_system,
    "breakpoint" => breakpoint,
    "breakpoint_disabled" => breakpoint_disabled,
    "execution_line" => execution_line,
    "inline_value" => inline_value,
    "inlay_text" => inlay_text,
    "inlay_bg" => inlay_bg,
    "diff_added" => diff_added,
    "diff_removed" => diff_removed,
    "diff_added_bg" => diff_added_bg,
    "diff_removed_bg" => diff_removed_bg,
    "diff_modified_bg" => diff_modified_bg,
    "annotation" => annotation,
    "vcs_modified" => vcs_modified,
    "vcs_added" => vcs_added,
    "vcs_deleted" => vcs_deleted,
    "vcs_untracked" => vcs_untracked,
    "vcs_ignored" => vcs_ignored,
    "vcs_conflict" => vcs_conflict,
    "vcs_gutter_added" => vcs_gutter_added,
    "vcs_gutter_modified" => vcs_gutter_modified,
    "vcs_gutter_deleted" => vcs_gutter_deleted,
    "vcs_annotation_bg" => vcs_annotation_bg,
    "keyword" => keyword,
    "string" => string,
    "number" => number,
    "comment" => comment,
    "doc_comment" => doc_comment,
    "function" => function,
    "macro" => macro_,
    "type" => type_,
    "lifetime" => lifetime,
    "attribute" => attribute,
    "constant" => constant,
    "field" => field,
    "escape" => escape,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_tokens_match_the_theme() {
        let mut engine: Vec<&str> = settings::COLOR_TOKENS.to_vec();
        let mut ui: Vec<&str> = Theme::TOKENS.to_vec();
        engine.sort_unstable();
        ui.sort_unstable();
        assert_eq!(engine, ui);
        for t in EDITOR_TOKENS {
            assert!(Theme::TOKENS.contains(t), "{t}");
        }
    }

    #[test]
    fn builtins_differ_and_light_is_light() {
        assert!(Theme::dark().dark && !Theme::light().dark && Theme::high_contrast().dark);
        assert_ne!(Theme::dark().editor_bg, Theme::light().editor_bg);
        assert_ne!(Theme::dark().keyword, Theme::high_contrast().keyword);
        assert_eq!(Theme::by_name("light").editor_bg, Theme::light().editor_bg);
        assert_eq!(Theme::by_name("nonsense").editor_bg, Theme::dark().editor_bg);
    }

    #[test]
    fn scheme_and_user_overrides_reach_the_editor_colours() {
        // Dark UI with the Light editor scheme.
        let mut st = Settings { theme: "Dark".into(), color_scheme: "Light".into(), ..Default::default() };
        let t = Theme::from_settings(&st);
        assert_eq!(t.panel_bg, Theme::dark().panel_bg);
        assert_eq!(t.editor_bg, Theme::light().editor_bg);
        assert_eq!(t.keyword, Theme::light().keyword);
        // A user scheme on top of High Contrast.
        settings::set_value(
            &mut st,
            "user_schemes",
            &serde_json::json!([{ "name": "Mine", "base": "High Contrast", "colors": { "keyword": "#123456" } }]),
        )
        .unwrap();
        settings::set_value(&mut st, "color_scheme", &serde_json::json!("Mine")).unwrap();
        let t = Theme::from_settings(&st);
        assert_eq!(t.keyword, hex(0x123456));
        assert_eq!(t.syntax(HighlightKind::Keyword), hex(0x123456));
        assert_eq!(t.string, Theme::high_contrast().string);
        assert_eq!(to_hex(t.keyword), "#123456");
        assert_eq!(parse_hex("#zzzzzz"), None);
    }
}
