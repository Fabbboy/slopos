use core::ffi::c_int;

use crate::framebuffer::{self, FbState};
use slopos_abi::draw::{Canvas, EncodedPixel};
use slopos_abi::video_traits::VideoError;
use slopos_ostd::boot::handoff::framebuffer::{
    fb_fill_u8_bulk, fb_ptr_add, fb_write_u8_at, fb_write_u16_at, fb_write_u32_at, fb_write_u64_at,
};
use slopos_ostd::util::ptr_buf::ptr_add;

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

    /// Flush the framebuffer to the display backend.
    ///
    /// Invokes the registered flush callback (e.g. the Xe driver's scanout
    /// trigger). Returns 0 on success or if no callback is registered.
    pub fn flush(&self) -> c_int {
        framebuffer::framebuffer_flush()
    }
}

fn snapshot() -> GraphicsResult<FbState> {
    framebuffer::snapshot().ok_or(VideoError::NoFramebuffer)
}

impl Canvas for GraphicsContext {
    #[inline]
    fn width(&self) -> u32 {
        self.fb.width()
    }

    #[inline]
    fn height(&self) -> u32 {
        self.fb.height()
    }

    #[inline]
    fn pitch_bytes(&self) -> usize {
        self.fb.pitch() as usize
    }

    #[inline]
    fn bytes_per_pixel(&self) -> u8 {
        self.fb.info.bytes_per_pixel()
    }

    #[inline]
    fn pixel_format(&self) -> slopos_abi::pixel::PixelFormat {
        self.fb.info.format
    }

    #[inline]
    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel) {
        let color = pixel.to_u32();
        let pixel_ptr = fb_ptr_add(self.fb.base_ptr(), byte_offset);
        let bytes_pp = self.fb.info.bytes_per_pixel();
        match bytes_pp {
            4 => fb_write_u32_at(pixel_ptr, color),
            3 => {
                fb_write_u8_at(pixel_ptr, (color & 0xFF) as u8);
                fb_write_u8_at(fb_ptr_add(pixel_ptr, 1), ((color >> 8) & 0xFF) as u8);
                fb_write_u8_at(fb_ptr_add(pixel_ptr, 2), ((color >> 16) & 0xFF) as u8);
            }
            2 => fb_write_u16_at(pixel_ptr, color as u16),
            _ => {}
        }
    }

    #[inline]
    fn fill_row_span(&mut self, row: i32, x0: i32, x1: i32, pixel: EncodedPixel) {
        let Some((row, x0, x1)) = self.clip_row_span(row, x0, x1) else {
            return;
        };

        let color = pixel.to_u32();
        let bytes_pp = self.fb.info.bytes_per_pixel() as usize;
        let pitch = self.fb.pitch() as usize;
        let buffer = self.fb.base_ptr();
        let pixel_ptr = fb_ptr_add(buffer, row * pitch + x0 * bytes_pp);
        let pixel_count = x1 - x0 + 1;

        if bytes_pp == 4 {
            let b0 = (color & 0xFF) as u8;
            let b1 = ((color >> 8) & 0xFF) as u8;
            let b2 = ((color >> 16) & 0xFF) as u8;
            let b3 = ((color >> 24) & 0xFF) as u8;

            if b0 == b1 && b1 == b2 && b2 == b3 {
                fb_fill_u8_bulk(pixel_ptr, b0, pixel_count * 4);
            } else {
                let color64 = (color as u64) | ((color as u64) << 32);
                let mut ptr = pixel_ptr;
                let mut remaining = pixel_count;

                if remaining > 0 && ((ptr as usize) & (core::mem::align_of::<u64>() - 1)) != 0 {
                    fb_write_u32_at(ptr, color);
                    ptr = fb_ptr_add(ptr, 4);
                    remaining -= 1;
                }

                let pairs = remaining / 2;
                let remainder = remaining % 2;
                let mut ptr64 = ptr as *mut u64;

                for _ in 0..pairs {
                    fb_write_u64_at(ptr64, color64);
                    ptr64 = ptr_add(ptr64, 1);
                }
                if remainder > 0 {
                    fb_write_u32_at(ptr64 as *mut u8, color);
                }
            }
        } else {
            let mut ptr = pixel_ptr;
            for _ in 0..pixel_count {
                match bytes_pp {
                    2 => fb_write_u16_at(ptr, color as u16),
                    3 => {
                        fb_write_u8_at(ptr, (color & 0xFF) as u8);
                        fb_write_u8_at(fb_ptr_add(ptr, 1), ((color >> 8) & 0xFF) as u8);
                        fb_write_u8_at(fb_ptr_add(ptr, 2), ((color >> 16) & 0xFF) as u8);
                    }
                    _ => {}
                }
                ptr = fb_ptr_add(ptr, bytes_pp);
            }
        }
    }
}
