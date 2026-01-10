//! Graphics context and drawing operations for the kernel framebuffer.
//!
//! Implements `DrawTarget` from `slopos_abi` to enable use of the shared
//! drawing primitives in `abi::draw_primitives`.

use crate::framebuffer::{self, FbState};
use slopos_abi::DrawTarget;
use slopos_abi::draw_primitives;
use slopos_abi::pixel::DrawPixelFormat;
use slopos_drivers::video_bridge::VideoError;

pub type GraphicsResult<T = ()> = Result<T, VideoError>;

pub struct GraphicsContext {
    fb: FbState,
}

impl GraphicsContext {
    pub fn new() -> GraphicsResult<Self> {
        snapshot().map(|fb| Self { fb })
    }

    pub fn width(&self) -> u32 {
        self.fb.width
    }

    pub fn height(&self) -> u32 {
        self.fb.height
    }
}

fn snapshot() -> GraphicsResult<FbState> {
    framebuffer::snapshot().ok_or(VideoError::NoFramebuffer)
}

fn bytes_per_pixel(bpp: u8) -> u32 {
    ((bpp as u32) + 7) / 8
}

impl DrawTarget for GraphicsContext {
    #[inline]
    fn width(&self) -> u32 {
        self.fb.width
    }

    #[inline]
    fn height(&self) -> u32 {
        self.fb.height
    }

    #[inline]
    fn pitch(&self) -> usize {
        self.fb.pitch as usize
    }

    #[inline]
    fn bytes_pp(&self) -> u8 {
        self.fb.bpp
    }

    fn pixel_format(&self) -> DrawPixelFormat {
        match self.fb.pixel_format {
            0x02 | 0x04 => DrawPixelFormat::Bgr,
            _ => DrawPixelFormat::Rgb,
        }
    }

    #[inline]
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x >= self.fb.width as i32 || y >= self.fb.height as i32 {
            return;
        }
        let bytes_pp = bytes_per_pixel(self.fb.bpp) as usize;
        let offset = y as usize * self.fb.pitch as usize + x as usize * bytes_pp;
        let pixel_ptr = unsafe { self.fb.base.add(offset) };

        unsafe {
            match bytes_pp {
                4 => (pixel_ptr as *mut u32).write_volatile(color),
                3 => {
                    pixel_ptr.write_volatile((color & 0xFF) as u8);
                    pixel_ptr.add(1).write_volatile(((color >> 8) & 0xFF) as u8);
                    pixel_ptr
                        .add(2)
                        .write_volatile(((color >> 16) & 0xFF) as u8);
                }
                2 => (pixel_ptr as *mut u16).write_volatile(color as u16),
                _ => {}
            }
        }
    }

    /// Optimized fill_rect using row-based volatile writes.
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        if w <= 0 || h <= 0 {
            return;
        }

        let mut x1 = x;
        let mut y1 = y;
        let mut x2 = x + w - 1;
        let mut y2 = y + h - 1;

        if x1 < 0 {
            x1 = 0;
        }
        if y1 < 0 {
            y1 = 0;
        }
        if x2 >= self.fb.width as i32 {
            x2 = self.fb.width as i32 - 1;
        }
        if y2 >= self.fb.height as i32 {
            y2 = self.fb.height as i32 - 1;
        }

        if x1 > x2 || y1 > y2 {
            return;
        }

        let bytes_pp = bytes_per_pixel(self.fb.bpp) as usize;
        let buffer = self.fb.base;
        let pitch = self.fb.pitch as usize;

        for row in y1..=y2 {
            let mut pixel_ptr =
                unsafe { buffer.add(row as usize * pitch + x1 as usize * bytes_pp) };
            if bytes_pp == 4 {
                let mut count = x2 - x1 + 1;
                while count > 0 {
                    unsafe {
                        (pixel_ptr as *mut u32).write_volatile(color);
                        pixel_ptr = pixel_ptr.add(bytes_pp);
                    }
                    count -= 1;
                }
            } else {
                for _ in x1..=x2 {
                    unsafe {
                        match bytes_pp {
                            2 => (pixel_ptr as *mut u16).write_volatile(color as u16),
                            3 => {
                                pixel_ptr.write_volatile((color & 0xFF) as u8);
                                pixel_ptr.add(1).write_volatile(((color >> 8) & 0xFF) as u8);
                                pixel_ptr
                                    .add(2)
                                    .write_volatile(((color >> 16) & 0xFF) as u8);
                            }
                            _ => {}
                        }
                        pixel_ptr = pixel_ptr.add(bytes_pp);
                    }
                }
            }
        }
    }
}

#[inline]
pub fn draw_pixel(ctx: &mut GraphicsContext, x: i32, y: i32, color: u32) {
    let raw = ctx.pixel_format().convert_color(color);
    ctx.draw_pixel(x, y, raw);
}

#[inline]
pub fn fill_rect(ctx: &mut GraphicsContext, x: i32, y: i32, w: i32, h: i32, color: u32) {
    draw_primitives::fill_rect(ctx, x, y, w, h, color);
}

#[inline]
pub fn draw_rect(ctx: &mut GraphicsContext, x: i32, y: i32, w: i32, h: i32, color: u32) {
    draw_primitives::rect(ctx, x, y, w, h, color);
}

#[inline]
pub fn draw_line(ctx: &mut GraphicsContext, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
    draw_primitives::line(ctx, x0, y0, x1, y1, color);
}

#[inline]
pub fn draw_circle(ctx: &mut GraphicsContext, cx: i32, cy: i32, radius: i32, color: u32) {
    draw_primitives::circle(ctx, cx, cy, radius, color);
}

#[inline]
pub fn draw_circle_filled(ctx: &mut GraphicsContext, cx: i32, cy: i32, radius: i32, color: u32) {
    draw_primitives::circle_filled(ctx, cx, cy, radius, color);
}
