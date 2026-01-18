pub mod font;
pub mod primitives;

pub use slopos_abi::DrawTarget;
pub use slopos_abi::damage::{DamageRect, DamageTracker, MAX_DAMAGE_REGIONS};
pub use slopos_abi::pixel::DrawPixelFormat;
use slopos_abi::{PixelBuffer, pixel_ops};

pub type PixelFormat = DrawPixelFormat;

pub struct DrawBuffer<'a> {
    data: &'a mut [u8],
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: DrawPixelFormat,
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
            pixel_format: DrawPixelFormat::from_bpp(bytes_pp * 8),
            damage: DamageTracker::new(),
        })
    }

    pub fn set_pixel_format(&mut self, format: DrawPixelFormat) {
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

    pub fn pixel_format(&self) -> DrawPixelFormat {
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

    #[inline]
    fn pixel_offset(&self, x: u32, y: u32) -> usize {
        (y as usize) * self.pitch + (x as usize) * (self.bytes_pp as usize)
    }

    pub fn set_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }

        let converted = self.pixel_format.convert_color(color);
        let offset = self.pixel_offset(x as u32, y as u32);
        self.write_pixel_at_offset(offset, converted);
    }

    pub fn get_pixel(&self, x: i32, y: i32) -> u32 {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return 0;
        }

        let offset = self.pixel_offset(x as u32, y as u32);
        let raw = match self.bytes_pp {
            4 => {
                if offset + 4 <= self.data.len() {
                    u32::from_le_bytes([
                        self.data[offset],
                        self.data[offset + 1],
                        self.data[offset + 2],
                        self.data[offset + 3],
                    ])
                } else {
                    0
                }
            }
            3 => {
                if offset + 3 <= self.data.len() {
                    u32::from_le_bytes([
                        self.data[offset],
                        self.data[offset + 1],
                        self.data[offset + 2],
                        0xFF,
                    ])
                } else {
                    0
                }
            }
            _ => 0,
        };

        self.pixel_format.convert_color(raw)
    }
}

impl PixelBuffer for DrawBuffer<'_> {
    #[inline]
    fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    fn pitch(&self) -> usize {
        self.pitch
    }

    #[inline]
    fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    #[inline]
    fn pixel_format(&self) -> DrawPixelFormat {
        self.pixel_format
    }

    #[inline]
    fn write_pixel_at_offset(&mut self, byte_offset: usize, color: u32) {
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
    fn fill_row_span(&mut self, row: i32, x0: i32, x1: i32, color: u32) {
        if row < 0 || row >= self.height as i32 {
            return;
        }
        let w = self.width as i32;
        let x0 = x0.max(0);
        let x1 = x1.min(w - 1);
        if x0 > x1 {
            return;
        }

        let bytes_pp = self.bytes_pp as usize;
        let pitch = self.pitch;
        let span_w = (x1 - x0 + 1) as usize;
        let row_off = (row as usize) * pitch + (x0 as usize) * bytes_pp;

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
    fn clear_buffer(&mut self, color: u32) {
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
}

impl DrawTarget for DrawBuffer<'_> {
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
        self.add_damage(0, 0, self.width as i32 - 1, self.height as i32 - 1);
    }
}

impl slopos_abi::DamageTracking for DrawBuffer<'_> {
    fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.damage.add_rect(x0, y0, x1, y1);
    }

    fn clear_damage(&mut self) {
        self.damage.clear();
    }

    fn is_dirty(&self) -> bool {
        self.damage.is_dirty()
    }
}

pub use slopos_abi::pixel::{rgb, rgba};

pub use primitives::*;
