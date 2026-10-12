//! Design tokens. Colours approximate RustRover's dark "New UI" look by eye (clean-room: no
//! scheme files are copied). Every colour the UI paints comes from here.

use egui::Color32;

use codecraft_syntax::HighlightKind;

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb(((rgb >> 16) & 0xff) as u8, ((rgb >> 8) & 0xff) as u8, (rgb & 0xff) as u8)
}

#[derive(Clone, Debug)]
pub struct Theme {
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
        *v = egui::Visuals::dark();
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
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        style.spacing.button_padding = egui::vec2(6.0, 2.0);
        style.spacing.menu_margin = egui::Margin::same(4);
        style.interaction.tooltip_delay = 0.4;
        ctx.set_global_style(style);
    }
}
