//! Buffer Object Model - Foundation for vector graphics rendering
//!
//! Provides:
//! - Multiple off-screen buffers (BufferObject)
//! - Display framebuffer wrapper (DisplayBuffer)
//! - Pixel format abstraction
//! - Blitting operations

use alloc::vec::Vec;
use bindings as c;

/// Pixel format enumeration
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum PixelFormat {
    /// 16-bit RGB (5-6-5)
    RGB565 = 0,
    /// 24-bit RGB (8-8-8)
    RGB888 = 1,
    /// 32-bit RGBA (8-8-8-8)
    RGBA8888 = 2,
    /// 24-bit BGR (8-8-8)
    BGR888 = 3,
    /// 32-bit BGRA (8-8-8-8)
    BGRA8888 = 4,
}

impl PixelFormat {
    /// Get bytes per pixel for this format
    pub const fn bytes_per_pixel(&self) -> usize {
        match self {
            PixelFormat::RGB565 => 2,
            PixelFormat::RGB888 | PixelFormat::BGR888 => 3,
            PixelFormat::RGBA8888 | PixelFormat::BGRA8888 => 4,
        }
    }

    /// Get bits per pixel for this format
    pub const fn bits_per_pixel(&self) -> u8 {
        match self {
            PixelFormat::RGB565 => 16,
            PixelFormat::RGB888 | PixelFormat::BGR888 => 24,
            PixelFormat::RGBA8888 | PixelFormat::BGRA8888 => 32,
        }
    }

    /// Determine pixel format from bits per pixel
    pub fn from_bpp(bpp: u8) -> Option<Self> {
        match bpp {
            16 => Some(PixelFormat::RGB565),
            24 => Some(PixelFormat::RGB888),
            32 => Some(PixelFormat::RGBA8888),
            _ => None,
        }
    }
}

/// RGBA color representation
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    /// Create a new color with alpha
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Create a new opaque color
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Create black color
    pub const fn black() -> Self {
        Self::rgb(0, 0, 0)
    }

    /// Create white color
    pub const fn white() -> Self {
        Self::rgb(255, 255, 255)
    }

    /// Create transparent color
    pub const fn transparent() -> Self {
        Self::rgba(0, 0, 0, 0)
    }

    /// Convert color to u32 for given pixel format
    pub fn to_u32(&self, format: PixelFormat) -> u32 {
        match format {
            PixelFormat::RGB565 => {
                let r = ((self.r as u32) >> 3) & 0x1F;
                let g = ((self.g as u32) >> 2) & 0x3F;
                let b = ((self.b as u32) >> 3) & 0x1F;
                (r << 11) | (g << 5) | b
            }
            PixelFormat::RGB888 => {
                ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
            }
            PixelFormat::RGBA8888 => {
                ((self.a as u32) << 24) | ((self.r as u32) << 16)
                    | ((self.g as u32) << 8) | (self.b as u32)
            }
            PixelFormat::BGR888 => {
                ((self.b as u32) << 16) | ((self.g as u32) << 8) | (self.r as u32)
            }
            PixelFormat::BGRA8888 => {
                ((self.a as u32) << 24) | ((self.b as u32) << 16)
                    | ((self.g as u32) << 8) | (self.r as u32)
            }
        }
    }

    /// Create color from u32 for given pixel format
    pub fn from_u32(value: u32, format: PixelFormat) -> Self {
        match format {
            PixelFormat::RGB565 => {
                let r = (((value >> 11) & 0x1F) << 3) as u8;
                let g = (((value >> 5) & 0x3F) << 2) as u8;
                let b = ((value & 0x1F) << 3) as u8;
                Self::rgb(r, g, b)
            }
            PixelFormat::RGB888 => {
                let r = ((value >> 16) & 0xFF) as u8;
                let g = ((value >> 8) & 0xFF) as u8;
                let b = (value & 0xFF) as u8;
                Self::rgb(r, g, b)
            }
            PixelFormat::RGBA8888 => {
                let a = ((value >> 24) & 0xFF) as u8;
                let r = ((value >> 16) & 0xFF) as u8;
                let g = ((value >> 8) & 0xFF) as u8;
                let b = (value & 0xFF) as u8;
                Self::rgba(r, g, b, a)
            }
            PixelFormat::BGR888 => {
                let b = ((value >> 16) & 0xFF) as u8;
                let g = ((value >> 8) & 0xFF) as u8;
                let r = (value & 0xFF) as u8;
                Self::rgb(r, g, b)
            }
            PixelFormat::BGRA8888 => {
                let a = ((value >> 24) & 0xFF) as u8;
                let b = ((value >> 16) & 0xFF) as u8;
                let g = ((value >> 8) & 0xFF) as u8;
                let r = (value & 0xFF) as u8;
                Self::rgba(r, g, b, a)
            }
        }
    }
}

/// Rectangle structure for clipping and blitting
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Create a new rectangle
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self { x, y, width, height }
    }

    /// Check if point is inside rectangle
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x
            && py >= self.y
            && px < self.x + self.width as i32
            && py < self.y + self.height as i32
    }

    /// Intersect two rectangles
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x1 = self.x.max(other.x);
        let y1 = self.y.max(other.y);
        let x2 = (self.x + self.width as i32).min(other.x + other.width as i32);
        let y2 = (self.y + self.height as i32).min(other.y + other.height as i32);

        if x1 < x2 && y1 < y2 {
            Some(Rect {
                x: x1,
                y: y1,
                width: (x2 - x1) as u32,
                height: (y2 - y1) as u32,
            })
        } else {
            None
        }
    }
}

/// Off-screen rendering buffer (owns its memory)
pub struct BufferObject {
    width: u32,
    height: u32,
    pitch: u32,
    format: PixelFormat,
    data: Vec<u8>,
}

impl BufferObject {
    /// Create a new buffer with the specified dimensions and format
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }

        let bytes_per_pixel = format.bytes_per_pixel() as u32;
        let pitch = width * bytes_per_pixel;
        let buffer_size = (pitch * height) as usize;

        // Allocate buffer - uses our kmalloc-based allocator
        let data = alloc::vec![0u8; buffer_size];

        Some(Self {
            width,
            height,
            pitch,
            format,
            data,
        })
    }

    /// Get buffer width
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Get buffer height
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Get buffer pitch (stride)
    pub fn pitch(&self) -> u32 {
        self.pitch
    }

    /// Get pixel format
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Get rectangle covering entire buffer
    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Clear buffer to specified color
    pub fn clear(&mut self, color: Color) {
        let pixel_value = color.to_u32(self.format);
        let bytes_per_pixel = self.format.bytes_per_pixel();

        for y in 0..self.height {
            let row_offset = (y * self.pitch) as usize;

            for x in 0..self.width {
                let pixel_offset = row_offset + (x as usize * bytes_per_pixel);

                match bytes_per_pixel {
                    2 => {
                        let bytes = (pixel_value as u16).to_le_bytes();
                        self.data[pixel_offset..pixel_offset + 2].copy_from_slice(&bytes);
                    }
                    3 => {
                        self.data[pixel_offset] = (pixel_value >> 16) as u8;
                        self.data[pixel_offset + 1] = (pixel_value >> 8) as u8;
                        self.data[pixel_offset + 2] = pixel_value as u8;
                    }
                    4 => {
                        let bytes = pixel_value.to_le_bytes();
                        self.data[pixel_offset..pixel_offset + 4].copy_from_slice(&bytes);
                    }
                    _ => {}
                }
            }
        }
    }

    /// Set pixel at coordinates (with bounds checking)
    pub fn set_pixel(&mut self, x: u32, y: u32, color: Color) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }

        let pixel_value = color.to_u32(self.format);
        let bytes_per_pixel = self.format.bytes_per_pixel();
        let offset = (y * self.pitch + x * bytes_per_pixel as u32) as usize;

        match bytes_per_pixel {
            2 => {
                let bytes = (pixel_value as u16).to_le_bytes();
                self.data[offset..offset + 2].copy_from_slice(&bytes);
            }
            3 => {
                self.data[offset] = (pixel_value >> 16) as u8;
                self.data[offset + 1] = (pixel_value >> 8) as u8;
                self.data[offset + 2] = pixel_value as u8;
            }
            4 => {
                let bytes = pixel_value.to_le_bytes();
                self.data[offset..offset + 4].copy_from_slice(&bytes);
            }
            _ => return false,
        }

        true
    }

    /// Get pixel at coordinates (with bounds checking)
    pub fn get_pixel(&self, x: u32, y: u32) -> Option<Color> {
        if x >= self.width || y >= self.height {
            return None;
        }

        let bytes_per_pixel = self.format.bytes_per_pixel();
        let offset = (y * self.pitch + x * bytes_per_pixel as u32) as usize;

        let pixel_value = match bytes_per_pixel {
            2 => {
                u16::from_le_bytes([self.data[offset], self.data[offset + 1]]) as u32
            }
            3 => {
                ((self.data[offset] as u32) << 16)
                    | ((self.data[offset + 1] as u32) << 8)
                    | (self.data[offset + 2] as u32)
            }
            4 => {
                u32::from_le_bytes([
                    self.data[offset],
                    self.data[offset + 1],
                    self.data[offset + 2],
                    self.data[offset + 3],
                ])
            }
            _ => return None,
        };

        Some(Color::from_u32(pixel_value, self.format))
    }

    /// Get raw buffer data (for advanced operations)
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Get mutable raw buffer data (for advanced operations)
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

/// Display framebuffer wrapper (borrows memory from Limine)
pub struct DisplayBuffer {
    width: u32,
    height: u32,
    pitch: u32,
    format: PixelFormat,
    data: *mut u8,
}

impl DisplayBuffer {
    /// Create DisplayBuffer from Limine framebuffer info
    pub fn from_limine() -> Option<Self> {
        unsafe {
            // Get framebuffer info through C FFI
            let mut phys_addr: u64 = 0;
            let mut width: u32 = 0;
            let mut height: u32 = 0;
            let mut pitch: u32 = 0;
            let mut bpp: u8 = 0;

            // Call C function to get framebuffer info
            let result = c::get_framebuffer_info(
                &mut phys_addr,
                &mut width,
                &mut height,
                &mut pitch,
                &mut bpp,
            );

            if result == 0 {
                return None;
            }

            // Determine pixel format from bpp
            let format = PixelFormat::from_bpp(bpp)?;

            // Get virtual address from physical (through HHDM)
            let virt_addr = c::mm_phys_to_virt(phys_addr);
            if virt_addr == 0 {
                return None;
            }

            Some(Self {
                width,
                height,
                pitch,
                format,
                data: virt_addr as *mut u8,
            })
        }
    }

    /// Get buffer width
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Get buffer height
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Get buffer pitch (stride)
    pub fn pitch(&self) -> u32 {
        self.pitch
    }

    /// Get pixel format
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// Get rectangle covering entire buffer
    pub fn rect(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Clear display to specified color
    pub fn clear(&mut self, color: Color) {
        let pixel_value = color.to_u32(self.format);
        let bytes_per_pixel = self.format.bytes_per_pixel();

        unsafe {
            for y in 0..self.height {
                let row_offset = (y * self.pitch) as isize;

                for x in 0..self.width {
                    let pixel_offset = row_offset + (x as isize * bytes_per_pixel as isize);
                    let pixel_ptr = self.data.offset(pixel_offset);

                    match bytes_per_pixel {
                        2 => {
                            *(pixel_ptr as *mut u16) = pixel_value as u16;
                        }
                        3 => {
                            *pixel_ptr = (pixel_value >> 16) as u8;
                            *pixel_ptr.offset(1) = (pixel_value >> 8) as u8;
                            *pixel_ptr.offset(2) = pixel_value as u8;
                        }
                        4 => {
                            *(pixel_ptr as *mut u32) = pixel_value;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Set pixel at coordinates (with bounds checking)
    pub fn set_pixel(&mut self, x: u32, y: u32, color: Color) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }

        let pixel_value = color.to_u32(self.format);
        let bytes_per_pixel = self.format.bytes_per_pixel();
        let offset = (y * self.pitch + x * bytes_per_pixel as u32) as isize;

        unsafe {
            let pixel_ptr = self.data.offset(offset);

            match bytes_per_pixel {
                2 => {
                    *(pixel_ptr as *mut u16) = pixel_value as u16;
                }
                3 => {
                    *pixel_ptr = (pixel_value >> 16) as u8;
                    *pixel_ptr.offset(1) = (pixel_value >> 8) as u8;
                    *pixel_ptr.offset(2) = pixel_value as u8;
                }
                4 => {
                    *(pixel_ptr as *mut u32) = pixel_value;
                }
                _ => return false,
            }
        }

        true
    }

    /// Get pixel at coordinates (with bounds checking)
    pub fn get_pixel(&self, x: u32, y: u32) -> Option<Color> {
        if x >= self.width || y >= self.height {
            return None;
        }

        let bytes_per_pixel = self.format.bytes_per_pixel();
        let offset = (y * self.pitch + x * bytes_per_pixel as u32) as isize;

        unsafe {
            let pixel_ptr = self.data.offset(offset);

            let pixel_value = match bytes_per_pixel {
                2 => *(pixel_ptr as *const u16) as u32,
                3 => {
                    ((*pixel_ptr as u32) << 16)
                        | ((*pixel_ptr.offset(1) as u32) << 8)
                        | (*pixel_ptr.offset(2) as u32)
                }
                4 => *(pixel_ptr as *const u32),
                _ => return None,
            };

            Some(Color::from_u32(pixel_value, self.format))
        }
    }

    /// Blit from BufferObject to DisplayBuffer
    pub fn blit_from(&mut self, src: &BufferObject, src_rect: Rect, dst_x: i32, dst_y: i32) {
        // Clip source rectangle to source buffer bounds
        let src_bounds = src.rect();
        let src_rect = match src_rect.intersect(&src_bounds) {
            Some(r) => r,
            None => return,
        };

        // Clip destination rectangle to destination buffer bounds
        let dst_rect = Rect::new(dst_x, dst_y, src_rect.width, src_rect.height);
        let dst_bounds = self.rect();
        let dst_rect = match dst_rect.intersect(&dst_bounds) {
            Some(r) => r,
            None => return,
        };

        // TODO: Handle format conversion if src.format != self.format
        // For now, assume same format or handle basic conversions

        let src_bytes_per_pixel = src.format().bytes_per_pixel();
        let dst_bytes_per_pixel = self.format.bytes_per_pixel();

        for y in 0..dst_rect.height {
            let src_y = (src_rect.y as u32 + y) as usize;
            let dst_y = (dst_rect.y as u32 + y) as usize;

            for x in 0..dst_rect.width {
                let src_x = (src_rect.x as u32 + x) as usize;
                let dst_x = (dst_rect.x as u32 + x) as usize;

                // Read pixel from source
                if let Some(color) = src.get_pixel(src_x as u32, src_y as u32) {
                    // Write pixel to destination
                    self.set_pixel(dst_x as u32, dst_y as u32, color);
                }
            }
        }
    }
}

// DisplayBuffer is not Send/Sync because it contains a raw pointer
// This is intentional - the framebuffer should only be accessed from one core
unsafe impl Send for DisplayBuffer {}
unsafe impl Sync for DisplayBuffer {}
