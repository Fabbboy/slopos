use slopos_abi::draw::Color32;

/// Central style sheet; widgets reference this instead of hardcoding colors and sizes.
pub struct StyleSheet {
    pub bg_primary: Color32,
    pub bg_secondary: Color32,
    pub bg_tertiary: Color32,
    pub bg_accent: Color32,
    pub bg_destructive: Color32,

    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_on_accent: Color32,
    pub text_disabled: Color32,

    pub border_default: Color32,
    pub border_focused: Color32,
    pub border_divider: Color32,

    pub shadow_color: Color32,
    pub focus_ring_color: Color32,

    pub font_size: i32,
    pub font_size_small: i32,
    pub font_size_heading: i32,
    pub line_height: i32,

    pub spacing_xs: i32,
    pub spacing_sm: i32,
    pub spacing_md: i32,
    pub spacing_lg: i32,
    pub spacing_xl: i32,

    pub corner_radius: i32,
    pub border_width: i32,
    pub focus_ring_width: i32,
    pub focus_ring_offset: i32,

    pub button_padding_h: i32,
    pub button_padding_v: i32,
    pub button_min_width: i32,

    pub field_padding_h: i32,
    pub field_padding_v: i32,
    pub field_min_width: i32,

    pub scrollbar_width: i32,
    pub scrollbar_thumb_min: i32,

    pub tab_height: i32,
    pub menu_item_height: i32,
    pub menu_min_width: i32,

    pub checkbox_size: i32,
    pub checkbox_gap: i32,
}

impl StyleSheet {
    pub fn dark() -> Self {
        Self {
            bg_primary: Color32::rgb(30, 30, 30),
            bg_secondary: Color32::rgb(45, 45, 45),
            bg_tertiary: Color32::rgb(60, 60, 60),
            bg_accent: Color32::rgb(0, 122, 255),
            bg_destructive: Color32::rgb(255, 59, 48),

            text_primary: Color32::rgb(0xE0, 0xE0, 0xE0),
            text_secondary: Color32::rgb(0x90, 0x90, 0x90),
            text_on_accent: Color32::WHITE,
            text_disabled: Color32::rgb(100, 100, 100),

            border_default: Color32::rgb(70, 70, 70),
            border_focused: Color32::rgb(0, 122, 255),
            border_divider: Color32::rgb(55, 55, 55),

            shadow_color: Color32::new(0, 0, 0, 80),
            focus_ring_color: Color32::new(0, 122, 255, 180),

            font_size: 14,
            font_size_small: 12,
            font_size_heading: 18,
            line_height: 20,

            spacing_xs: 4,
            spacing_sm: 8,
            spacing_md: 12,
            spacing_lg: 16,
            spacing_xl: 24,

            corner_radius: 6,
            border_width: 1,
            focus_ring_width: 2,
            focus_ring_offset: 1,

            button_padding_h: 12,
            button_padding_v: 6,
            button_min_width: 64,

            field_padding_h: 8,
            field_padding_v: 6,
            field_min_width: 80,

            scrollbar_width: 8,
            scrollbar_thumb_min: 20,

            tab_height: 36,
            menu_item_height: 28,
            menu_min_width: 120,

            checkbox_size: 16,
            checkbox_gap: 8,
        }
    }
}

impl Default for StyleSheet {
    fn default() -> Self {
        Self::dark()
    }
}
