use slopos_abi::damage;
use slopos_abi::draw::{Canvas, Color32, EncodedPixel};
use slopos_abi::pixel::PixelFormat;

use crate::DamageTracker;

/// A safe, heap-free pixel buffer that implements [`Canvas`].
///
/// `DrawBuffer` wraps a caller-supplied `&mut [u8]` slice and provides
/// bounds-checked pixel writes, automatic damage tracking, and block-level
/// operations such as blitting and scrolling.
///
/// Both the kernel (via framebuffer MMIO) and userland (via shared-memory
/// surfaces) can construct a `DrawBuffer` over any appropriately-sized
/// byte slice.
pub struct DrawBuffer<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
    damage: DamageTracker,
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
