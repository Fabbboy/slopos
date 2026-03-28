use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_font::FontRenderer;

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;
use crate::theme::*;

use super::decorations;
use super::dock::LauncherShelf;
use super::hover::HoverRegistry;
use super::menu_bar::SystemBar;
use super::output::{RenderMode, WINDOW_STATE_MINIMIZED};
use super::surface_cache::ClientSurfaceCache;

const COLOR_WINDOW_PLACEHOLDER: Color32 = Color32::rgb(0x20, 0x20, 0x30);

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

        if let Some(data) = crate::gfx::font_loader::load_font("sans") {
            self.ttf_font =
                FontRenderer::new_with_source(data, slopos_font::FontSource::Filesystem);
        }
    }

    pub fn set_output_info(&mut self, width: u32, height: u32, bytes_pp: u8, pitch: usize) {
        self.output_width = width;
        self.output_height = height;
        self.output_bytes_pp = bytes_pp;
        self.output_pitch = pitch;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        buf: &mut DrawBuffer,
        windows: &[UserWindowInfo],
        window_count: usize,
        focused_task: u32,
        signal_hovered_task: u32,
        mouse_x: i32,
        mouse_y: i32,
        cursor_shape: u8,
        _hover: &HoverRegistry,
        surface_cache: &mut ClientSurfaceCache,
        system_bar: &mut SystemBar,
        shelf: &mut LauncherShelf,
        active_app_name: &str,
        uptime_secs: u64,
        force_full: bool,
        damage_regions: &[DamageRect],
    ) -> RenderMode {
        self.ensure_font();

        if force_full {
            let full_clip = full_screen_clip(buf);
            // 1. Desktop background
            gfx::fill_rect(
                buf,
                0,
                0,
                buf.width() as i32,
                buf.height() as i32,
                DESKTOP_BG,
            );

            // 2. Windows (z-order)
            for i in 0..window_count {
                let window = windows[i];
                if window.state == WINDOW_STATE_MINIMIZED {
                    continue;
                }
                // 2a. Shadow
                self.draw_window_shadow(buf, &window, &full_clip);
                // 2b. Content (window.y is the content top from the kernel)
                self.draw_window_content(buf, &window, &full_clip, surface_cache);
                // 2c. Decorations (title bar is above window.y)
                let focused = window.task_id == focused_task;
                let sig_hovered = window.task_id == signal_hovered_task;
                let title = title_to_str(&window.title);
                let frame_y = window.y - TITLE_BAR_HEIGHT;
                decorations::draw_window_decorations(
                    buf,
                    window.x,
                    frame_y,
                    window.width,
                    window.height,
                    title,
                    focused,
                    sig_hovered,
                    self.ttf_font.as_mut(),
                    Some(full_clip),
                );
            }

            // 3. Shelf (dock)
            shelf.draw(
                buf,
                self.output_width,
                self.output_height,
                mouse_x,
                mouse_y,
                self.ttf_font.as_mut(),
                Some(full_clip),
            );

            // 4. System bar (top)
            system_bar.draw(
                buf,
                self.output_width,
                active_app_name,
                uptime_secs,
                self.ttf_font.as_mut(),
                Some(full_clip),
            );

            // 5. Cursor
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
                    signal_hovered_task,
                    mouse_x,
                    mouse_y,
                    cursor_shape,
                    surface_cache,
                    system_bar,
                    shelf,
                    active_app_name,
                    uptime_secs,
                );
            }
            RenderMode::Partial
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_partial_region(
        &mut self,
        buf: &mut DrawBuffer,
        damage: &DamageRect,
        windows: &[UserWindowInfo],
        window_count: usize,
        focused_task: u32,
        signal_hovered_task: u32,
        mouse_x: i32,
        mouse_y: i32,
        cursor_shape: u8,
        surface_cache: &mut ClientSurfaceCache,
        system_bar: &mut SystemBar,
        shelf: &mut LauncherShelf,
        active_app_name: &str,
        uptime_secs: u64,
    ) {
        if !damage.is_valid() {
            return;
        }

        // 1. Desktop background (clipped)
        gfx::fill_rect(
            buf,
            damage.x0,
            damage.y0,
            damage.x1 - damage.x0 + 1,
            damage.y1 - damage.y0 + 1,
            DESKTOP_BG,
        );

        // 2. Windows (z-order)
        for i in 0..window_count {
            let window = windows[i];
            if window.state == WINDOW_STATE_MINIMIZED {
                continue;
            }

            if intersect_rect(damage, &shadow_bounds(&window)).is_some() {
                self.draw_window_shadow(buf, &window, damage);
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

            let frame_y = window.y - TITLE_BAR_HEIGHT;
            let title_rect = DamageRect {
                x0: window.x,
                y0: frame_y,
                x1: window.x + window.width as i32 - 1,
                y1: window.y - 1,
            };
            if intersect_rect(damage, &title_rect).is_some() {
                let focused = window.task_id == focused_task;
                let sig_hovered = window.task_id == signal_hovered_task;
                let title = title_to_str(&window.title);
                decorations::draw_window_decorations(
                    buf,
                    window.x,
                    frame_y,
                    window.width,
                    window.height,
                    title,
                    focused,
                    sig_hovered,
                    self.ttf_font.as_mut(),
                    Some(*damage),
                );
            }
        }

        // 3. Shelf (dock) -- always repaint if damage intersects shelf bounds
        let shelf_bounds = shelf.bounds();
        if shelf_bounds.is_valid() && intersect_rect(damage, &shelf_bounds).is_some() {
            shelf.draw(
                buf,
                self.output_width,
                self.output_height,
                mouse_x,
                mouse_y,
                self.ttf_font.as_mut(),
                Some(*damage),
            );
        }

        // 4. System bar (top)
        let bar_rect = DamageRect {
            x0: 0,
            y0: 0,
            x1: buf.width() as i32 - 1,
            y1: SYSTEM_BAR_HEIGHT,
        };
        if intersect_rect(damage, &bar_rect).is_some() {
            system_bar.draw(
                buf,
                self.output_width,
                active_app_name,
                uptime_secs,
                self.ttf_font.as_mut(),
                Some(*damage),
            );
        }

        // 5. Cursor
        let cursor_rect = cursor_bounds(mouse_x, mouse_y, cursor_shape);
        if intersect_rect(damage, &cursor_rect).is_some() {
            self.draw_cursor(buf, mouse_x, mouse_y, cursor_shape, damage);
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
        // Classic arrow pointer (12x17, hotspot at top-left corner)
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

    fn draw_window_shadow(&self, buf: &mut DrawBuffer, window: &UserWindowInfo, clip: &DamageRect) {
        let ww = window.width as i32;
        let wh = window.height as i32 + TITLE_BAR_HEIGHT;
        let sx = window.x;
        let sy = window.y - TITLE_BAR_HEIGHT + SHADOW_OFFSET_Y;
        let spread = SHADOW_SPREAD;

        // Pre-compute alpha profile: alpha[d-1] = shadow alpha at distance d.
        // Using quadratic falloff matching the original concentric-rect approach.
        let spread_sq = (spread * spread) as u32;
        let mut inv_alpha = [0u32; 12]; // 255 - alpha, for fast black-shadow blend
        for d in 1..=spread {
            let t = (spread - d) as u32;
            let a = (SHADOW_MAX_ALPHA as u32 * t * t) / spread_sq;
            inv_alpha[(d - 1) as usize] = 255 - a;
        }

        let buf_w = buf.width() as i32;
        let buf_h = buf.height() as i32;
        let bpp = buf.bytes_per_pixel() as usize;
        let pitch = buf.pitch_bytes();

        // Blend a single pixel at (px, py) with black at the given inv_alpha.
        // For black shadow: out = dst * inv_alpha / 255, using the RB/AG trick.
        #[inline(always)]
        fn shadow_blend_pixel(buf: &mut DrawBuffer, off: usize, inv_a: u32) {
            let dst = buf.read_encoded_at(off);
            let rb = (dst & 0x00FF00FF) * inv_a + 0x00800080;
            let ag = ((dst >> 8) & 0x00FF00FF) * inv_a + 0x00800080;
            let result = (rb >> 8 & 0x00FF00FF) | (ag >> 8 & 0x00FF00FF) << 8;
            buf.write_encoded_at(off, EncodedPixel(result));
        }

        // Process shadow in 4 regions: top strip, bottom strip, left strip, right strip.
        // Each strip is `spread` pixels wide/tall. Corners are handled by overlap.

        // Top strip: rows from (sy - spread) to (sy - 1)
        for d in 1..=spread {
            let row = sy - d;
            if row < clip.y0 || row > clip.y1 {
                continue;
            }
            let ia = inv_alpha[(d - 1) as usize];
            if ia == 255 {
                continue; // fully transparent shadow at this distance
            }
            let x_start = (sx - d).max(clip.x0).max(0);
            let x_end = (sx + ww - 1 + d).min(clip.x1).min(buf_w - 1);
            if x_start > x_end {
                continue;
            }
            let base = (row as usize) * pitch;
            for px in x_start..=x_end {
                shadow_blend_pixel(buf, base + (px as usize) * bpp, ia);
            }
        }

        // Bottom strip: rows from (sy + wh) to (sy + wh + spread - 1)
        for d in 1..=spread {
            let row = sy + wh + d - 1;
            if row < clip.y0 || row > clip.y1 {
                continue;
            }
            let ia = inv_alpha[(d - 1) as usize];
            if ia == 255 {
                continue;
            }
            let x_start = (sx - d).max(clip.x0).max(0);
            let x_end = (sx + ww - 1 + d).min(clip.x1).min(buf_w - 1);
            if x_start > x_end {
                continue;
            }
            let base = (row as usize) * pitch;
            for px in x_start..=x_end {
                shadow_blend_pixel(buf, base + (px as usize) * bpp, ia);
            }
        }

        // Left strip: rows from sy to (sy + wh - 1), columns (sx - spread) to (sx - 1)
        for row in sy.max(clip.y0)..=(sy + wh - 1).min(clip.y1).min(buf_h - 1) {
            let base = (row as usize) * pitch;
            for d in 1..=spread {
                let col = sx - d;
                if col < clip.x0 || col > clip.x1 || col < 0 || col >= buf_w {
                    continue;
                }
                let ia = inv_alpha[(d - 1) as usize];
                if ia == 255 {
                    continue;
                }
                shadow_blend_pixel(buf, base + (col as usize) * bpp, ia);
            }
        }

        // Right strip: rows from sy to (sy + wh - 1), columns (sx + ww) to (sx + ww + spread - 1)
        for row in sy.max(clip.y0)..=(sy + wh - 1).min(clip.y1).min(buf_h - 1) {
            let base = (row as usize) * pitch;
            for d in 1..=spread {
                let col = sx + ww - 1 + d;
                if col < clip.x0 || col > clip.x1 || col < 0 || col >= buf_w {
                    continue;
                }
                let ia = inv_alpha[(d - 1) as usize];
                if ia == 255 {
                    continue;
                }
                shadow_blend_pixel(buf, base + (col as usize) * bpp, ia);
            }
        }
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

        // window.y from the kernel is the content top (title bar is above it).
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

        gfx::fill_rect_clipped(buf, wx, wy, ww, 1, TITLE_BAR_UNFOCUSED, clip);
        gfx::fill_rect_clipped(buf, wx, wy + wh - 1, ww, 1, TITLE_BAR_UNFOCUSED, clip);
        gfx::fill_rect_clipped(buf, wx, wy, 1, wh, TITLE_BAR_UNFOCUSED, clip);
        gfx::fill_rect_clipped(buf, wx + ww - 1, wy, 1, wh, TITLE_BAR_UNFOCUSED, clip);

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

fn shadow_bounds(window: &UserWindowInfo) -> DamageRect {
    let sx = window.x;
    let sy = window.y - TITLE_BAR_HEIGHT + SHADOW_OFFSET_Y;
    let ww = window.width as i32;
    let wh = window.height as i32 + TITLE_BAR_HEIGHT;
    DamageRect {
        x0: sx - SHADOW_SPREAD,
        y0: sy - SHADOW_SPREAD,
        x1: sx + ww - 1 + SHADOW_SPREAD,
        y1: sy + wh - 1 + SHADOW_SPREAD,
    }
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
