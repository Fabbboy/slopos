//! Core drawing abstraction for SlopOS
//!
//! Defines the `DrawTarget` trait that abstracts over different pixel buffer
//! implementations (kernel framebuffer with volatile writes, userland shared
//! memory with safe slice operations).
//!
//! Inspired by embedded-graphics but tailored for SlopOS:
//! - Uses u32 colors (not generic color types)
//! - Integrates with DrawPixelFormat
//! - Includes optional damage tracking

use crate::pixel::DrawPixelFormat;

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

/// Extension trait for DrawTarget with damage tracking.
/// Not all DrawTargets need damage tracking (e.g., kernel panic screen).
pub trait DamageTracking: DrawTarget {
    fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32);
    fn clear_damage(&mut self);
    fn is_dirty(&self) -> bool;
}
