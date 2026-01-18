//! Core drawing abstraction for SlopOS
//!
//! Defines the `DrawTarget` trait that abstracts over different pixel buffer
//! implementations (kernel framebuffer with volatile writes, userland shared
//! memory with safe slice operations).
//!
//! The `PixelBuffer` trait provides low-level byte access for optimized
//! implementations. Types implementing `PixelBuffer` get efficient default
//! implementations of `DrawTarget` methods.
//!
//! Inspired by embedded-graphics but tailored for SlopOS:
//! - Uses u32 colors (not generic color types)
//! - Integrates with DrawPixelFormat
//! - Includes optional damage tracking

use crate::pixel::DrawPixelFormat;

/// Low-level pixel buffer access trait.
///
/// This trait provides the primitive operations needed for pixel manipulation.
/// Implementations differ in how they write bytes:
/// - Kernel: volatile writes to MMIO framebuffer
/// - Userland: safe slice operations on shared memory
///
/// Types implementing this trait automatically get optimized `DrawTarget`
/// implementations via the blanket impl.
pub trait PixelBuffer {
    /// Get buffer width in pixels
    fn width(&self) -> u32;

    /// Get buffer height in pixels
    fn height(&self) -> u32;

    /// Get row pitch in bytes
    fn pitch(&self) -> usize;

    /// Get bytes per pixel (3 or 4)
    fn bytes_pp(&self) -> u8;

    /// Get the pixel format for color conversion
    fn pixel_format(&self) -> DrawPixelFormat;

    /// Write raw bytes at the given byte offset.
    ///
    /// # Safety contract
    /// Implementations must handle bounds checking. The offset is pre-validated
    /// by callers to be within buffer bounds, but implementations may add
    /// additional checks.
    ///
    /// For 4bpp: writes 4 bytes from color.to_le_bytes()
    /// For 3bpp: writes 3 bytes (low 3 bytes of color)
    fn write_pixel_at_offset(&mut self, byte_offset: usize, color: u32);

    /// Fill a row span with a color (optimized path).
    ///
    /// Fills pixels from x0 to x1 (inclusive) on the given row.
    /// Default implementation calls write_pixel_at_offset in a loop.
    /// Implementations can override for better performance (e.g., memset-like).
    #[inline]
    fn fill_row_span(&mut self, row: i32, x0: i32, x1: i32, color: u32) {
        if row < 0 || row >= self.height() as i32 {
            return;
        }
        let w = self.width() as i32;
        let x0 = x0.max(0);
        let x1 = x1.min(w - 1);
        if x0 > x1 {
            return;
        }

        let bytes_pp = self.bytes_pp() as usize;
        let pitch = self.pitch();
        let row_start = (row as usize) * pitch;

        for x in x0..=x1 {
            let offset = row_start + (x as usize) * bytes_pp;
            self.write_pixel_at_offset(offset, color);
        }
    }

    /// Clear the entire buffer with a color (optimized path).
    ///
    /// Default implementation fills row by row.
    /// Implementations can override for bulk operations.
    #[inline]
    fn clear_buffer(&mut self, color: u32) {
        let h = self.height() as i32;
        let w = self.width() as i32;
        for row in 0..h {
            self.fill_row_span(row, 0, w - 1, color);
        }
    }
}

/// Core trait for any drawable surface.
///
/// Implementations exist for:
/// - Kernel GraphicsContext (volatile MMIO writes)
/// - Userland DrawBuffer (safe slice writes)
///
/// Only `draw_pixel` is required. All other methods have default implementations
/// that use `draw_pixel`, but can be overridden for performance.
///
/// Colors are passed as pre-converted u32 values (already in the target's
/// pixel format). Use `pixel_format().convert_color(rgba)` before drawing.
pub trait DrawTarget {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn pitch(&self) -> usize;
    fn bytes_pp(&self) -> u8;
    fn pixel_format(&self) -> DrawPixelFormat;

    /// Draw a single pixel with a pre-converted color value.
    /// Out-of-bounds coordinates should be silently ignored (clipped).
    fn draw_pixel(&mut self, x: i32, y: i32, color: u32);

    /// Draw a horizontal line (x0 to x1 inclusive).
    #[inline]
    fn draw_hline(&mut self, x0: i32, x1: i32, y: i32, color: u32) {
        let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        for x in x0..=x1 {
            self.draw_pixel(x, y, color);
        }
    }

    /// Draw a vertical line (y0 to y1 inclusive).
    #[inline]
    fn draw_vline(&mut self, x: i32, y0: i32, y1: i32, color: u32) {
        let (y0, y1) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        for y in y0..=y1 {
            self.draw_pixel(x, y, color);
        }
    }

    /// Fill a rectangle with a solid color.
    #[inline]
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u32) {
        if w <= 0 || h <= 0 {
            return;
        }
        for row in y..(y + h) {
            self.draw_hline(x, x + w - 1, row, color);
        }
    }

    /// Clear the entire surface with a color.
    #[inline]
    fn clear(&mut self, color: u32) {
        let w = self.width() as i32;
        let h = self.height() as i32;
        self.fill_rect(0, 0, w, h, color);
    }
}

/// Helper functions for pixel buffer operations.
///
/// These functions implement the common pixel writing logic that was previously
/// duplicated between kernel GraphicsContext and userland DrawBuffer.
pub mod pixel_ops {
    use super::PixelBuffer;

    /// Calculate byte offset for a pixel coordinate.
    #[inline]
    pub fn pixel_offset(pitch: usize, bytes_pp: usize, x: i32, y: i32) -> usize {
        (y as usize) * pitch + (x as usize) * bytes_pp
    }

    /// Check if coordinates are within bounds.
    #[inline]
    pub fn in_bounds(x: i32, y: i32, width: u32, height: u32) -> bool {
        x >= 0 && y >= 0 && x < width as i32 && y < height as i32
    }

    /// Clip a rectangle to buffer bounds and return (x0, y0, x1, y1).
    /// Returns None if the rectangle is entirely outside bounds.
    #[inline]
    pub fn clip_rect(
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        buf_w: u32,
        buf_h: u32,
    ) -> Option<(i32, i32, i32, i32)> {
        if w <= 0 || h <= 0 {
            return None;
        }

        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w - 1).min(buf_w as i32 - 1);
        let y1 = (y + h - 1).min(buf_h as i32 - 1);

        if x0 > x1 || y0 > y1 {
            None
        } else {
            Some((x0, y0, x1, y1))
        }
    }

    /// Generic draw_pixel implementation for PixelBuffer types.
    #[inline]
    pub fn draw_pixel_impl<P: PixelBuffer + ?Sized>(buf: &mut P, x: i32, y: i32, color: u32) {
        if !in_bounds(x, y, buf.width(), buf.height()) {
            return;
        }
        let offset = pixel_offset(buf.pitch(), buf.bytes_pp() as usize, x, y);
        buf.write_pixel_at_offset(offset, color);
    }

    /// Generic fill_rect implementation for PixelBuffer types.
    #[inline]
    pub fn fill_rect_impl<P: PixelBuffer + ?Sized>(
        buf: &mut P,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        color: u32,
    ) {
        let Some((x0, y0, x1, y1)) = clip_rect(x, y, w, h, buf.width(), buf.height()) else {
            return;
        };

        for row in y0..=y1 {
            buf.fill_row_span(row, x0, x1, color);
        }
    }

    /// Generic clear implementation for PixelBuffer types.
    #[inline]
    pub fn clear_impl<P: PixelBuffer + ?Sized>(buf: &mut P, color: u32) {
        buf.clear_buffer(color);
    }
}

/// Extension trait for DrawTarget with damage tracking.
/// Not all DrawTargets need damage tracking (e.g., kernel panic screen).
pub trait DamageTracking: DrawTarget {
    fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32);
    fn clear_damage(&mut self);
    fn is_dirty(&self) -> bool;
}
