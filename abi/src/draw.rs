/// Canonical color representation: 0xAARRGGBB.
///
/// This is the standard encoding used by:
/// - `abi::pixel::rgba()` and `abi::pixel::rgb()` helpers
/// - All userland theme constants
/// - The web/CSS ARGB convention
///
/// Use `PixelFormat::encode()` to convert to an `EncodedPixel` for
/// writing to a specific framebuffer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Color32(pub u32);

impl Color32 {
    /// Fully transparent black.
    pub const TRANSPARENT: Self = Self(0x00000000);
    /// Opaque black.
    pub const BLACK: Self = Self(0xFF000000);
    /// Opaque white.
    pub const WHITE: Self = Self(0xFFFFFFFF);

    /// Construct from individual RGBA components.
    #[inline]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    /// Construct an opaque color from RGB.
    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 0xFF)
    }

    /// Extract the alpha component.
    #[inline]
    pub const fn alpha(self) -> u8 {
        (self.0 >> 24) as u8
    }
    /// Extract the red component.
    #[inline]
    pub const fn red(self) -> u8 {
        (self.0 >> 16) as u8
    }
    /// Extract the green component.
    #[inline]
    pub const fn green(self) -> u8 {
        (self.0 >> 8) as u8
    }
    /// Extract the blue component.
    #[inline]
    pub const fn blue(self) -> u8 {
        self.0 as u8
    }

    /// Get the raw u32 value (0xAARRGGBB).
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

/// A color value already encoded for a specific `PixelFormat`.
///
/// Produced by `PixelFormat::encode()`. The internal representation
/// matches what the framebuffer hardware expects — write this directly
/// to pixel memory.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct EncodedPixel(pub u32);

impl EncodedPixel {
    /// Get the raw u32 value for writing to a pixel buffer.
    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

#[inline]
fn clip_row_span_bounds(
    width: u32,
    height: u32,
    row: i32,
    x0: i32,
    x1: i32,
) -> Option<(usize, usize, usize)> {
    if row < 0 || row >= height as i32 {
        return None;
    }
    let w = width as i32;
    let x0 = x0.max(0);
    let x1 = x1.min(w - 1);
    if x0 > x1 {
        return None;
    }
    Some((row as usize, x0 as usize, x1 as usize))
}

/// Unified drawing surface trait.
///
/// `Canvas` merges the responsibilities of the legacy `PixelBuffer` and
/// `DrawTarget` traits into a single trait. Implementors provide the low-level
/// byte-writing primitives; higher-level drawing operations are provided as
/// default methods that use those primitives efficiently.
///
/// Two implementations exist:
/// - **Kernel `GraphicsContext`**: volatile writes to MMIO framebuffer memory
/// - **Userland `DrawBuffer`**: safe slice writes to shared memory
///
/// Colors are passed as `EncodedPixel` values (already converted for the
/// target's pixel format via `PixelFormat::encode()`).
pub trait Canvas {
    /// Buffer width in pixels.
    fn width(&self) -> u32;

    /// Buffer height in pixels.
    fn height(&self) -> u32;

    /// Row stride in bytes.
    fn pitch_bytes(&self) -> usize;

    /// Bytes per pixel (3 or 4).
    fn bytes_per_pixel(&self) -> u8;

    /// The pixel format of this surface.
    fn pixel_format(&self) -> crate::pixel::PixelFormat;

    /// Write a single pre-encoded pixel at the given byte offset.
    ///
    /// Implementations handle volatile vs safe writes.
    /// Callers must ensure `byte_offset` is within buffer bounds.
    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel);

    #[inline]
    fn clip_row_span(&self, row: i32, x0: i32, x1: i32) -> Option<(usize, usize, usize)> {
        clip_row_span_bounds(self.width(), self.height(), row, x0, x1)
    }

    /// Fill a horizontal span with a pre-encoded pixel.
    ///
    /// Fills pixels from column `x0` to `x1` (inclusive) on `row`.
    /// Out-of-bounds coordinates are clipped.
    ///
    /// The default implementation calls `write_encoded_at` in a loop.
    /// Implementors should override this for bulk-write optimisations.
    #[inline]
    fn fill_row_span(&mut self, row: i32, x0: i32, x1: i32, pixel: EncodedPixel) {
        let Some((row, x0, x1)) = self.clip_row_span(row, x0, x1) else {
            return;
        };
        let bpp = self.bytes_per_pixel() as usize;
        let pitch = self.pitch_bytes();
        let row_start = row * pitch;
        for x in x0..=x1 {
            self.write_encoded_at(row_start + x * bpp, pixel);
        }
    }

    /// Clear the entire buffer to a single encoded pixel value.
    ///
    /// Default fills row by row via `fill_row_span`.
    #[inline]
    fn clear_canvas(&mut self, pixel: EncodedPixel) {
        let h = self.height() as i32;
        let w = self.width() as i32;
        for row in 0..h {
            self.fill_row_span(row, 0, w - 1, pixel);
        }
    }

    // -- convenience defaults built on the above primitives --

    /// Draw a single pixel (pre-encoded). Out-of-bounds silently ignored.
    #[inline]
    fn put_pixel(&mut self, x: i32, y: i32, pixel: EncodedPixel) {
        if x < 0 || y < 0 || x >= self.width() as i32 || y >= self.height() as i32 {
            return;
        }
        let off =
            (y as usize) * self.pitch_bytes() + (x as usize) * self.bytes_per_pixel() as usize;
        self.write_encoded_at(off, pixel);
    }

    /// Draw a horizontal line from `x0` to `x1` (inclusive).
    #[inline]
    fn hline(&mut self, x0: i32, x1: i32, y: i32, pixel: EncodedPixel) {
        let (x0, x1) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        self.fill_row_span(y, x0, x1, pixel);
    }

    /// Draw a vertical line from `y0` to `y1` (inclusive).
    #[inline]
    fn vline(&mut self, x: i32, y0: i32, y1: i32, pixel: EncodedPixel) {
        let (y0, y1) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        for y in y0..=y1 {
            self.put_pixel(x, y, pixel);
        }
    }

    /// Read a raw pixel value at the given byte offset.
    ///
    /// Returns the pixel as a u32 in the buffer's native encoding (typically
    /// little-endian ARGB8888). Used by alpha-blending and anti-aliased
    /// drawing operations that need to read-modify-write pixels.
    ///
    /// The default returns 0 (transparent black), which is correct for
    /// write-only surfaces like MMIO framebuffers. Buffer-backed surfaces
    /// should override this to read actual pixel data.
    #[inline]
    fn read_encoded_at(&self, _byte_offset: usize) -> u32 {
        0
    }

    /// Report that a rectangular region was modified.
    ///
    /// Drawing functions in the `gfx` crate call this automatically after
    /// rendering. The default is a no-op — appropriate for direct framebuffer
    /// surfaces where damage tracking is unnecessary. Buffer-backed surfaces
    /// (e.g. shared-memory draw buffers) override this to feed their damage
    /// tracker, eliminating the need for per-call wrapper boilerplate.
    #[inline]
    fn report_damage(&mut self, _rect: crate::damage::DamageRect) {}

    /// Fill a rectangle with a solid encoded pixel value.
    #[inline]
    fn fill_rect_encoded(&mut self, x: i32, y: i32, w: i32, h: i32, pixel: EncodedPixel) {
        if w <= 0 || h <= 0 {
            return;
        }
        let buf_w = self.width() as i32;
        let buf_h = self.height() as i32;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + w - 1).min(buf_w - 1);
        let y1 = (y + h - 1).min(buf_h - 1);
        if x0 > x1 || y0 > y1 {
            return;
        }
        for row in y0..=y1 {
            self.fill_row_span(row, x0, x1, pixel);
        }
    }
}
