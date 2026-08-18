use slopos_abi::damage;
use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;

use crate::DamageTracker;

/// A heap-free [`Canvas`] over a caller-supplied `&mut [u8]`, with
/// bounds-checked writes and damage tracking.
pub struct DrawBuffer<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
    damage: DamageTracker,
    /// Active scissor (clip) region. When `Some`, every primitive confines its
    /// writes to this rect, so a partial-region repaint cannot paint outside
    /// the region currently being composited (e.g. a window's title text
    /// spilling over a window stacked above it).
    scissor: Option<damage::DamageRect>,
}

impl<'a> DrawBuffer<'a> {
    pub fn new(
        data: &'a mut [u8],
        width: u32,
        height: u32,
        pitch: usize,
        bytes_pp: u8,
    ) -> Option<Self> {
        let required_size = pitch * (height as usize);
        if data.len() < required_size {
            return None;
        }
        if bytes_pp != 3 && bytes_pp != 4 {
            return None;
        }

        Some(Self {
            data,
            width,
            height,
            pitch,
            bytes_pp,
            pixel_format: if bytes_pp == 4 {
                PixelFormat::Argb8888
            } else {
                PixelFormat::Rgb888
            },
            damage: DamageTracker::new(),
            scissor: None,
        })
    }

    pub fn set_pixel_format(&mut self, format: PixelFormat) {
        self.pixel_format = format;
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pitch(&self) -> usize {
        self.pitch
    }

    pub fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    pub fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    pub fn data(&self) -> &[u8] {
        self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        self.data
    }

    pub fn damage(&self) -> &DamageTracker {
        &self.damage
    }

    pub fn damage_mut(&mut self) -> &mut DamageTracker {
        &mut self.damage
    }

    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }

    /// Set (or clear with `None`) the scissor region. While set, every drawing
    /// primitive — solid fills, glyph blits, anti-aliased lines and circles —
    /// confines its writes to this rect, so partial-region repaints cannot
    /// over-paint pixels outside the region being composited.
    #[inline]
    pub fn set_scissor(&mut self, scissor: Option<damage::DamageRect>) {
        self.scissor = scissor;
    }

    /// Run `f` with the scissor narrowed to `clip` (intersected with any
    /// scissor already set), restoring the previous scissor afterwards.
    pub fn with_scissor<F: FnOnce(&mut Self)>(&mut self, clip: damage::DamageRect, f: F) {
        let prev = self.scissor;
        let next = match prev {
            Some(p) => damage::DamageRect {
                x0: p.x0.max(clip.x0),
                y0: p.y0.max(clip.y0),
                x1: p.x1.min(clip.x1),
                y1: p.y1.min(clip.y1),
            },
            None => clip,
        };
        self.scissor = Some(next);
        f(self);
        self.scissor = prev;
    }

    pub fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let x0 = x0.max(0);
        let y0 = y0.max(0);
        let x1 = x1.min(self.width as i32 - 1);
        let y1 = y1.min(self.height as i32 - 1);

        if x0 <= x1 && y0 <= y1 {
            self.damage.add_rect(x0, y0, x1, y1);
        }
    }

    /// Copy a rectangular region within the same buffer (handles overlap).
    pub fn blit(
        &mut self,
        src_x: i32,
        src_y: i32,
        dst_x: i32,
        dst_y: i32,
        width: i32,
        height: i32,
    ) {
        if width <= 0 || height <= 0 {
            return;
        }

        let buf_width = self.width as i32;
        let buf_height = self.height as i32;
        let bytes_pp = self.bytes_pp as usize;
        let pitch = self.pitch;

        let src_x0 = src_x.max(0);
        let src_y0 = src_y.max(0);
        let src_x1 = (src_x + width - 1).min(buf_width - 1);
        let src_y1 = (src_y + height - 1).min(buf_height - 1);

        if src_x0 > src_x1 || src_y0 > src_y1 {
            return;
        }

        let actual_width = (src_x1 - src_x0 + 1) as usize;
        let actual_height = (src_y1 - src_y0 + 1) as usize;

        let dst_x0 = dst_x.max(0);
        let dst_y0 = dst_y.max(0);
        let dst_x1 = (dst_x + actual_width as i32 - 1).min(buf_width - 1);
        let dst_y1 = (dst_y + actual_height as i32 - 1).min(buf_height - 1);

        if dst_x0 > dst_x1 || dst_y0 > dst_y1 {
            return;
        }

        let copy_width = ((dst_x1 - dst_x0 + 1) as usize).min(actual_width);
        let copy_height = ((dst_y1 - dst_y0 + 1) as usize).min(actual_height);
        let row_bytes = copy_width * bytes_pp;

        if dst_y0 < src_y0 || (dst_y0 == src_y0 && dst_x0 < src_x0) {
            for row in 0..copy_height {
                let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
                let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
                self.data.copy_within(src_off..src_off + row_bytes, dst_off);
            }
        } else {
            for row in (0..copy_height).rev() {
                let src_off = ((src_y0 as usize + row) * pitch) + (src_x0 as usize * bytes_pp);
                let dst_off = ((dst_y0 as usize + row) * pitch) + (dst_x0 as usize * bytes_pp);
                self.data.copy_within(src_off..src_off + row_bytes, dst_off);
            }
        }

        self.add_damage(dst_x0, dst_y0, dst_x1, dst_y1);
    }

    /// Scroll contents upward by `pixels` rows, filling the vacated bottom
    /// region with `fill_color`.
    pub fn scroll_up(&mut self, pixels: i32, fill_color: Color32) {
        if pixels <= 0 {
            return;
        }

        let height = self.height as i32;
        let width = self.width as i32;

        if pixels >= height {
            let px = self.pixel_format.encode(fill_color);
            self.clear_canvas(px);
            self.add_damage(0, 0, width - 1, height - 1);
            return;
        }

        self.blit(0, pixels, 0, 0, width, height - pixels);
        crate::canvas_ops::fill_rect(self, 0, height - pixels, width, pixels, fill_color);
    }

    /// Scroll contents downward by `pixels` rows, filling the vacated top
    /// region with `fill_color`.
    pub fn scroll_down(&mut self, pixels: i32, fill_color: Color32) {
        if pixels <= 0 {
            return;
        }

        let height = self.height as i32;
        let width = self.width as i32;

        if pixels >= height {
            let px = self.pixel_format.encode(fill_color);
            self.clear_canvas(px);
            self.add_damage(0, 0, width - 1, height - 1);
            return;
        }

        self.blit(0, 0, 0, pixels, width, height - pixels);
        crate::canvas_ops::fill_rect(self, 0, 0, width, pixels, fill_color);
    }
}

impl Canvas for DrawBuffer<'_> {
    #[inline]
    fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    fn pitch_bytes(&self) -> usize {
        self.pitch
    }

    #[inline]
    fn bytes_per_pixel(&self) -> u8 {
        self.bytes_pp
    }

    #[inline]
    fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    #[inline]
    fn scissor(&self) -> Option<damage::DamageRect> {
        self.scissor
    }

    /// Clip a horizontal span to the buffer bounds and the active scissor.
    /// All span-fill primitives (`fill_row_span`, `fill_rect_encoded`,
    /// `hline`, filled circles/rounded-rect interiors) route through here, so
    /// the scissor confines every one of them.
    #[inline]
    fn clip_row_span(&self, row: i32, x0: i32, x1: i32) -> Option<(usize, usize, usize)> {
        if row < 0 || row >= self.height as i32 {
            return None;
        }
        let mut x0 = x0.max(0);
        let mut x1 = x1.min(self.width as i32 - 1);
        if let Some(s) = self.scissor {
            if row < s.y0 || row > s.y1 {
                return None;
            }
            x0 = x0.max(s.x0);
            x1 = x1.min(s.x1);
        }
        if x0 > x1 {
            return None;
        }
        Some((row as usize, x0 as usize, x1 as usize))
    }

    /// Write a single opaque pixel, honoring the active scissor. Glyph blits,
    /// circle outlines and `vline` route through here, so the scissor confines
    /// text and shape edges as well as solid spans.
    #[inline]
    fn put_pixel(&mut self, x: i32, y: i32, pixel: EncodedPixel) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        if let Some(s) = self.scissor {
            if !s.contains(x, y) {
                return;
            }
        }
        let off = (y as usize) * self.pitch + (x as usize) * self.bytes_pp as usize;
        self.write_encoded_at(off, pixel);
    }

    #[inline]
    fn read_encoded_at(&self, byte_offset: usize) -> u32 {
        match self.bytes_pp {
            4 => {
                if byte_offset + 4 <= self.data.len() {
                    u32::from_le_bytes([
                        self.data[byte_offset],
                        self.data[byte_offset + 1],
                        self.data[byte_offset + 2],
                        self.data[byte_offset + 3],
                    ])
                } else {
                    0
                }
            }
            3 => {
                if byte_offset + 3 <= self.data.len() {
                    u32::from_le_bytes([
                        self.data[byte_offset],
                        self.data[byte_offset + 1],
                        self.data[byte_offset + 2],
                        0xFF,
                    ])
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    #[inline]
    fn write_encoded_at(&mut self, byte_offset: usize, pixel: EncodedPixel) {
        let color = pixel.to_u32();
        let bytes = color.to_le_bytes();
        match self.bytes_pp {
            4 => {
                if byte_offset + 4 <= self.data.len() {
                    self.data[byte_offset..byte_offset + 4].copy_from_slice(&bytes);
                }
            }
            3 => {
                if byte_offset + 3 <= self.data.len() {
                    self.data[byte_offset] = bytes[0];
                    self.data[byte_offset + 1] = bytes[1];
                    self.data[byte_offset + 2] = bytes[2];
                }
            }
            _ => {}
        }
    }

    #[inline]
    fn fill_row_span(&mut self, row: i32, x0: i32, x1: i32, pixel: EncodedPixel) {
        let Some((row, x0, x1)) = self.clip_row_span(row, x0, x1) else {
            return;
        };

        let color = pixel.to_u32();
        let bytes_pp = self.bytes_pp as usize;
        let pitch = self.pitch;
        let span_w = x1 - x0 + 1;
        let row_off = row * pitch + x0 * bytes_pp;

        match bytes_pp {
            4 => {
                let end = row_off + span_w * 4;
                if end <= self.data.len() {
                    let row_slice = &mut self.data[row_off..end];
                    if color == 0 {
                        row_slice.fill(0);
                    } else {
                        let bytes = color.to_le_bytes();
                        for chunk in row_slice.chunks_exact_mut(4) {
                            chunk.copy_from_slice(&bytes);
                        }
                    }
                }
            }
            3 => {
                let bytes = color.to_le_bytes();
                for col in 0..span_w {
                    let off = row_off + col * 3;
                    if off + 3 <= self.data.len() {
                        self.data[off] = bytes[0];
                        self.data[off + 1] = bytes[1];
                        self.data[off + 2] = bytes[2];
                    }
                }
            }
            _ => {}
        }
    }

    #[inline]
    fn clear_canvas(&mut self, pixel: EncodedPixel) {
        let color = pixel.to_u32();
        let bytes_pp = self.bytes_pp as usize;

        if color == 0 {
            self.data.fill(0);
        } else {
            let bytes = color.to_le_bytes();
            match bytes_pp {
                4 => {
                    for chunk in self.data.chunks_exact_mut(4) {
                        chunk.copy_from_slice(&bytes);
                    }
                }
                3 => {
                    for chunk in self.data.chunks_exact_mut(3) {
                        chunk[0] = bytes[0];
                        chunk[1] = bytes[1];
                        chunk[2] = bytes[2];
                    }
                }
                _ => {}
            }
        }
    }

    #[inline]
    fn report_damage(&mut self, rect: damage::DamageRect) {
        self.damage.add(rect);
    }
}

#[cfg(test)]
mod scissor_tests {
    extern crate alloc;

    use super::*;
    use crate::{blend, canvas_ops};
    use slopos_abi::damage::DamageRect;
    use slopos_abi::draw::Color32;

    const W: u32 = 32;
    const H: u32 = 32;

    fn raw_at(buf: &DrawBuffer, x: i32, y: i32) -> u32 {
        let off = y as usize * buf.pitch() + x as usize * buf.bytes_pp() as usize;
        buf.read_encoded_at(off)
    }

    // A solid span fill (the fill_row_span / clip_row_span funnel) must not
    // write a single pixel outside the scissor, even when the requested rect
    // covers the whole buffer.
    #[test]
    fn scissor_confines_fill_rect() {
        let mut data = alloc::vec![0u8; (W * H * 4) as usize];
        let mut buf = DrawBuffer::new(&mut data, W, H, (W * 4) as usize, 4).unwrap();
        buf.set_scissor(Some(DamageRect {
            x0: 8,
            y0: 8,
            x1: 15,
            y1: 15,
        }));
        canvas_ops::fill_rect(&mut buf, 0, 0, W as i32, H as i32, Color32::WHITE);
        assert_ne!(raw_at(&buf, 8, 8), 0, "inside scissor written");
        assert_ne!(raw_at(&buf, 15, 15), 0, "inside scissor written");
        assert_eq!(raw_at(&buf, 7, 8), 0, "left of scissor untouched");
        assert_eq!(raw_at(&buf, 16, 8), 0, "right of scissor untouched");
        assert_eq!(raw_at(&buf, 8, 7), 0, "above scissor untouched");
        assert_eq!(raw_at(&buf, 8, 16), 0, "below scissor untouched");
    }

    // Opaque single pixels (the glyph-blit path, plus circle outlines and
    // vline) route through put_pixel, which must honor the scissor.
    #[test]
    fn scissor_confines_put_pixel() {
        let mut data = alloc::vec![0u8; (W * H * 4) as usize];
        let mut buf = DrawBuffer::new(&mut data, W, H, (W * 4) as usize, 4).unwrap();
        buf.set_scissor(Some(DamageRect {
            x0: 10,
            y0: 10,
            x1: 12,
            y1: 12,
        }));
        let white = buf.pixel_format().encode(Color32::WHITE);
        buf.put_pixel(11, 11, white);
        buf.put_pixel(5, 5, white);
        assert_ne!(raw_at(&buf, 11, 11), 0, "inside scissor written");
        assert_eq!(raw_at(&buf, 5, 5), 0, "outside scissor untouched");
    }

    // The anti-aliased coverage path (line_aa, AA circle / rounded corners)
    // writes via blend::put_pixel_coverage, which must honor the scissor — this
    // is the path the title-bar signal glyphs use on hover.
    #[test]
    fn scissor_confines_line_aa() {
        let mut data = alloc::vec![0u8; (W * H * 4) as usize];
        let mut buf = DrawBuffer::new(&mut data, W, H, (W * 4) as usize, 4).unwrap();
        buf.set_scissor(Some(DamageRect {
            x0: 0,
            y0: 0,
            x1: 15,
            y1: 31,
        }));
        canvas_ops::line_aa(&mut buf, 0, 0, 31, 31, Color32::WHITE);
        for y in 0..H as i32 {
            for x in 16..W as i32 {
                assert_eq!(
                    raw_at(&buf, x, y),
                    0,
                    "AA line wrote past scissor at ({x},{y})"
                );
            }
        }
        assert_ne!(raw_at(&buf, 0, 0), 0, "AA line drew inside scissor");
    }

    // The semi-transparent fill path writes pixels directly (read-modify-write)
    // and must clamp itself to the scissor.
    #[test]
    fn scissor_confines_fill_rect_blended() {
        let mut data = alloc::vec![0u8; (W * H * 4) as usize];
        let mut buf = DrawBuffer::new(&mut data, W, H, (W * 4) as usize, 4).unwrap();
        buf.set_scissor(Some(DamageRect {
            x0: 4,
            y0: 4,
            x1: 7,
            y1: 7,
        }));
        let translucent = Color32::new(255, 255, 255, 128);
        blend::fill_rect_blended(&mut buf, 0, 0, W as i32, H as i32, translucent);
        assert_ne!(raw_at(&buf, 5, 5), 0, "inside scissor blended");
        assert_eq!(raw_at(&buf, 0, 0), 0, "outside scissor untouched");
        assert_eq!(raw_at(&buf, 20, 20), 0, "outside scissor untouched");
    }

    // Clearing the scissor restores whole-buffer drawing (the full-repaint
    // path relies on this).
    #[test]
    fn scissor_none_draws_everywhere() {
        let mut data = alloc::vec![0u8; (W * H * 4) as usize];
        let mut buf = DrawBuffer::new(&mut data, W, H, (W * 4) as usize, 4).unwrap();
        buf.set_scissor(Some(DamageRect {
            x0: 0,
            y0: 0,
            x1: 1,
            y1: 1,
        }));
        buf.set_scissor(None);
        canvas_ops::fill_rect(&mut buf, 0, 0, W as i32, H as i32, Color32::WHITE);
        assert_ne!(raw_at(&buf, 31, 31), 0, "no scissor → whole buffer drawn");
    }

    // with_scissor narrows to the intersection and restores the prior scissor.
    #[test]
    fn with_scissor_intersects_and_restores() {
        let mut data = alloc::vec![0u8; (W * H * 4) as usize];
        let mut buf = DrawBuffer::new(&mut data, W, H, (W * 4) as usize, 4).unwrap();
        buf.set_scissor(Some(DamageRect {
            x0: 0,
            y0: 0,
            x1: 15,
            y1: 15,
        }));
        buf.with_scissor(
            DamageRect {
                x0: 10,
                y0: 10,
                x1: 31,
                y1: 31,
            },
            |b| {
                // Effective scissor is the intersection [10,10]..[15,15].
                canvas_ops::fill_rect(b, 0, 0, W as i32, H as i32, Color32::WHITE);
            },
        );
        assert_ne!(raw_at(&buf, 10, 10), 0, "intersection drawn");
        assert_ne!(raw_at(&buf, 15, 15), 0, "intersection drawn");
        assert_eq!(raw_at(&buf, 9, 10), 0, "outside intersection untouched");
        assert_eq!(raw_at(&buf, 16, 16), 0, "outside intersection untouched");
        // Prior scissor restored: [0,0]..[15,15] still active.
        let white = buf.pixel_format().encode(Color32::WHITE);
        buf.put_pixel(0, 0, white);
        buf.put_pixel(20, 20, white);
        assert_ne!(
            raw_at(&buf, 0, 0),
            0,
            "prior scissor restored (inside drawn)"
        );
        assert_eq!(
            raw_at(&buf, 20, 20),
            0,
            "prior scissor restored (outside untouched)"
        );
    }
}
