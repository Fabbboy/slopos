use slopos_abi::draw::Color32;

use slopos_abi::DamageRect;
use std::string::String;

use crate::gfx::draw_str_clipped;
use crate::gfx::font::string_width;
use crate::gfx::{DrawBuffer, draw_rect, fill_rect};

use super::{
    COLOR_BAR_BG, COLOR_BAR_HIGH, COLOR_BAR_LOW, COLOR_BAR_MED, COLOR_BRIGHT, COLOR_DIM,
    COLOR_HEADER, COLOR_SECTION, COLOR_TAB_ACTIVE, COLOR_TAB_BAR, COLOR_TAB_INACTIVE, SortColumn,
    SysmonApp, TAB_HEIGHT, TAB_WIDTH, Tab,
};

impl SysmonApp {
    pub(crate) fn text(fb: &mut DrawBuffer<'_>, x: i32, y: i32, s: &str, fg: Color32, bg: Color32) {
        let clip = DamageRect {
            x0: 0,
            y0: 0,
            x1: fb.width() as i32 - 1,
            y1: fb.height() as i32 - 1,
        };
        draw_str_clipped(fb, x, y, s, fg, bg, &clip);
    }

    pub(crate) fn draw_tab_bar(&self, fb: &mut DrawBuffer<'_>, width: i32) {
        fill_rect(fb, 0, 0, width, TAB_HEIGHT, COLOR_TAB_BAR);

        self.draw_tab(fb, 4, "Overview", self.active_tab == Tab::Overview);
        self.draw_tab(
            fb,
            4 + TAB_WIDTH + 2,
            "Processes",
            self.active_tab == Tab::Processes,
        );
        self.draw_tab(
            fb,
            4 + (TAB_WIDTH + 2) * 2,
            "Hardware",
            self.active_tab == Tab::Hardware,
        );

        draw_rect(fb, 0, TAB_HEIGHT - 1, width, 1, COLOR_DIM);
    }

    pub(crate) fn draw_tab(&self, fb: &mut DrawBuffer<'_>, x: i32, label: &str, active: bool) {
        let bg = if active {
            COLOR_TAB_ACTIVE
        } else {
            COLOR_TAB_INACTIVE
        };
        let fg = if active { COLOR_BRIGHT } else { COLOR_DIM };
        fill_rect(fb, x, 2, TAB_WIDTH, TAB_HEIGHT - 3, bg);
        draw_rect(fb, x, 2, TAB_WIDTH, TAB_HEIGHT - 3, COLOR_DIM);
        Self::text(
            fb,
            x + ((TAB_WIDTH - string_width(label)) / 2).max(4),
            6,
            label,
            fg,
            bg,
        );
    }

    pub(crate) fn draw_section_title(&self, fb: &mut DrawBuffer<'_>, x: i32, y: i32, title: &str) {
        Self::text(fb, x, y, title, COLOR_SECTION, super::COLOR_BG);
    }

    pub(crate) fn draw_usage_bar(
        &self,
        fb: &mut DrawBuffer<'_>,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        pct: u32,
        label: &str,
    ) {
        fill_rect(fb, x, y, w, h, COLOR_BAR_BG);
        let fill_w = ((w as u32).saturating_mul(pct.min(100)) / 100) as i32;
        let color = if pct < 50 {
            COLOR_BAR_LOW
        } else if pct < 80 {
            COLOR_BAR_MED
        } else {
            COLOR_BAR_HIGH
        };
        fill_rect(fb, x, y, fill_w, h, color);
        draw_rect(fb, x, y, w, h, COLOR_DIM);
        Self::text(fb, x + 4, y + 1, label, COLOR_BRIGHT, COLOR_BAR_BG);
    }

    pub(crate) fn draw_header_cell(
        &self,
        fb: &mut DrawBuffer<'_>,
        x: i32,
        y: i32,
        label: &str,
        col: SortColumn,
    ) {
        if self.sort_column == col {
            let indicator = if self.sort_ascending { "^" } else { "v" };
            let mut header = String::from(label);
            header.push(' ');
            header.push_str(indicator);
            Self::text(fb, x, y, &header, COLOR_BRIGHT, COLOR_HEADER);
        } else {
            Self::text(fb, x, y, label, COLOR_BRIGHT, COLOR_HEADER);
        }
    }
}
