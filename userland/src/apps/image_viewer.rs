use std::env;
use std::path::Path;

use slopos_abi::draw::Color32;
use slopos_gfx::image::{BitmapRef, ImageFit, ImageSampling};
use slopos_image::{DecodeOptions, Image};
use slopos_windowing::{ControlFlow, Event, Window, WindowedApp};

use crate::gfx::{self, DamageRect, DrawBuffer};

const DEFAULT_IMAGE: &str = "/usr/share/slopos/wallpapers/default.png";
const STATUS_HEIGHT: i32 = 22;
const CHECKER_SIZE: i32 = 16;
const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 32.0;

const BG: Color32 = Color32::rgb(0x18, 0x1B, 0x20);
const STATUS_BG: Color32 = Color32::rgb(0x23, 0x26, 0x2B);
const STATUS_FG: Color32 = Color32::rgb(0xE6, 0xE8, 0xEA);
const ERROR_FG: Color32 = Color32::rgb(0xFF, 0xB0, 0xA8);
const CHECKER_A: Color32 = Color32::rgb(0xCF, 0xD4, 0xDA);
const CHECKER_B: Color32 = Color32::rgb(0xF2, 0xF4, 0xF6);

struct ImageViewer {
    path: String,
    loaded: Result<Image, String>,
    fit_mode: bool,
    zoom: f64,
    pan_x: i32,
    pan_y: i32,
    dragging: bool,
    last_x: i32,
    last_y: i32,
}

impl ImageViewer {
    fn new(path: String) -> Self {
        let loaded =
            slopos_image::load_path(&path, DecodeOptions::default()).map_err(|e| format!("{}", e));
        Self {
            path,
            loaded,
            fit_mode: true,
            zoom: 1.0,
            pan_x: 0,
            pan_y: 0,
            dragging: false,
            last_x: 0,
            last_y: 0,
        }
    }

    fn view_height(&self, fb: &DrawBuffer) -> i32 {
        (fb.height() as i32 - STATUS_HEIGHT).max(1)
    }

    fn fit_zoom_for(&self, view_w: i32, view_h: i32) -> f64 {
        let Ok(image) = self.loaded.as_ref() else {
            return 1.0;
        };
        if image.width == 0 || image.height == 0 || view_w <= 0 || view_h <= 0 {
            return 1.0;
        }
        let zx = view_w as f64 / image.width as f64;
        let zy = view_h as f64 / image.height as f64;
        zx.min(zy).clamp(MIN_ZOOM, MAX_ZOOM)
    }

    fn active_zoom(&self, view_w: i32, view_h: i32) -> f64 {
        if self.fit_mode {
            self.fit_zoom_for(view_w, view_h)
        } else {
            self.zoom.clamp(MIN_ZOOM, MAX_ZOOM)
        }
    }

    fn reset_fit(&mut self) {
        self.fit_mode = true;
        self.pan_x = 0;
        self.pan_y = 0;
    }

    fn reset_actual(&mut self) {
        self.fit_mode = false;
        self.zoom = 1.0;
        self.pan_x = 0;
        self.pan_y = 0;
    }

    fn zoom_around(&mut self, px: i32, py: i32, view_w: i32, view_h: i32, factor: f64) {
        let Ok(image) = self.loaded.as_ref() else {
            return;
        };
        let old_zoom = self.active_zoom(view_w, view_h);
        if self.fit_mode {
            self.zoom = old_zoom;
            self.pan_x = 0;
            self.pan_y = 0;
            self.fit_mode = false;
        }
        let old_w = scaled_len(image.width, old_zoom);
        let old_h = scaled_len(image.height, old_zoom);
        let old_x = (view_w - old_w) / 2 + self.pan_x;
        let old_y = (view_h - old_h) / 2 + self.pan_y;
        let image_x = (px - old_x) as f64 / old_zoom;
        let image_y = (py - old_y) as f64 / old_zoom;

        self.zoom = (old_zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let new_w = scaled_len(image.width, self.zoom);
        let new_h = scaled_len(image.height, self.zoom);
        let base_x = (view_w - new_w) / 2;
        let base_y = (view_h - new_h) / 2;
        self.pan_x = px - base_x - (image_x * self.zoom).round() as i32;
        self.pan_y = py - base_y - (image_y * self.zoom).round() as i32;
    }

    fn draw_checkerboard(&self, fb: &mut DrawBuffer<'_>, view_h: i32) {
        gfx::fill_rect(fb, 0, 0, fb.width() as i32, view_h, BG);
        let cols = (fb.width() as i32 + CHECKER_SIZE - 1) / CHECKER_SIZE;
        let rows = (view_h + CHECKER_SIZE - 1) / CHECKER_SIZE;
        for row in 0..rows {
            for col in 0..cols {
                let color = if (row + col) & 1 == 0 {
                    CHECKER_A
                } else {
                    CHECKER_B
                };
                gfx::fill_rect(
                    fb,
                    col * CHECKER_SIZE,
                    row * CHECKER_SIZE,
                    CHECKER_SIZE,
                    CHECKER_SIZE,
                    color,
                );
            }
        }
    }

    fn draw_status(&self, fb: &mut DrawBuffer<'_>, view_h: i32, zoom: f64) {
        gfx::fill_rect(fb, 0, view_h, fb.width() as i32, STATUS_HEIGHT, STATUS_BG);
        let label = self.status_text(zoom);
        let clip = full_clip(fb);
        gfx::draw_str_clipped(fb, 8, view_h + 4, &label, STATUS_FG, STATUS_BG, &clip);
    }

    fn status_text(&self, zoom: f64) -> String {
        let name = Path::new(&self.path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&self.path);
        match self.loaded {
            Ok(ref image) => format!(
                "{}  {}x{}  {}%",
                name,
                image.width,
                image.height,
                (zoom * 100.0).round() as i32
            ),
            Err(_) => format!("{}  load failed", name),
        }
    }
}

impl WindowedApp for ImageViewer {
    fn init(&mut self, win: &mut Window) {
        win.set_title("Image Viewer");
        win.set_app_id("org.slopos.image-viewer");
        win.request_redraw();
    }

    fn on_event(&mut self, win: &mut Window, event: Event) -> ControlFlow {
        let view_w = win.width() as i32;
        let view_h = (win.height() as i32 - STATUS_HEIGHT).max(1);
        match event {
            Event::CloseRequest => ControlFlow::Exit,
            Event::Configure { .. } => {
                win.request_redraw();
                ControlFlow::Continue
            }
            Event::PointerMotion { x, y } => {
                if self.dragging {
                    self.fit_mode = false;
                    self.pan_x += x - self.last_x;
                    self.pan_y += y - self.last_y;
                    win.request_redraw();
                }
                self.last_x = x;
                self.last_y = y;
                ControlFlow::Continue
            }
            Event::PointerPress { .. } => {
                let (x, y) = win.pointer();
                self.dragging = true;
                self.last_x = x;
                self.last_y = y;
                ControlFlow::Continue
            }
            Event::PointerRelease { .. } => {
                self.dragging = false;
                ControlFlow::Continue
            }
            Event::PointerAxis { value_v120, .. } => {
                let (x, y) = win.pointer();
                let factor = if value_v120 > 0 { 1.2 } else { 1.0 / 1.2 };
                self.zoom_around(x, y.min(view_h - 1), view_w, view_h, factor);
                win.request_redraw();
                ControlFlow::Continue
            }
            Event::KeyPress { ascii, .. } => {
                match ascii {
                    b'f' | b'F' => self.reset_fit(),
                    b'0' => self.reset_actual(),
                    b'+' | b'=' => {
                        let (x, y) = win.pointer();
                        self.zoom_around(x, y.min(view_h - 1), view_w, view_h, 1.2);
                    }
                    b'-' | b'_' => {
                        let (x, y) = win.pointer();
                        self.zoom_around(x, y.min(view_h - 1), view_w, view_h, 1.0 / 1.2);
                    }
                    _ => {}
                }
                win.request_redraw();
                ControlFlow::Continue
            }
            _ => ControlFlow::Continue,
        }
    }

    fn draw(&mut self, fb: &mut DrawBuffer<'_>) {
        let view_h = self.view_height(fb);
        let view_w = fb.width() as i32;
        let zoom = self.active_zoom(view_w, view_h);
        self.draw_checkerboard(fb, view_h);

        match self.loaded {
            Ok(ref image) => {
                if let Some(bitmap) = BitmapRef::new(image.width, image.height, &image.pixels) {
                    let target_w = scaled_len(image.width, zoom);
                    let target_h = scaled_len(image.height, zoom);
                    let x = (view_w - target_w) / 2 + self.pan_x;
                    let y = (view_h - target_h) / 2 + self.pan_y;
                    let clip = DamageRect {
                        x0: 0,
                        y0: 0,
                        x1: view_w - 1,
                        y1: view_h - 1,
                    };
                    slopos_gfx::image::draw_image_clipped(
                        fb,
                        bitmap,
                        x,
                        y,
                        target_w,
                        target_h,
                        ImageFit::Stretch,
                        ImageSampling::for_resize(image.width, image.height, target_w, target_h),
                        &clip,
                    );
                }
            }
            Err(ref message) => {
                let clip = full_clip(fb);
                gfx::draw_str_clipped(fb, 16, 16, "Could not load image", ERROR_FG, BG, &clip);
                gfx::draw_str_clipped(fb, 16, 34, message, STATUS_FG, BG, &clip);
            }
        }

        self.draw_status(fb, view_h, zoom);
    }
}

pub fn image_viewer_main() {
    let path = env::args()
        .nth(1)
        .unwrap_or_else(|| String::from(DEFAULT_IMAGE));
    slopos_windowing::run(ImageViewer::new(path), 900, 640);
}

fn scaled_len(value: u32, zoom: f64) -> i32 {
    ((value as f64 * zoom).round() as i32).max(1)
}

fn full_clip(fb: &DrawBuffer<'_>) -> DamageRect {
    DamageRect {
        x0: 0,
        y0: 0,
        x1: fb.width() as i32 - 1,
        y1: fb.height() as i32 - 1,
    }
}
