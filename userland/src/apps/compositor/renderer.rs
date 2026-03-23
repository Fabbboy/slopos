use slopos_abi::draw::Color32;
use slopos_font::FontRenderer;

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;
use crate::theme::*;

use super::hover::{
    HOVER_APP_BTN_BASE, HOVER_CLOSE_BASE, HOVER_MENU_ITEM_BASE, HOVER_MINIMIZE_BASE,
    HOVER_START_BTN, HoverRegistry,
};
use super::output::{RenderMode, WINDOW_STATE_MINIMIZED};
use super::surface_cache::ClientSurfaceCache;
use super::taskbar::{self, START_MENU_ITEMS};

const COLOR_WINDOW_PLACEHOLDER: Color32 = Color32::rgb(0x20, 0x20, 0x30);

/// Default TTF font size for title bar text (in pixels).
const TITLE_FONT_SIZE: u16 = 14;

pub struct Renderer {
    pub output_width: u32,
    pub output_height: u32,
    pub output_bytes_pp: u8,
    pub output_pitch: usize,
    ttf_font: Option<FontRenderer<'static>>,
    ttf_init_attempted: bool,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            output_width: 0,
            output_height: 0,
            output_bytes_pp: 4,
            output_pitch: 0,
            ttf_font: None,
            ttf_init_attempted: false,
        }
    }

    /// Try to load the TTF font from the filesystem (once).
    fn ensure_font(&mut self) {
        if self.ttf_init_attempted {
            return;
        }
        self.ttf_init_attempted = true;

        if let Ok(data) = std::fs::read("/usr/share/fonts/Inter-Regular.ttf") {
            let leaked: &'static [u8] = Box::leak(data.into_boxed_slice());
            self.ttf_font = FontRenderer::new(leaked);
        }
    }

    pub fn set_output_info(&mut self, width: u32, height: u32, bytes_pp: u8, pitch: usize) {
        self.output_width = width;
        self.output_height = height;
        self.output_bytes_pp = bytes_pp;
        self.output_pitch = pitch;
    }

    pub fn render(
        &mut self,
        buf: &mut DrawBuffer,
        windows: &[UserWindowInfo],
        window_count: usize,
        focused_task: u32,
        start_menu_open: bool,
        mouse_x: i32,
        mouse_y: i32,
        cursor_shape: u8,
        hover: &HoverRegistry,
        surface_cache: &mut ClientSurfaceCache,
        force_full: bool,
        damage_regions: &[DamageRect],
    ) -> RenderMode {
        if force_full {
            let full_clip = full_screen_clip(buf);
            gfx::fill_rect(
                buf,
                0,
                0,
                buf.width() as i32,
                buf.height() as i32,
                COLOR_BACKGROUND,
            );

            for i in 0..window_count {
                let window = windows[i];
                if window.state == WINDOW_STATE_MINIMIZED {
                    continue;
                }
                self.draw_window_content(buf, &window, &full_clip, surface_cache);
                self.draw_title_bar(buf, &window, focused_task, hover, &full_clip);
            }

            self.draw_taskbar(
                buf,
                windows,
                window_count,
                focused_task,
                start_menu_open,
                hover,
                &full_clip,
            );
            self.draw_start_menu(buf, start_menu_open, hover, &full_clip);
            self.draw_cursor(buf, mouse_x, mouse_y, cursor_shape, &full_clip);
            RenderMode::Full
        } else if damage_regions.is_empty() {
            RenderMode::Partial
        } else {
            for rect in damage_regions {
                self.draw_partial_region(
                    buf,
                    rect,
                    windows,
                    window_count,
                    focused_task,
                    start_menu_open,
                    mouse_x,
                    mouse_y,
                    cursor_shape,
                    hover,
                    surface_cache,
                );
            }
            RenderMode::Partial
        }
    }

    fn draw_partial_region(
        &mut self,
        buf: &mut DrawBuffer,
        damage: &DamageRect,
        windows: &[UserWindowInfo],
        window_count: usize,
        focused_task: u32,
        start_menu_open: bool,
        mouse_x: i32,
        mouse_y: i32,
        cursor_shape: u8,
        hover: &HoverRegistry,
        surface_cache: &mut ClientSurfaceCache,
    ) {
        if !damage.is_valid() {
            return;
        }

        gfx::fill_rect(
            buf,
            damage.x0,
            damage.y0,
            damage.x1 - damage.x0 + 1,
            damage.y1 - damage.y0 + 1,
            COLOR_BACKGROUND,
        );

        for i in 0..window_count {
            let window = windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            let content_rect = DamageRect {
                x0: window.x,
                y0: window.y,
                x1: window.x + window.width as i32 - 1,
                y1: window.y + window.height as i32 - 1,
            };
            if intersect_rect(damage, &content_rect).is_some() {
                self.draw_window_content(buf, &window, damage, surface_cache);
            }

            let title_rect = DamageRect {
                x0: window.x,
                y0: window.y - TITLE_BAR_HEIGHT,
                x1: window.x + window.width as i32 - 1,
                y1: window.y - 1,
            };
            if intersect_rect(damage, &title_rect).is_some() {
                self.draw_title_bar(buf, &window, focused_task, hover, damage);
            }
        }

        let taskbar_y = buf.height() as i32 - TASKBAR_HEIGHT;
        let taskbar_rect = DamageRect {
            x0: 0,
            y0: taskbar_y,
            x1: buf.width() as i32 - 1,
            y1: buf.height() as i32 - 1,
        };
        if intersect_rect(damage, &taskbar_rect).is_some() {
            self.draw_taskbar(
                buf,
                windows,
                window_count,
                focused_task,
                start_menu_open,
                hover,
                damage,
            );
        }

        if start_menu_open {
            let menu_h = taskbar::start_menu_height();
            let fb_h = buf.height() as i32;
            let menu_rect = DamageRect {
                x0: taskbar::start_menu_x(),
                y0: taskbar::start_menu_y(fb_h),
                x1: taskbar::start_menu_x() + START_MENU_WIDTH - 1,
                y1: taskbar::start_menu_y(fb_h) + menu_h - 1,
            };
            if intersect_rect(damage, &menu_rect).is_some() {
                self.draw_start_menu(buf, start_menu_open, hover, damage);
            }
        }

        let cursor_rect = cursor_bounds(mouse_x, mouse_y, cursor_shape);
        if intersect_rect(damage, &cursor_rect).is_some() {
            self.draw_cursor(buf, mouse_x, mouse_y, cursor_shape, damage);
        }
    }

    fn draw_title_bar(
        &mut self,
        buf: &mut DrawBuffer,
        window: &UserWindowInfo,
        focused_task: u32,
        hover: &HoverRegistry,
        clip: &DamageRect,
    ) {
        let focused = window.task_id == focused_task;
        let color = if focused {
            COLOR_TITLE_BAR_FOCUSED
        } else {
            COLOR_TITLE_BAR
        };
        let title_y = window.y - TITLE_BAR_HEIGHT;

        gfx::fill_rect_clipped(
            buf,
            window.x,
            title_y,
            window.width as i32,
            TITLE_BAR_HEIGHT,
            color,
            clip,
        );

        let title = title_to_str(&window.title);

        // Try TTF font first, fall back to bitmap
        self.ensure_font();
        let used_ttf = if let Some(ref mut font) = self.ttf_font {
            font.draw_text(
                buf,
                window.x + 8,
                title_y + 3,
                title,
                TITLE_FONT_SIZE,
                COLOR_TEXT,
            );
            true
        } else {
            false
        };

        if !used_ttf {
            gfx::draw_str_clipped(
                buf,
                window.x + 8,
                title_y + 4,
                title,
                COLOR_TEXT,
                color,
                clip,
            );
        }

        draw_button_clipped(
            buf,
            window.x + window.width as i32 - BUTTON_SIZE - BUTTON_PADDING,
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "X",
            hover.is_hovered(HOVER_CLOSE_BASE | window.task_id),
            true,
            clip,
        );

        draw_button_clipped(
            buf,
            window.x + window.width as i32 - (BUTTON_SIZE * 2) - (BUTTON_PADDING * 2),
            title_y + BUTTON_PADDING,
            BUTTON_SIZE,
            "_",
            hover.is_hovered(HOVER_MINIMIZE_BASE | window.task_id),
            false,
            clip,
        );
    }

    fn draw_taskbar(
        &self,
        buf: &mut DrawBuffer,
        windows: &[UserWindowInfo],
        window_count: usize,
        focused_task: u32,
        start_menu_open: bool,
        hover: &HoverRegistry,
        clip: &DamageRect,
    ) {
        let taskbar_y = buf.height() as i32 - TASKBAR_HEIGHT;

        gfx::fill_rect_clipped(
            buf,
            0,
            taskbar_y,
            buf.width() as i32,
            TASKBAR_HEIGHT,
            COLOR_TASKBAR,
            clip,
        );

        let start_btn_x = taskbar::start_button_x();
        let btn_y = taskbar_y + TASKBAR_BUTTON_PADDING;
        let btn_height = TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2);

        let start_hover = hover.is_hovered(HOVER_START_BTN);
        let start_color = if start_menu_open || start_hover {
            COLOR_BUTTON_HOVER
        } else {
            COLOR_BUTTON
        };

        gfx::fill_rect_clipped(
            buf,
            start_btn_x,
            btn_y,
            START_BUTTON_WIDTH,
            btn_height,
            start_color,
            clip,
        );
        gfx::draw_str_clipped(
            buf,
            start_btn_x + 4,
            btn_y + 4,
            "Start",
            COLOR_TEXT,
            start_color,
            clip,
        );

        let separator_x = taskbar::app_buttons_start_x() - (START_APPS_GAP / 2);
        gfx::fill_rect_clipped(
            buf,
            separator_x,
            taskbar_y + 2,
            1,
            TASKBAR_HEIGHT - 4,
            COLOR_BUTTON_HOVER,
            clip,
        );

        let mut x = taskbar::app_buttons_start_x();
        for i in 0..window_count {
            let window = &windows[i];
            let focused = window.task_id == focused_task;
            let hovered = hover.is_hovered(HOVER_APP_BTN_BASE | window.task_id);
            let btn_color = if focused || hovered {
                COLOR_BUTTON_HOVER
            } else {
                COLOR_BUTTON
            };

            let btn_y = taskbar_y + TASKBAR_BUTTON_PADDING;
            let btn_height = TASKBAR_HEIGHT - (TASKBAR_BUTTON_PADDING * 2);

            gfx::fill_rect_clipped(
                buf,
                x,
                btn_y,
                TASKBAR_BUTTON_WIDTH,
                btn_height,
                btn_color,
                clip,
            );

            let title = title_to_str(&window.title);
            let max_chars = (TASKBAR_BUTTON_WIDTH / 8 - 1) as usize;
            let truncated: &str = if title.len() > max_chars {
                &title[..max_chars]
            } else {
                title
            };
            gfx::draw_str_clipped(
                buf,
                x + 4,
                btn_y + 4,
                truncated,
                COLOR_TEXT,
                btn_color,
                clip,
            );

            x += TASKBAR_BUTTON_WIDTH + TASKBAR_BUTTON_PADDING;
        }
    }

    fn draw_start_menu(
        &self,
        buf: &mut DrawBuffer,
        start_menu_open: bool,
        hover: &HoverRegistry,
        clip: &DamageRect,
    ) {
        if !start_menu_open {
            return;
        }

        let fb_height = buf.height() as i32;
        let menu_x = taskbar::start_menu_x();
        let menu_y = taskbar::start_menu_y(fb_height);
        let menu_h = taskbar::start_menu_height();

        gfx::fill_rect_clipped(
            buf,
            menu_x,
            menu_y,
            START_MENU_WIDTH,
            menu_h,
            COLOR_START_MENU_BG,
            clip,
        );

        for (idx, item) in START_MENU_ITEMS.iter().enumerate() {
            let item_y = menu_y + START_MENU_PADDING + (idx as i32 * START_MENU_ITEM_HEIGHT);
            let item_hover = hover.is_hovered(HOVER_MENU_ITEM_BASE | idx as u32);
            let item_color = if item_hover {
                COLOR_BUTTON_HOVER
            } else {
                COLOR_START_MENU_BG
            };

            gfx::fill_rect_clipped(
                buf,
                menu_x + START_MENU_PADDING,
                item_y,
                START_MENU_WIDTH - (START_MENU_PADDING * 2),
                START_MENU_ITEM_HEIGHT,
                item_color,
                clip,
            );
            gfx::draw_str_clipped(
                buf,
                menu_x + START_MENU_PADDING + 4,
                item_y + 6,
                item.label,
                COLOR_TEXT,
                item_color,
                clip,
            );
        }
    }

    fn draw_cursor(
        &self,
        buf: &mut DrawBuffer,
        mx: i32,
        my: i32,
        cursor_shape: u8,
        clip: &DamageRect,
    ) {
        match cursor_shape {
            1 => self.draw_cursor_text(buf, mx, my, clip),
            _ => self.draw_cursor_default(buf, mx, my, clip),
        }
    }

    fn draw_cursor_default(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        // Classic arrow pointer (12×17, hotspot at top-left corner)
        // 0 = transparent, 1 = border (black), 2 = fill (white)
        const W: usize = 12;
        const H: usize = 17;
        #[rustfmt::skip]
        const ARROW: [[u8; W]; H] = [
            [1,0,0,0,0,0,0,0,0,0,0,0],
            [1,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,1,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0],
            [1,2,2,2,2,2,1,0,0,0,0,0],
            [1,2,2,2,2,2,2,1,0,0,0,0],
            [1,2,2,2,2,2,2,2,1,0,0,0],
            [1,2,2,2,2,2,2,2,2,1,0,0],
            [1,2,2,2,2,2,1,1,1,1,1,0],
            [1,2,2,1,2,2,1,0,0,0,0,0],
            [1,2,1,0,1,2,2,1,0,0,0,0],
            [1,1,0,0,1,2,2,1,0,0,0,0],
            [1,0,0,0,0,1,2,2,1,0,0,0],
            [0,0,0,0,0,1,2,2,1,0,0,0],
            [0,0,0,0,0,0,1,1,0,0,0,0],
        ];

        const BORDER: Color32 = Color32::rgb(0x00, 0x00, 0x00);

        for row in 0..H {
            let py = my + row as i32;
            let mut col = 0;
            while col < W {
                let pixel = ARROW[row][col];
                if pixel == 0 {
                    col += 1;
                    continue;
                }
                let color = if pixel == 1 { BORDER } else { COLOR_CURSOR };
                let start = col;
                while col < W && ARROW[row][col] == pixel {
                    col += 1;
                }
                gfx::fill_rect_clipped(
                    buf,
                    mx + start as i32,
                    py,
                    (col - start) as i32,
                    1,
                    color,
                    clip,
                );
            }
        }
    }

    fn draw_cursor_text(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const BEAM_HEIGHT: i32 = 16;
        const SERIF_WIDTH: i32 = 5;
        let top = my - BEAM_HEIGHT / 2;
        gfx::fill_rect_clipped(buf, mx, top, 1, BEAM_HEIGHT, COLOR_CURSOR, clip);
        gfx::fill_rect_clipped(
            buf,
            mx - SERIF_WIDTH / 2,
            top,
            SERIF_WIDTH,
            1,
            COLOR_CURSOR,
            clip,
        );
        gfx::fill_rect_clipped(
            buf,
            mx - SERIF_WIDTH / 2,
            top + BEAM_HEIGHT - 1,
            SERIF_WIDTH,
            1,
            COLOR_CURSOR,
            clip,
        );
    }

    fn draw_window_content(
        &self,
        buf: &mut DrawBuffer,
        window: &UserWindowInfo,
        clip: &DamageRect,
        surface_cache: &mut ClientSurfaceCache,
    ) {
        let bytes_pp = self.output_bytes_pp as usize;
        let src_pitch = (window.width as usize) * bytes_pp;
        let buffer_size = src_pitch * (window.height as usize);

        let cache_index = match surface_cache.get_or_create_index(
            window.task_id,
            window.shm_token,
            buffer_size,
        ) {
            Some(idx) => idx,
            None => {
                self.draw_window_placeholder(buf, window, clip);
                return;
            }
        };

        let src_data = match surface_cache.get_slice(cache_index) {
            Some(slice) => slice,
            None => {
                self.draw_window_placeholder(buf, window, clip);
                return;
            }
        };

        let dst_pitch = self.output_pitch;
        let buf_width = buf.width() as i32;
        let buf_height = buf.height() as i32;

        let window_rect = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + window.width as i32 - 1,
            y1: window.y + window.height as i32 - 1,
        };
        let Some(draw_rect) = intersect_rect(clip, &window_rect) else {
            return;
        };

        let x0 = draw_rect.x0.max(0);
        let y0 = draw_rect.y0.max(0);
        let x1 = (draw_rect.x1 + 1).min(buf_width);
        let y1 = (draw_rect.y1 + 1).min(buf_height);

        if x0 >= x1 || y0 >= y1 {
            return;
        }

        let src_start_x = (x0 - window.x) as usize;
        let src_start_y = (y0 - window.y) as usize;

        let dst_data = buf.data_mut();

        for row in 0..(y1 - y0) as usize {
            let src_row = src_start_y + row;
            let dst_row = (y0 as usize) + row;

            let src_off = src_row * src_pitch + src_start_x * bytes_pp;
            let dst_off = dst_row * dst_pitch + (x0 as usize) * bytes_pp;
            let copy_width = ((x1 - x0) as usize) * bytes_pp;

            let src_end = src_off + copy_width;
            let dst_end = dst_off + copy_width;

            if src_end <= src_data.len() && dst_end <= dst_data.len() {
                dst_data[dst_off..dst_end].copy_from_slice(&src_data[src_off..src_end]);
            }
        }
    }

    fn draw_window_placeholder(
        &self,
        buf: &mut DrawBuffer,
        window: &UserWindowInfo,
        clip: &DamageRect,
    ) {
        let wx = window.x;
        let wy = window.y;
        let ww = window.width as i32;
        let wh = window.height as i32;

        gfx::fill_rect_clipped(buf, wx, wy, ww, wh, COLOR_WINDOW_PLACEHOLDER, clip);

        gfx::fill_rect_clipped(buf, wx, wy, ww, 1, COLOR_TITLE_BAR, clip);
        gfx::fill_rect_clipped(buf, wx, wy + wh - 1, ww, 1, COLOR_TITLE_BAR, clip);
        gfx::fill_rect_clipped(buf, wx, wy, 1, wh, COLOR_TITLE_BAR, clip);
        gfx::fill_rect_clipped(buf, wx + ww - 1, wy, 1, wh, COLOR_TITLE_BAR, clip);

        let text = "Window content pending migration";
        let text_x = wx + 10;
        let text_y = wy + wh / 2 - 8;
        gfx::draw_str_clipped(
            buf,
            text_x,
            text_y,
            text,
            COLOR_TEXT,
            COLOR_WINDOW_PLACEHOLDER,
            clip,
        );
    }
}

fn full_screen_clip(buf: &DrawBuffer) -> DamageRect {
    DamageRect {
        x0: 0,
        y0: 0,
        x1: buf.width() as i32 - 1,
        y1: buf.height() as i32 - 1,
    }
}

fn intersect_rect(a: &DamageRect, b: &DamageRect) -> Option<DamageRect> {
    let x0 = a.x0.max(b.x0);
    let y0 = a.y0.max(b.y0);
    let x1 = a.x1.min(b.x1);
    let y1 = a.y1.min(b.y1);
    if x0 <= x1 && y0 <= y1 {
        Some(DamageRect { x0, y0, x1, y1 })
    } else {
        None
    }
}

fn title_to_str(title: &[u8; 32]) -> &str {
    let len = title.iter().position(|&b| b == 0).unwrap_or(32);
    if len == 0 {
        return "";
    }
    core::str::from_utf8(&title[..len]).unwrap_or("<invalid>")
}

fn cursor_bounds(mx: i32, my: i32, cursor_shape: u8) -> DamageRect {
    match cursor_shape {
        1 => DamageRect {
            x0: mx - 2,
            y0: my - 8,
            x1: mx + 2,
            y1: my + 7,
        },
        _ => DamageRect {
            x0: mx,
            y0: my,
            x1: mx + 11,
            y1: my + 16,
        },
    }
}

fn draw_button_clipped(
    buf: &mut DrawBuffer,
    x: i32,
    y: i32,
    size: i32,
    label: &str,
    hover: bool,
    is_close: bool,
    clip: &DamageRect,
) {
    let color = if hover && is_close {
        COLOR_BUTTON_CLOSE_HOVER
    } else if hover {
        COLOR_BUTTON_HOVER
    } else {
        COLOR_BUTTON
    };
    gfx::fill_rect_clipped(buf, x, y, size, size, color, clip);
    gfx::draw_str_clipped(
        buf,
        x + size / 4,
        y + size / 4,
        label,
        COLOR_TEXT,
        color,
        clip,
    );
}
