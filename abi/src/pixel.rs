//! Pixel format definitions (Wayland wl_shm compatible)

/// Construct an ARGB color value: 0xAARRGGBB (alpha high byte, blue low byte).
#[inline]
pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> u32 {
    ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
}

/// Construct an opaque ARGB color value (alpha=0xFF).
#[inline]
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    rgba(r, g, b, 0xFF)
}

/// Pixel format for shared memory buffers.
///
/// These values match the Wayland wl_shm format constants.
/// This is the canonical definition used by both kernel and userland.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PixelFormat {
    /// 32-bit ARGB (alpha in high byte, red in bits 16-23)
    /// Memory layout: [B, G, R, A] (little-endian)
    #[default]
    Argb8888 = 0,
    /// 32-bit XRGB (alpha ignored, red in bits 16-23)
    /// Memory layout: [B, G, R, X] (little-endian)
    Xrgb8888 = 1,
    /// 24-bit RGB (no alpha)
    /// Memory layout: [B, G, R] (little-endian)
    Rgb888 = 2,
    /// 24-bit BGR (no alpha)
    /// Memory layout: [R, G, B] (little-endian)
    Bgr888 = 3,
    /// 32-bit RGBA (red in high byte, alpha in bits 0-7)
    /// Memory layout: [A, B, G, R] (little-endian)
    Rgba8888 = 4,
    /// 32-bit BGRA (blue in high byte, alpha in bits 0-7)
    /// Memory layout: [A, R, G, B] (little-endian)
    Bgra8888 = 5,
}

impl PixelFormat {
    /// Convert from u32 representation
    #[inline]
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(Self::Argb8888),
            1 => Some(Self::Xrgb8888),
            2 => Some(Self::Rgb888),
            3 => Some(Self::Bgr888),
            4 => Some(Self::Rgba8888),
            5 => Some(Self::Bgra8888),
            _ => None,
        }
    }

    /// Get bytes per pixel for this format
    #[inline]
    pub fn bytes_per_pixel(self) -> u8 {
        match self {
            Self::Argb8888 | Self::Xrgb8888 | Self::Rgba8888 | Self::Bgra8888 => 4,
            Self::Rgb888 | Self::Bgr888 => 3,
        }
    }

    /// Check if format has an alpha channel
    #[inline]
    pub fn has_alpha(self) -> bool {
        matches!(self, Self::Argb8888 | Self::Rgba8888 | Self::Bgra8888)
    }

    /// Check if format uses BGR byte order (vs RGB)
    #[inline]
    pub fn is_bgr_order(self) -> bool {
        matches!(self, Self::Argb8888 | Self::Xrgb8888 | Self::Bgra8888)
    }

    /// Convert a color value from RGBA format to this pixel format
    ///
    /// Input color is 0xRRGGBBAA format (red in high byte)
    #[inline]
    pub fn convert_color(self, rgba: u32) -> u32 {
        let r = (rgba >> 24) & 0xFF;
        let g = (rgba >> 16) & 0xFF;
        let b = (rgba >> 8) & 0xFF;
        let a = rgba & 0xFF;

        match self {
            Self::Argb8888 => (a << 24) | (r << 16) | (g << 8) | b,
            Self::Xrgb8888 => (0xFF << 24) | (r << 16) | (g << 8) | b,
            Self::Rgba8888 => (r << 24) | (g << 16) | (b << 8) | a,
            Self::Bgra8888 => (b << 24) | (g << 16) | (r << 8) | a,
            Self::Rgb888 => (r << 16) | (g << 8) | b,
            Self::Bgr888 => (b << 16) | (g << 8) | r,
        }
    }

    /// Get a bitmap of all supported formats
    ///
    /// Returns a u32 where bit N is set if format with value N is supported.
    #[inline]
    pub fn supported_formats_bitmap() -> u32 {
        (1 << Self::Argb8888 as u32)
            | (1 << Self::Xrgb8888 as u32)
            | (1 << Self::Rgb888 as u32)
            | (1 << Self::Bgr888 as u32)
            | (1 << Self::Rgba8888 as u32)
            | (1 << Self::Bgra8888 as u32)
    }
}

/// Simple pixel format enum for drawing operations
///
/// This is a simplified version for userland drawing that maps
/// to the full PixelFormat enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum DrawPixelFormat {
    /// RGB byte order (R in high bits)
    #[default]
    Rgb,
    /// BGR byte order (B in high bits)
    Bgr,
    /// RGBA with alpha
    Rgba,
    /// BGRA with alpha
    Bgra,
}

impl DrawPixelFormat {
    /// Create from bits-per-pixel and format hints
    #[inline]
    pub fn from_bpp(bpp: u8) -> Self {
        match bpp {
            16 | 24 => Self::Rgb,
            32 => Self::Rgba,
            _ => Self::Rgb,
        }
    }

    /// Convert from PixelFormat
    #[inline]
    pub fn from_pixel_format(fmt: PixelFormat) -> Self {
        match fmt {
            PixelFormat::Rgb888 => Self::Rgb,
            PixelFormat::Bgr888 => Self::Bgr,
            PixelFormat::Rgba8888 => Self::Rgba,
            PixelFormat::Argb8888 | PixelFormat::Xrgb8888 | PixelFormat::Bgra8888 => Self::Bgra,
        }
    }

    /// Convert a color from standard format to this pixel format
    ///
    /// Input color is 0xAARRGGBB format. BGR formats (Bgr, Bgra) match this
    /// layout on little-endian, so no conversion needed. RGB formats need swap.
    #[inline]
    pub fn convert_color(self, color: u32) -> u32 {
        match self {
            Self::Rgb | Self::Rgba => {
                ((color & 0xFF0000) >> 16)
                    | (color & 0x00FF00)
                    | ((color & 0x0000FF) << 16)
                    | (color & 0xFF000000)
            }
            _ => color,
        }
    }
}
