use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;
use slopos_font::FontRenderer;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_image::{DecodeOptions, Image};

use crate::gfx::{self, DamageRect, DrawBuffer};
use crate::syscall::UserWindowInfo;
use crate::theme::*;

use super::decorations;
use super::dock::LauncherShelf;
use super::menu_bar::SystemBar;
use super::output::WINDOW_STATE_MINIMIZED;
use super::region::Region;
use super::surface_cache::ClientSurfaceCache;

const COLOR_WINDOW_PLACEHOLDER: Color32 = Color32::rgb(0x20, 0x20, 0x30);
const DEFAULT_WALLPAPER: &str = "/usr/share/slopos/wallpapers/default.png";

/// Hardware cursor overlay dimensions (virtio-gpu mandates 64×64).
pub const HW_CURSOR_DIM: u32 = 64;
/// Uniform hotspot offset used when rasterizing any cursor shape into the
/// hardware overlay. Each cursor shape draws its click-point at this reference,
/// so the overlay's hotspot lands on the pointer position for every shape.
pub const HW_CURSOR_HOTSPOT: u32 = 20;

struct WallpaperCache {
    load_attempted: bool,
    source: Option<Image>,
    cache: Vec<u8>,
    cache_width: u32,
    cache_height: u32,
    cache_pitch: usize,
    cache_bpp: u8,
    cache_format: PixelFormat,
}

impl WallpaperCache {
    fn new() -> Self {
        Self {
            load_attempted: false,
            source: None,
            cache: Vec::new(),
            cache_width: 0,
            cache_height: 0,
            cache_pitch: 0,
            cache_bpp: 0,
            cache_format: PixelFormat::Argb8888,
        }
    }

    fn draw(&mut self, buf: &mut DrawBuffer, clip: &DamageRect) -> bool {
        if !clip.is_valid() || !self.ensure_cache(buf) {
            return false;
        }
        let clipped = clip.clip(buf.width() as i32, buf.height() as i32);
        if !clipped.is_valid() {
            return false;
        }

        let bpp = buf.bytes_pp() as usize;
        let dst_pitch = buf.pitch();
        let src_pitch = self.cache_pitch;
        let row_bytes = (clipped.x1 - clipped.x0 + 1) as usize * bpp;
        let x_byte = clipped.x0 as usize * bpp;
        let dst_data = buf.data_mut();
        for row in clipped.y0..=clipped.y1 {
            let row = row as usize;
            let src_off = row * src_pitch + x_byte;
            let dst_off = row * dst_pitch + x_byte;
            let src = &self.cache[src_off..src_off + row_bytes];
            dst_data[dst_off..dst_off + row_bytes].copy_from_slice(src);
        }
        buf.add_damage(clipped.x0, clipped.y0, clipped.x1, clipped.y1);
        true
    }

    fn ensure_cache(&mut self, buf: &mut DrawBuffer) -> bool {
        if !self.load_attempted {
            self.load_attempted = true;
            let path =
                std::env::var("SLOPOS_WALLPAPER").unwrap_or_else(|_| DEFAULT_WALLPAPER.into());
            self.source = slopos_image::load_path(path, DecodeOptions::default()).ok();
        }

        let Some(source) = self.source.as_ref() else {
            return false;
        };
        let width = buf.width();
        let height = buf.height();
        let pitch = buf.pitch();
        let bpp = buf.bytes_pp();
        let format = buf.pixel_format();
        if self.cache_width == width
            && self.cache_height == height
            && self.cache_pitch == pitch
            && self.cache_bpp == bpp
            && self.cache_format == format
            && !self.cache.is_empty()
        {
            return true;
        }

        let Some(size) = pitch.checked_mul(height as usize) else {
            self.cache.clear();
            return false;
        };
        self.cache.clear();
        self.cache.resize(size, 0);

        let Some(mut cache_buf) = DrawBuffer::new(&mut self.cache, width, height, pitch, bpp)
        else {
            self.cache.clear();
            return false;
        };
        cache_buf.set_pixel_format(format);
        cache_buf.clear_canvas(format.encode(DESKTOP_BG));
        let Some(bitmap) = BitmapRef::new(source.width, source.height, &source.pixels) else {
            self.cache.clear();
            return false;
        };
        slopos_gfx::image::draw_image(
            &mut cache_buf,
            bitmap,
            0,
            0,
            width as i32,
            height as i32,
            ImageFit::Cover,
            ImageSampling::Bilinear,
        );

        self.cache_width = width;
        self.cache_height = height;
        self.cache_pitch = pitch;
        self.cache_bpp = bpp;
        self.cache_format = format;
        true
    }
}

pub struct Renderer {
    pub output_width: u32,
    pub output_height: u32,
    pub output_bytes_pp: u8,
    pub output_pitch: usize,
    ttf_font: Option<FontRenderer<'static>>,
    ttf_init_attempted: bool,
    wallpaper: WallpaperCache,
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
            wallpaper: WallpaperCache::new(),
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

    /// Composite one frame into `buf`, repainting exactly the damaged,
    /// non-occluded pixels in z-order.
    ///
    /// Walking windows front-to-back, each opaque content box is carved out of
    /// the still-exposed region, yielding per window the damaged pixels it owns,
    /// where its shadow may fall, and the background remainder no opaque window
    /// covers. A window's decorations are confined to that exposed region and its
    /// shadow to a separate shadow-availability region; both already exclude every
    /// higher window, so a lower window's chrome can never paint over a window
    /// stacked above it. `frame_damage` is the precise, disjoint region to
    /// repaint; a full-output region drives a full redraw.
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
        surface_cache: &mut ClientSurfaceCache,
        system_bar: &mut SystemBar,
        shelf: &mut LauncherShelf,
        active_app_name: &str,
        uptime_secs: u64,
        frame_damage: &Region,
        hw_cursor: bool,
    ) {
        self.ensure_font();

        let output_rect = full_screen_clip(buf);
        // Clip damage to the output; the region is already disjoint so blended
        // primitives (shadows) never touch a pixel twice.
        let frame_damage = frame_damage.intersect_rect(&output_rect);
        if frame_damage.is_empty() {
            return;
        }

        // ── Occlusion pass (front-to-back) ──────────────────────────────────
        let mut order: Vec<usize> = Vec::with_capacity(window_count);
        for i in 0..window_count {
            if windows[i].state != WINDOW_STATE_MINIMIZED {
                order.push(i);
            }
        }

        // Two independent occlusion accumulators, both walked top→bottom:
        //  - `exposed`: damaged pixels not yet covered by an opaque content box,
        //    used for content/decoration visibility and the background remainder.
        //  - `shadow_avail`: damaged pixels no higher window has already claimed
        //    a shadow (or frame footprint) over. Each window's shadow region is a
        //    translucent black blend, so to avoid double-darkening shared gap
        //    pixels the topmost window owns them; subtracting each window's full
        //    shadow box keeps every `shadow_vis` mutually disjoint AND keeps a
        //    lower shadow off a higher window's content (its box ⊇ its content).
        let mut exposed = frame_damage.clone();
        let mut shadow_avail = frame_damage.clone();
        let mut content_vis: Vec<Region> = Vec::with_capacity(order.len());
        let mut shadow_vis: Vec<Region> = Vec::with_capacity(order.len());
        content_vis.resize(order.len(), Region::new());
        shadow_vis.resize(order.len(), Region::new());

        for k in (0..order.len()).rev() {
            let w = &windows[order[k]];
            let fbox = frame_box(w);
            let cbox = content_box(w);
            let sbox = shadow_bounds(w);

            // Content + decoration area still visible (not under higher opaque
            // content).
            content_vis[k] = exposed.intersect_rect(&fbox);
            // Shadow halo this window owns, minus its own opaque content (which
            // overwrites it — avoids the seam double-darken).
            let mut sv = shadow_avail.intersect_rect(&sbox);
            sv.subtract(&cbox);
            shadow_vis[k] = sv;

            // This window claims its whole shadow/frame footprint, and its opaque
            // content occludes everything strictly below it.
            shadow_avail.subtract(&sbox);
            exposed.subtract(&cbox);
        }
        let background_visible = exposed;

        // ── Paint ───────────────────────────────────────────────────────────
        // 1. Desktop background, only where no opaque window covers it.
        for rect in background_visible.rects() {
            if !self.draw_wallpaper(buf, rect) {
                gfx::fill_rect(
                    buf,
                    rect.x0,
                    rect.y0,
                    rect.x1 - rect.x0 + 1,
                    rect.y1 - rect.y0 + 1,
                    DESKTOP_BG,
                );
            }
        }

        // 2. Windows bottom→top: shadow, then content + decorations, each
        //    clipped to its occlusion-masked visible region.
        for k in 0..order.len() {
            let w = windows[order[k]];
            for rect in shadow_vis[k].rects() {
                self.draw_window_shadow(buf, &w, rect);
            }
            if content_vis[k].is_empty() {
                continue;
            }
            let focused = w.task_id == focused_task;
            let sig_hovered = w.task_id == signal_hovered_task;
            let title = title_to_str(&w.title);
            let frame_y = w.y - TITLE_BAR_HEIGHT;
            for rect in content_vis[k].rects() {
                buf.set_scissor(Some(*rect));
                self.draw_window_content(buf, &w, rect, surface_cache);
                decorations::draw_window_decorations(
                    buf,
                    w.x,
                    frame_y,
                    w.effective_width(),
                    w.effective_height(),
                    title,
                    focused,
                    sig_hovered,
                    self.ttf_font.as_mut(),
                    Some(*rect),
                );
                buf.set_scissor(None);
            }
        }

        // 3. Shelf (dock) + 4. system bar: always-on-top chrome. Each self-clips
        //    to the passed rect (the dock early-returns when empty), so drawing
        //    them per damaged rect paints their portion once across the disjoint
        //    region. The dock is drawn unconditionally because its bounds are
        //    only known after it has drawn.
        let bar_rect = DamageRect {
            x0: 0,
            y0: 0,
            x1: buf.width() as i32 - 1,
            y1: SYSTEM_BAR_HEIGHT,
        };
        for rect in frame_damage.rects() {
            buf.set_scissor(Some(*rect));
            shelf.draw(
                buf,
                self.output_width,
                self.output_height,
                mouse_x,
                mouse_y,
                self.ttf_font.as_mut(),
                Some(*rect),
            );
            if intersect_rect(rect, &bar_rect).is_some() {
                system_bar.draw(
                    buf,
                    self.output_width,
                    active_app_name,
                    uptime_secs,
                    self.ttf_font.as_mut(),
                    Some(*rect),
                );
            }
            buf.set_scissor(None);
        }

        // 5. Software cursor (skipped when the hardware overlay owns the pointer).
        if !hw_cursor {
            let cursor_rect = cursor_bounds(mouse_x, mouse_y, cursor_shape);
            for rect in frame_damage.rects() {
                if intersect_rect(rect, &cursor_rect).is_some() {
                    self.draw_cursor(buf, mouse_x, mouse_y, cursor_shape, rect);
                }
            }
        }
    }

    /// Rasterize `cursor_shape` into a 64×64 BGRA hardware-cursor image.
    /// `out` must be `HW_CURSOR_DIM * HW_CURSOR_DIM * 4` bytes; it is filled
    /// with a transparent background and the cursor drawn at the hotspot
    /// reference so the overlay tracks the pointer correctly.
    pub fn render_cursor_image(&self, cursor_shape: u8, out: &mut [u8]) {
        for b in out.iter_mut() {
            *b = 0; // transparent (alpha 0)
        }
        let Some(mut buf) = DrawBuffer::new(
            out,
            HW_CURSOR_DIM,
            HW_CURSOR_DIM,
            (HW_CURSOR_DIM * 4) as usize,
            4,
        ) else {
            return;
        };
        buf.set_pixel_format(PixelFormat::Argb8888);
        let clip = DamageRect {
            x0: 0,
            y0: 0,
            x1: HW_CURSOR_DIM as i32 - 1,
            y1: HW_CURSOR_DIM as i32 - 1,
        };
        self.draw_cursor(
            &mut buf,
            HW_CURSOR_HOTSPOT as i32,
            HW_CURSOR_HOTSPOT as i32,
            cursor_shape,
            &clip,
        );
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
            3 | 4 => self.draw_cursor_ns(buf, mx, my, clip),
            5 | 6 => self.draw_cursor_ew(buf, mx, my, clip),
            7 | 10 => self.draw_cursor_nwse(buf, mx, my, clip),
            8 | 9 => self.draw_cursor_nesw(buf, mx, my, clip),
            11 => self.draw_cursor_grab(buf, mx, my, clip),
            12 => self.draw_cursor_grabbing(buf, mx, my, clip),
            _ => self.draw_cursor_default(buf, mx, my, clip),
        }
    }

    fn draw_wallpaper(&mut self, buf: &mut DrawBuffer, clip: &DamageRect) -> bool {
        self.wallpaper.draw(buf, clip)
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

    /// Vertical double-arrow cursor (N/S resize). 9×17, hotspot centered.
    fn draw_cursor_ns(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 9;
        const H: usize = 17;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,0,1,0,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,1,2,2,2,1,0,0],
            [0,1,2,2,2,2,2,1,0],
            [1,2,2,2,2,2,2,2,1],
            [1,1,1,1,2,1,1,1,1],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,1,2,1,0,0,0],
            [1,1,1,1,2,1,1,1,1],
            [1,2,2,2,2,2,2,2,1],
            [0,1,2,2,2,2,2,1,0],
            [0,0,1,2,2,2,1,0,0],
            [0,0,0,1,2,1,0,0,0],
            [0,0,0,0,1,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 4, my - 8, &BITMAP, clip);
    }

    /// Horizontal double-arrow cursor (E/W resize). 17×9, hotspot centered.
    fn draw_cursor_ew(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 17;
        const H: usize = 9;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,0,1,0,0,0,0,0,0,0,1,0,0,0,0],
            [0,0,0,1,1,0,0,0,0,0,0,0,1,1,0,0,0],
            [0,0,1,2,1,0,0,0,0,0,0,0,1,2,1,0,0],
            [0,1,2,2,1,1,1,1,1,1,1,1,1,2,2,1,0],
            [1,2,2,2,2,2,2,2,2,2,2,2,2,2,2,2,1],
            [0,1,2,2,1,1,1,1,1,1,1,1,1,2,2,1,0],
            [0,0,1,2,1,0,0,0,0,0,0,0,1,2,1,0,0],
            [0,0,0,1,1,0,0,0,0,0,0,0,1,1,0,0,0],
            [0,0,0,0,1,0,0,0,0,0,0,0,1,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 8, my - 4, &BITMAP, clip);
    }

    /// Diagonal double-arrow cursor (NW/SE resize). 15×15, hotspot centered.
    fn draw_cursor_nwse(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 15;
        const H: usize = 15;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [1,1,1,1,1,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,1,2,2,2,1,0,0,0,0,0,0,0,0],
            [1,1,0,1,2,2,2,1,0,0,0,0,0,0,0],
            [0,0,0,0,1,2,2,2,1,0,0,0,0,0,0],
            [0,0,0,0,0,1,2,2,2,1,0,0,0,0,0],
            [0,0,0,0,0,0,1,2,2,2,1,0,0,0,0],
            [0,0,0,0,0,0,0,1,2,2,2,1,0,1,1],
            [0,0,0,0,0,0,0,0,1,2,2,2,1,2,1],
            [0,0,0,0,0,0,0,0,0,1,2,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,0,1,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,0,1,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,1,1,1,1,1,1],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 7, my - 7, &BITMAP, clip);
    }

    /// Diagonal double-arrow cursor (NE/SW resize). 15×15, hotspot centered.
    fn draw_cursor_nesw(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 15;
        const H: usize = 15;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,0,0,0,0,0,0,1,1,1,1,1,1],
            [0,0,0,0,0,0,0,0,0,1,2,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,0,1,2,2,2,1],
            [0,0,0,0,0,0,0,0,0,1,2,2,2,2,1],
            [0,0,0,0,0,0,0,0,1,2,2,2,1,2,1],
            [0,0,0,0,0,0,0,1,2,2,2,1,0,1,1],
            [0,0,0,0,0,0,1,2,2,2,1,0,0,0,0],
            [0,0,0,0,0,1,2,2,2,1,0,0,0,0,0],
            [0,0,0,0,1,2,2,2,1,0,0,0,0,0,0],
            [1,1,0,1,2,2,2,1,0,0,0,0,0,0,0],
            [1,2,1,2,2,2,1,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,1,0,0,0,0,0,0,0,0,0,0],
            [1,2,2,2,2,1,0,0,0,0,0,0,0,0,0],
            [1,1,1,1,1,1,0,0,0,0,0,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 7, my - 7, &BITMAP, clip);
    }

    /// Open hand cursor (grab/ready to drag). 15×16, hotspot at (6,1).
    fn draw_cursor_grab(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 15;
        const H: usize = 16;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,0,1,1,0,1,1,0,0,0,0,0,0,0],
            [0,0,1,2,2,1,2,2,1,0,0,0,0,0,0],
            [0,0,1,2,2,1,2,2,1,1,1,0,0,0,0],
            [0,0,1,2,2,1,2,2,1,2,2,1,0,0,0],
            [0,0,1,2,2,1,2,2,1,2,2,1,1,0,0],
            [1,1,1,2,2,1,2,2,1,2,2,1,2,1,0],
            [1,2,1,2,2,2,2,2,2,2,2,2,2,1,0],
            [1,2,1,2,2,2,2,2,2,2,2,2,2,1,0],
            [0,1,2,2,2,2,2,2,2,2,2,2,1,0,0],
            [0,1,2,2,2,2,2,2,2,2,2,2,1,0,0],
            [0,0,1,2,2,2,2,2,2,2,2,1,0,0,0],
            [0,0,1,2,2,2,2,2,2,2,2,1,0,0,0],
            [0,0,0,1,2,2,2,2,2,2,1,0,0,0,0],
            [0,0,0,1,2,2,2,2,2,2,1,0,0,0,0],
            [0,0,0,0,1,2,2,2,2,1,0,0,0,0,0],
            [0,0,0,0,0,1,1,1,1,0,0,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 6, my - 1, &BITMAP, clip);
    }

    /// Closed fist cursor (grabbing/active drag). 13×13, hotspot at (5,3).
    fn draw_cursor_grabbing(&self, buf: &mut DrawBuffer, mx: i32, my: i32, clip: &DamageRect) {
        const W: usize = 13;
        const H: usize = 13;
        #[rustfmt::skip]
        const BITMAP: [[u8; W]; H] = [
            [0,0,1,1,0,1,1,0,1,1,0,0,0],
            [0,1,2,2,1,2,2,1,2,2,1,0,0],
            [0,1,2,2,1,2,2,1,2,2,1,1,0],
            [1,1,2,2,2,2,2,2,2,2,1,2,1],
            [1,2,2,2,2,2,2,2,2,2,2,2,1],
            [1,2,2,2,2,2,2,2,2,2,2,2,1],
            [0,1,2,2,2,2,2,2,2,2,2,1,0],
            [0,1,2,2,2,2,2,2,2,2,2,1,0],
            [0,0,1,2,2,2,2,2,2,2,1,0,0],
            [0,0,1,2,2,2,2,2,2,2,1,0,0],
            [0,0,0,1,2,2,2,2,2,1,0,0,0],
            [0,0,0,1,2,2,2,2,2,1,0,0,0],
            [0,0,0,0,1,1,1,1,1,0,0,0,0],
        ];
        self.draw_cursor_bitmap::<W, H>(buf, mx - 5, my - 3, &BITMAP, clip);
    }

    /// Generic bitmap cursor renderer. 0=transparent, 1=border(black), 2=fill(white).
    fn draw_cursor_bitmap<const W: usize, const H: usize>(
        &self,
        buf: &mut DrawBuffer,
        ox: i32,
        oy: i32,
        bitmap: &[[u8; W]; H],
        clip: &DamageRect,
    ) {
        const BORDER: Color32 = Color32::rgb(0x00, 0x00, 0x00);
        for row in 0..H {
            let py = oy + row as i32;
            let mut col = 0;
            while col < W {
                let pixel = bitmap[row][col];
                if pixel == 0 {
                    col += 1;
                    continue;
                }
                let color = if pixel == 1 { BORDER } else { COLOR_CURSOR };
                let start = col;
                while col < W && bitmap[row][col] == pixel {
                    col += 1;
                }
                gfx::fill_rect_clipped(
                    buf,
                    ox + start as i32,
                    py,
                    (col - start) as i32,
                    1,
                    color,
                    clip,
                );
            }
        }
    }

    fn draw_window_shadow(&self, buf: &mut DrawBuffer, window: &UserWindowInfo, clip: &DamageRect) {
        let ww = window.effective_width() as i32;
        let wh = window.effective_height() as i32 + TITLE_BAR_HEIGHT;
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

        // Use BUFFER dimensions (not frame dimensions) for content blit.
        // During resize, frame_width/frame_height may be larger than the
        // actual SHM buffer; using them for pitch would cause corruption.
        let buf_w = window.width;
        let buf_h = window.height;
        let src_pitch = (buf_w as usize) * bytes_pp;
        let buffer_size = src_pitch * (buf_h as usize);

        // Frame dimensions for the content area rect (may be larger during resize)
        let frame_w = window.effective_width() as i32;
        let frame_h = window.effective_height() as i32;

        let cache_index = match surface_cache.get_or_create_index(
            window.task_id,
            window.buffer_generation,
            window.buffer_id,
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
        let out_w = buf.width() as i32;
        let out_h = buf.height() as i32;

        // Frame rect (the full content area the compositor allocated)
        let frame_rect = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + frame_w - 1,
            y1: window.y + frame_h - 1,
        };

        // Wayland model: always show the last committed buffer, clipped to
        // the frame boundary.  During shrink the content is cropped; during
        // grow a placeholder fills the gap.  No timing-dependent "resizing"
        // flag — the compositor never shows an inconsistent state.
        let visible_w = (buf_w as i32).min(frame_w);
        let visible_h = (buf_h as i32).min(frame_h);

        let content_rect = DamageRect {
            x0: window.x,
            y0: window.y,
            x1: window.x + visible_w - 1,
            y1: window.y + visible_h - 1,
        };

        if let Some(draw_rect) = intersect_rect(clip, &content_rect) {
            let x0 = draw_rect.x0.max(0);
            let y0 = draw_rect.y0.max(0);
            let x1 = (draw_rect.x1 + 1).min(out_w);
            let y1 = (draw_rect.y1 + 1).min(out_h);

            if x0 < x1 && y0 < y1 {
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
        }

        // Fill gap between committed content and frame (grow case).
        if frame_w > buf_w as i32 {
            let strip = DamageRect {
                x0: window.x + buf_w as i32,
                y0: window.y,
                x1: window.x + frame_w - 1,
                y1: window.y + frame_h - 1,
            };
            if intersect_rect(clip, &strip).is_some() {
                gfx::fill_rect_clipped(
                    buf,
                    strip.x0,
                    strip.y0,
                    strip.x1 - strip.x0 + 1,
                    strip.y1 - strip.y0 + 1,
                    COLOR_WINDOW_PLACEHOLDER,
                    clip,
                );
            }
        }
        if frame_h > buf_h as i32 {
            let strip = DamageRect {
                x0: window.x,
                y0: window.y + buf_h as i32,
                x1: window.x + frame_w - 1,
                y1: window.y + frame_h - 1,
            };
            if intersect_rect(clip, &strip).is_some() {
                gfx::fill_rect_clipped(
                    buf,
                    strip.x0,
                    strip.y0,
                    strip.x1 - strip.x0 + 1,
                    strip.y1 - strip.y0 + 1,
                    COLOR_WINDOW_PLACEHOLDER,
                    clip,
                );
            }
        }

        let _ = frame_rect;
    }

    fn draw_window_placeholder(
        &self,
        buf: &mut DrawBuffer,
        window: &UserWindowInfo,
        clip: &DamageRect,
    ) {
        static PH_LOGGED: core::sync::atomic::AtomicBool =
            core::sync::atomic::AtomicBool::new(false);
        if !PH_LOGGED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            crate::syscall::tty::write(b"DEBUG: draw_window_placeholder called!\n");
        }
        let wx = window.x;
        let wy = window.y;
        let ww = window.effective_width() as i32;
        let wh = window.effective_height() as i32;

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
    let ww = window.effective_width() as i32;
    let wh = window.effective_height() as i32 + TITLE_BAR_HEIGHT;
    DamageRect {
        x0: sx - SHADOW_SPREAD,
        y0: sy - SHADOW_SPREAD,
        x1: sx + ww - 1 + SHADOW_SPREAD,
        y1: sy + wh - 1 + SHADOW_SPREAD,
    }
}

/// The opaque content area of a window (below the title bar). Composited by
/// overwrite, so it is the window's occluder box: everything strictly below it
/// in z-order is hidden here.
fn content_box(window: &UserWindowInfo) -> DamageRect {
    let ew = window.effective_width() as i32;
    let eh = window.effective_height() as i32;
    DamageRect {
        x0: window.x,
        y0: window.y,
        x1: window.x + ew - 1,
        y1: window.y + eh - 1,
    }
}

/// The full window frame: title bar (above `window.y`) plus content area. Used
/// to bound where a window's content and decorations may paint. The title bar
/// is deliberately NOT treated as opaque (its rounded corners let the backdrop
/// show through), so only [`content_box`] is subtracted during occlusion.
fn frame_box(window: &UserWindowInfo) -> DamageRect {
    let ew = window.effective_width() as i32;
    let eh = window.effective_height() as i32;
    DamageRect {
        x0: window.x,
        y0: window.y - TITLE_BAR_HEIGHT,
        x1: window.x + ew - 1,
        y1: window.y + eh - 1,
    }
}

fn cursor_bounds(mx: i32, my: i32, cursor_shape: u8) -> DamageRect {
    match cursor_shape {
        // Text beam
        1 => DamageRect {
            x0: mx - 2,
            y0: my - 8,
            x1: mx + 2,
            y1: my + 7,
        },
        // NS resize (9×17 centered)
        3 | 4 => DamageRect {
            x0: mx - 4,
            y0: my - 8,
            x1: mx + 4,
            y1: my + 8,
        },
        // EW resize (17×9 centered)
        5 | 6 => DamageRect {
            x0: mx - 8,
            y0: my - 4,
            x1: mx + 8,
            y1: my + 4,
        },
        // NWSE / NESW resize (15×15 centered)
        7 | 8 | 9 | 10 => DamageRect {
            x0: mx - 7,
            y0: my - 7,
            x1: mx + 7,
            y1: my + 7,
        },
        // Grab — open hand (15×16, hotspot at (6,1))
        11 => DamageRect {
            x0: mx - 6,
            y0: my - 1,
            x1: mx + 8,
            y1: my + 14,
        },
        // Grabbing — closed fist (13×13, hotspot at (5,3))
        12 => DamageRect {
            x0: mx - 5,
            y0: my - 3,
            x1: mx + 7,
            y1: my + 9,
        },
        // Default arrow (12×17, top-left hotspot)
        _ => DamageRect {
            x0: mx,
            y0: my,
            x1: mx + 11,
            y1: my + 16,
        },
    }
}
