use crate::framebuffer::{self, FbState};
use slopos_abi::draw_primitives;
use slopos_abi::pixel::DrawPixelFormat;
use slopos_abi::video_traits::VideoError;
use slopos_abi::{pixel_ops, DrawTarget, PixelBuffer};

pub type GraphicsResult<T = ()> = Result<T, VideoError>;

pub struct GraphicsContext {
    fb: FbState,
}

impl GraphicsContext {
    pub fn new() -> GraphicsResult<Self> {
        snapshot().map(|fb| Self { fb })
    }

    pub fn width(&self) -> u32 {
        self.fb.width()
    }

    pub fn height(&self) -> u32 {
        self.fb.height()
    }
}

fn snapshot() -> GraphicsResult<FbState> {
    framebuffer::snapshot().ok_or(VideoError::NoFramebuffer)
}

impl PixelBuffer for GraphicsContext {
    #[inline]
    fn width(&self) -> u32 {
        self.fb.width()
    }

    #[inline]
    fn height(&self) -> u32 {
        self.fb.height()
    }

    #[inline]
    fn pitch(&self) -> usize {
        self.fb.pitch() as usize
    }

    #[inline]
    fn bytes_pp(&self) -> u8 {
        self.fb.info.bytes_per_pixel()
    }

    #[inline]
    fn pixel_format(&self) -> DrawPixelFormat {
        DrawPixelFormat::from_pixel_format(self.fb.info.format)
    }

    #[inline]
    fn write_pixel_at_offset(&mut self, byte_offset: usize, color: u32) {
        let pixel_ptr = unsafe { self.fb.base_ptr().add(byte_offset) };
        let bytes_pp = self.fb.info.bytes_per_pixel();

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

    #[inline]
    fn fill_row_span(&mut self, row: i32, x0: i32, x1: i32, color: u32) {
        if row < 0 || row >= self.fb.height() as i32 {
            return;
        }
        let w = self.fb.width() as i32;
        let x0 = x0.max(0);
        let x1 = x1.min(w - 1);
        if x0 > x1 {
            return;
        }

        let bytes_pp = self.fb.info.bytes_per_pixel() as usize;
        let pitch = self.fb.pitch() as usize;
        let buffer = self.fb.base_ptr();
        let pixel_ptr = unsafe { buffer.add(row as usize * pitch + x0 as usize * bytes_pp) };
        let pixel_count = (x1 - x0 + 1) as usize;

        if bytes_pp == 4 {
            // Fast path: check if all bytes of color are the same (e.g., 0x00000000 or 0xFFFFFFFF)
            let b0 = (color & 0xFF) as u8;
            let b1 = ((color >> 8) & 0xFF) as u8;
            let b2 = ((color >> 16) & 0xFF) as u8;
            let b3 = ((color >> 24) & 0xFF) as u8;

            if b0 == b1 && b1 == b2 && b2 == b3 {
                // All bytes identical - use fast bulk write (common for black/white)
                unsafe {
                    core::ptr::write_bytes(pixel_ptr, b0, pixel_count * 4);
                }
            } else {
                // Use 64-bit writes for better throughput (2 pixels at a time)
                let color64 = (color as u64) | ((color as u64) << 32);
                let pairs = pixel_count / 2;
                let remainder = pixel_count % 2;

                unsafe {
                    let mut ptr64 = pixel_ptr as *mut u64;
                    for _ in 0..pairs {
                        ptr64.write_volatile(color64);
                        ptr64 = ptr64.add(1);
                    }
                    if remainder > 0 {
                        (ptr64 as *mut u32).write_volatile(color);
                    }
                }
            }
        } else {
            // Fallback for 2bpp/3bpp - per-pixel writes
            let mut ptr = pixel_ptr;
            for _ in x0..=x1 {
                unsafe {
                    match bytes_pp {
                        2 => (ptr as *mut u16).write_volatile(color as u16),
                        3 => {
                            ptr.write_volatile((color & 0xFF) as u8);
                            ptr.add(1).write_volatile(((color >> 8) & 0xFF) as u8);
                            ptr.add(2).write_volatile(((color >> 16) & 0xFF) as u8);
                        }
                        _ => {}
                    }
                    ptr = ptr.add(bytes_pp);
                }
            }
        }
    }
}

impl DrawTarget for GraphicsContext {
    #[inline]
    fn width(&self) -> u32 {
        PixelBuffer::width(self)
    }

    #[inline]
    fn height(&self) -> u32 {
        PixelBuffer::height(self)
    }

    #[inline]
    fn pitch(&self) -> usize {
        PixelBuffer::pitch(self)
    }

    #[inline]
    fn bytes_pp(&self) -> u8 {
        PixelBuffer::bytes_pp(self)
    }

    #[inline]
    fn pixel_format(&self) -> DrawPixelFormat {
        PixelBuffer::pixel_format(self)
    }

    #[inline]
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32) {
        pixel_ops::draw_pixel_impl(self, x, y, color);
    }

    #[inline]
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        pixel_ops::fill_rect_impl(self, x, y, w, h, color);
    }

    #[inline]
    fn clear(&mut self, color: u32) {
        pixel_ops::clear_impl(self, color);
    }
}

#[inline]
pub fn draw_pixel(ctx: &mut GraphicsContext, x: i32, y: i32, color: u32) {
    let raw = PixelBuffer::pixel_format(ctx).convert_color(color);
    DrawTarget::draw_pixel(ctx, x, y, raw);
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
