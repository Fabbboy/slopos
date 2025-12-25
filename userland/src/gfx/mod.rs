//! Userland graphics library for SlopOS Wayland-like compositor
//!
//! This module provides 100% safe Rust drawing primitives that operate on
//! shared memory buffers. No unsafe code - all drawing is pure slice operations.

pub mod font;
pub mod primitives;

/// Maximum damage regions tracked per buffer before merging
pub const MAX_DAMAGE_REGIONS: usize = 8;

/// A single damage rectangle in buffer-local coordinates
#[derive(Copy, Clone, Default, Debug)]
pub struct DamageRect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32, // Inclusive
    pub y1: i32, // Inclusive
}

impl DamageRect {
    /// Create an invalid (empty) damage rect
    pub const fn invalid() -> Self {
        Self {
            x0: 0,
            y0: 0,
            x1: -1,
            y1: -1,
        }
    }

    /// Check if this rect is valid (non-empty)
    pub fn is_valid(&self) -> bool {
        self.x0 <= self.x1 && self.y0 <= self.y1
    }

    /// Calculate the area of this rect
    pub fn area(&self) -> i32 {
        if !self.is_valid() {
            0
        } else {
            (self.x1 - self.x0 + 1) * (self.y1 - self.y0 + 1)
        }
    }

    /// Compute the union (bounding box) of two rects
    pub fn union(&self, other: &DamageRect) -> DamageRect {
        DamageRect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    /// Calculate what the area would be if merged with another rect
    pub fn combined_area(&self, other: &DamageRect) -> i32 {
        self.union(other).area()
    }
}

/// Tracks damage regions for a buffer, with automatic merging when at capacity
#[derive(Clone)]
pub struct DamageTracker {
    regions: [DamageRect; MAX_DAMAGE_REGIONS],
    count: u8,
}

impl Default for DamageTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DamageTracker {
    /// Create a new empty damage tracker
    pub const fn new() -> Self {
        Self {
            regions: [DamageRect::invalid(); MAX_DAMAGE_REGIONS],
            count: 0,
        }
    }

    /// Add a damage region, merging if at capacity
    pub fn add(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        if x0 > x1 || y0 > y1 {
            return;
        }

        let new_rect = DamageRect { x0, y0, x1, y1 };

        // Merge if at capacity
        if (self.count as usize) >= MAX_DAMAGE_REGIONS {
            self.merge_smallest_pair();
        }

        if (self.count as usize) < MAX_DAMAGE_REGIONS {
            self.regions[self.count as usize] = new_rect;
            self.count += 1;
        }
    }

    /// Merge the two damage regions with the smallest combined area
    fn merge_smallest_pair(&mut self) {
        if self.count < 2 {
            return;
        }

        let count = self.count as usize;
        let mut best_i = 0;
        let mut best_j = 1;
        let mut best_area = i32::MAX;

        for i in 0..count {
            for j in (i + 1)..count {
                let combined = self.regions[i].combined_area(&self.regions[j]);
                if combined < best_area {
                    best_area = combined;
                    best_i = i;
                    best_j = j;
                }
            }
        }

        // Merge i and j into i
        let merged = self.regions[best_i].union(&self.regions[best_j]);
        self.regions[best_i] = merged;

        // Remove j by swapping with last element
        if best_j < count - 1 {
            self.regions[best_j] = self.regions[count - 1];
        }
        self.count -= 1;
    }

    /// Clear all damage
    pub fn clear(&mut self) {
        self.count = 0;
    }

    /// Get the number of damage regions
    pub fn count(&self) -> u8 {
        self.count
    }

    /// Get the damage regions slice
    pub fn regions(&self) -> &[DamageRect] {
        &self.regions[..self.count as usize]
    }

    /// Get the bounding box of all damage regions
    pub fn bounding_box(&self) -> DamageRect {
        if self.count == 0 {
            return DamageRect::invalid();
        }
        let mut result = self.regions[0];
        for i in 1..self.count as usize {
            result = result.union(&self.regions[i]);
        }
        result
    }

    /// Check if there is any damage
    pub fn is_dirty(&self) -> bool {
        self.count > 0
    }
}

/// Pixel format for color conversion
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Rgb,
    Bgr,
    Rgba,
    Bgra,
}

impl PixelFormat {
    /// Create from bits-per-pixel value
    pub fn from_bpp(bpp: u8) -> Self {
        match bpp {
            16 | 24 => PixelFormat::Rgb,
            32 => PixelFormat::Rgba,
            _ => PixelFormat::Rgb,
        }
    }

    /// Convert a color from RGBA to this pixel format
    pub fn convert_color(&self, color: u32) -> u32 {
        match self {
            PixelFormat::Bgr | PixelFormat::Bgra => {
                // Swap R and B channels
                ((color & 0xFF0000) >> 16)
                    | (color & 0x00FF00)
                    | ((color & 0x0000FF) << 16)
                    | (color & 0xFF000000)
            }
            _ => color,
        }
    }
}

/// A safe drawing buffer wrapping a shared memory slice
///
/// This is the core type for userland drawing. It wraps a mutable byte slice
/// and provides safe drawing primitives that handle bounds checking and
/// damage tracking automatically.
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
    /// Create a new DrawBuffer wrapping a shared memory slice
    ///
    /// # Arguments
    /// * `data` - Mutable byte slice (shared memory buffer)
    /// * `width` - Buffer width in pixels
    /// * `height` - Buffer height in pixels
    /// * `pitch` - Row stride in bytes (usually width * bytes_pp)
    /// * `bytes_pp` - Bytes per pixel (3 or 4)
    pub fn new(
        data: &'a mut [u8],
        width: u32,
        height: u32,
        pitch: usize,
        bytes_pp: u8,
    ) -> Option<Self> {
        // Validate buffer size
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
            pixel_format: PixelFormat::from_bpp(bytes_pp * 8),
            damage: DamageTracker::new(),
        })
    }

    /// Set the pixel format for color conversion
    pub fn set_pixel_format(&mut self, format: PixelFormat) {
        self.pixel_format = format;
    }

    /// Get buffer width
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Get buffer height
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Get row pitch in bytes
    pub fn pitch(&self) -> usize {
        self.pitch
    }

    /// Get bytes per pixel
    pub fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    /// Get the underlying data slice
    pub fn data(&self) -> &[u8] {
        self.data
    }

    /// Get mutable access to the underlying data slice
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.data
    }

    /// Get the damage tracker
    pub fn damage(&self) -> &DamageTracker {
        &self.damage
    }

    /// Get mutable access to damage tracker
    pub fn damage_mut(&mut self) -> &mut DamageTracker {
        &mut self.damage
    }

    /// Clear all damage
    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }

    /// Add a damage region
    pub fn add_damage(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        // Clip to buffer bounds
        let x0 = x0.max(0);
        let y0 = y0.max(0);
        let x1 = x1.min(self.width as i32 - 1);
        let y1 = y1.min(self.height as i32 - 1);

        if x0 <= x1 && y0 <= y1 {
            self.damage.add(x0, y0, x1, y1);
        }
    }

    /// Calculate the byte offset for a pixel coordinate
    #[inline]
    fn pixel_offset(&self, x: u32, y: u32) -> usize {
        (y as usize) * self.pitch + (x as usize) * (self.bytes_pp as usize)
    }

    /// Set a single pixel (with bounds checking)
    pub fn set_pixel(&mut self, x: i32, y: i32, color: u32) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }

        let converted = self.pixel_format.convert_color(color);
        let offset = self.pixel_offset(x as u32, y as u32);

        match self.bytes_pp {
            4 => {
                if offset + 4 <= self.data.len() {
                    let bytes = converted.to_le_bytes();
                    self.data[offset..offset + 4].copy_from_slice(&bytes);
                }
            }
            3 => {
                if offset + 3 <= self.data.len() {
                    let bytes = converted.to_le_bytes();
                    self.data[offset] = bytes[0];
                    self.data[offset + 1] = bytes[1];
                    self.data[offset + 2] = bytes[2];
                }
            }
            _ => {}
        }
    }

    /// Get a single pixel (with bounds checking)
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

        // Convert back from pixel format
        self.pixel_format.convert_color(raw)
    }

    /// Clear the entire buffer to a color
    pub fn clear(&mut self, color: u32) {
        let converted = self.pixel_format.convert_color(color);
        let bytes_pp = self.bytes_pp as usize;

        if converted == 0 {
            self.data.fill(0);
        } else {
            let bytes = converted.to_le_bytes();
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

        self.add_damage(0, 0, self.width as i32 - 1, self.height as i32 - 1);
    }
}

/// Create an RGBA color from components
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((r as u32) << 16) | ((g as u32) << 8) | (b as u32) | ((a as u32) << 24)
}

/// Create an RGB color from components (alpha = 255)
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    rgba(r, g, b, 0xFF)
}

// Re-export primitives for convenience
pub use primitives::*;
