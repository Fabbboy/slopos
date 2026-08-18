//! Render surface abstraction — decouples rendering from the windowing protocol.
//!
//! Implementations include compositor-backed SHM surfaces and
//! [`HeadlessSurface`] for testing without a display server.

use crate::DrawBuffer;
use slopos_abi::pixel::PixelFormat;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// Zero width/height, or a pitch that overflows `usize`.
    BadSize,
    BufferUnavailable,
}

/// A rendering target that hands out CPU pixel buffers and presents frames.
pub trait RenderSurface {
    /// Borrow a [`DrawBuffer`] for the current frame.
    ///
    /// The caller must drop it before calling [`present()`](Self::present).
    fn frame(&mut self) -> Option<DrawBuffer<'_>>;

    /// Present the completed frame, damaging the whole surface.
    fn present(&mut self);

    fn present_region(&mut self, x: i32, y: i32, w: i32, h: i32);

    /// Resize the backing buffer, invalidating any previously obtained `DrawBuffer`.
    fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError>;

    fn width(&self) -> u32;

    fn height(&self) -> u32;

    fn pixel_format(&self) -> PixelFormat;

    /// Bytes per pixel (3 or 4).
    fn bytes_pp(&self) -> u8;

    /// Row stride in bytes.
    fn pitch(&self) -> usize;
}

#[cfg(feature = "alloc")]
extern crate alloc;

/// A render surface backed by a heap-allocated buffer, with no compositor.
///
/// `present()` and `present_region()` are no-ops.
#[cfg(feature = "alloc")]
pub struct HeadlessSurface {
    data: alloc::vec::Vec<u8>,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
}

#[cfg(feature = "alloc")]
impl HeadlessSurface {
    pub fn new(width: u32, height: u32, pixel_format: PixelFormat) -> Result<Self, RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::BadSize);
        }
        let bytes_pp = pixel_format.bytes_per_pixel();
        let pitch = (width as usize)
            .checked_mul(bytes_pp as usize)
            .ok_or(RenderError::BadSize)?;
        let buffer_size = pitch
            .checked_mul(height as usize)
            .ok_or(RenderError::BadSize)?;

        let data = alloc::vec![0u8; buffer_size];
        Ok(Self {
            data,
            width,
            height,
            pitch,
            bytes_pp,
            pixel_format,
        })
    }

    /// Decoded pixel at `(x, y)`, or `None` if out of bounds.
    pub fn pixel_at(&self, x: u32, y: u32) -> Option<slopos_abi::draw::Color32> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let offset = (y as usize) * self.pitch + (x as usize) * (self.bytes_pp as usize);
        let raw = match self.bytes_pp {
            4 => {
                let b = self.data[offset] as u32;
                let g = self.data[offset + 1] as u32;
                let r = self.data[offset + 2] as u32;
                let a = self.data[offset + 3] as u32;
                (a << 24) | (r << 16) | (g << 8) | b
            }
            3 => {
                let b = self.data[offset] as u32;
                let g = self.data[offset + 1] as u32;
                let r = self.data[offset + 2] as u32;
                (0xFF << 24) | (r << 16) | (g << 8) | b
            }
            _ => return None,
        };
        Some(self.pixel_format.decode(raw))
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[cfg(feature = "alloc")]
impl RenderSurface for HeadlessSurface {
    fn frame(&mut self) -> Option<DrawBuffer<'_>> {
        let mut buf = DrawBuffer::new(
            &mut self.data,
            self.width,
            self.height,
            self.pitch,
            self.bytes_pp,
        )?;
        buf.set_pixel_format(self.pixel_format);
        Some(buf)
    }

    fn present(&mut self) {}

    fn present_region(&mut self, _x: i32, _y: i32, _w: i32, _h: i32) {}

    fn resize(&mut self, width: u32, height: u32) -> Result<(), RenderError> {
        if width == 0 || height == 0 {
            return Err(RenderError::BadSize);
        }
        let bytes_pp = self.bytes_pp;
        let pitch = (width as usize)
            .checked_mul(bytes_pp as usize)
            .ok_or(RenderError::BadSize)?;
        let buffer_size = pitch
            .checked_mul(height as usize)
            .ok_or(RenderError::BadSize)?;

        self.data.resize(buffer_size, 0);
        self.data.fill(0);
        self.width = width;
        self.height = height;
        self.pitch = pitch;
        Ok(())
    }

    fn width(&self) -> u32 {
        self.width
    }

    fn height(&self) -> u32 {
        self.height
    }

    fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    fn pitch(&self) -> usize {
        self.pitch
    }
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use super::*;
    use slopos_abi::draw::{Canvas, Color32};

    #[test]
    fn headless_create_and_frame() {
        let mut surface =
            HeadlessSurface::new(64, 48, PixelFormat::Argb8888).expect("create failed");
        assert_eq!(surface.width(), 64);
        assert_eq!(surface.height(), 48);
        assert_eq!(surface.bytes_pp(), 4);
        assert_eq!(surface.pitch(), 256);

        let fb = surface.frame().expect("frame failed");
        assert_eq!(fb.width(), 64);
        assert_eq!(fb.height(), 48);
    }

    #[test]
    fn headless_write_and_read_pixel() {
        let mut surface = HeadlessSurface::new(8, 8, PixelFormat::Argb8888).expect("create failed");
        {
            let mut fb = surface.frame().expect("frame failed");
            let red = fb.pixel_format().encode(Color32::new(255, 0, 0, 255));
            fb.put_pixel(3, 5, red);
        }
        let pixel = surface.pixel_at(3, 5).expect("out of bounds");
        assert_eq!(pixel.red(), 255);
        assert_eq!(pixel.green(), 0);
        assert_eq!(pixel.blue(), 0);
    }

    #[test]
    fn headless_resize() {
        let mut surface =
            HeadlessSurface::new(10, 10, PixelFormat::Argb8888).expect("create failed");
        surface.resize(20, 15).expect("resize failed");
        assert_eq!(surface.width(), 20);
        assert_eq!(surface.height(), 15);
        assert_eq!(surface.pitch(), 80);

        let fb = surface.frame().expect("frame after resize");
        assert_eq!(fb.width(), 20);
        assert_eq!(fb.height(), 15);
    }

    #[test]
    fn headless_reject_zero_size() {
        assert!(HeadlessSurface::new(0, 10, PixelFormat::Argb8888).is_err());
        assert!(HeadlessSurface::new(10, 0, PixelFormat::Argb8888).is_err());

        let mut surface =
            HeadlessSurface::new(10, 10, PixelFormat::Argb8888).expect("create failed");
        assert!(surface.resize(0, 5).is_err());
        assert!(surface.resize(5, 0).is_err());
    }

    #[test]
    fn headless_present_does_not_panic() {
        let mut surface = HeadlessSurface::new(4, 4, PixelFormat::Argb8888).expect("create failed");
        surface.present();
        surface.present_region(0, 0, 2, 2);
    }

    #[test]
    fn headless_pixel_at_out_of_bounds() {
        let surface = HeadlessSurface::new(4, 4, PixelFormat::Argb8888).expect("create failed");
        assert!(surface.pixel_at(4, 0).is_none());
        assert!(surface.pixel_at(0, 4).is_none());
        assert!(surface.pixel_at(3, 3).is_some());
    }
}
