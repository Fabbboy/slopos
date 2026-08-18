/// Canonical color representation: 0xAARRGGBB (the web/CSS ARGB convention).
/// `PixelFormat::encode` converts one into an `EncodedPixel` for a framebuffer.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct Color32(pub u32);

impl Color32 {
    pub const TRANSPARENT: Self = Self(0x00000000);
    pub const BLACK: Self = Self(0xFF000000);
    pub const WHITE: Self = Self(0xFFFFFFFF);

    #[inline]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self(((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32))
    }

    /// Construct an opaque color from RGB.
    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r, g, b, 0xFF)
    }

    #[inline]
    pub const fn alpha(self) -> u8 {
        (self.0 >> 24) as u8
    }
    #[inline]
    pub const fn red(self) -> u8 {
        (self.0 >> 16) as u8
    }
    #[inline]
    pub const fn green(self) -> u8 {
        (self.0 >> 8) as u8
    }
    #[inline]
    pub const fn blue(self) -> u8 {
        self.0 as u8
    }

    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }
}

/// A color value already encoded for a specific `PixelFormat` — the exact
/// representation the framebuffer expects; write it straight to pixel memory.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub struct EncodedPixel(pub u32);

impl EncodedPixel {
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

/// Unified drawing surface trait. Implementors provide the byte-writing
/// primitives; the higher-level operations are default methods built on them.
/// Colors arrive as `EncodedPixel`, already converted for the target format.
pub trait Canvas {
    fn width(&self) -> u32;

    fn height(&self) -> u32;

    fn pitch_bytes(&self) -> usize;

    /// Bytes per pixel (3 or 4).
    fn bytes_per_pixel(&self) -> u8;

    fn pixel_format(&self) -> crate::pixel::PixelFormat;

    /// Write a pre-encoded pixel at `byte_offset`; the caller must ensure it is
    /// within the buffer.
    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel);

    /// The active scissor (clip) rectangle in buffer-local pixel coordinates.
    /// When `Some`, every primitive confines its writes to it as well as to the
    /// buffer bounds. `None` suits surfaces that always paint their full target.
    #[inline]
    fn scissor(&self) -> Option<crate::damage::DamageRect> {
        None
    }

    #[inline]
    fn clip_row_span(&self, row: i32, x0: i32, x1: i32) -> Option<(usize, usize, usize)> {
        clip_row_span_bounds(self.width(), self.height(), row, x0, x1)
    }

    /// Fills columns `x0..=x1` on `row`, clipping out-of-bounds coordinates.
    /// Implementors should override the per-pixel default for bulk writes.
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

    #[inline]
    fn clear_canvas(&mut self, pixel: EncodedPixel) {
        let h = self.height() as i32;
        let w = self.width() as i32;
        for row in 0..h {
            self.fill_row_span(row, 0, w - 1, pixel);
        }
    }

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

    /// Reads the raw pixel at `byte_offset` in the buffer's native encoding.
    /// Defaults to 0, correct for write-only surfaces like MMIO framebuffers.
    #[inline]
    fn read_encoded_at(&self, _byte_offset: usize) -> u32 {
        0
    }

    /// Report that a rectangular region was modified. A no-op by default, which
    /// suits direct framebuffers; buffer-backed surfaces feed a damage tracker.
    #[inline]
    fn report_damage(&mut self, _rect: crate::damage::DamageRect) {}

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
