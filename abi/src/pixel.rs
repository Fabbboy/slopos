//! Pixel format definitions (Wayland wl_shm compatible)

use crate::draw::{Color32, EncodedPixel};

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

    /// Encode a `Color32` (0xAARRGGBB) into the native pixel format.
    #[inline]
    pub fn encode(self, color: Color32) -> EncodedPixel {
        let v = color.0;
        let a = (v >> 24) & 0xFF;
        let r = (v >> 16) & 0xFF;
        let g = (v >> 8) & 0xFF;
        let b = v & 0xFF;

        EncodedPixel(match self {
            Self::Argb8888 => (a << 24) | (r << 16) | (g << 8) | b,
            Self::Xrgb8888 => (0xFF << 24) | (r << 16) | (g << 8) | b,
            Self::Rgba8888 => (r << 24) | (g << 16) | (b << 8) | a,
            Self::Bgra8888 => (b << 24) | (g << 16) | (r << 8) | a,
            Self::Rgb888 => (r << 16) | (g << 8) | b,
            Self::Bgr888 => (b << 16) | (g << 8) | r,
        })
    }

    /// Decode a raw encoded pixel value back to `Color32` (0xAARRGGBB).
    ///
    /// This is the inverse of `encode()`. For 3-byte formats without alpha,
    /// the returned alpha is 0xFF (fully opaque).
    #[inline]
    pub fn decode(self, raw: u32) -> Color32 {
        match self {
            Self::Argb8888 => Color32(raw),
            Self::Xrgb8888 => Color32(0xFF000000 | (raw & 0x00FFFFFF)),
            Self::Rgba8888 => {
                let r = (raw >> 24) & 0xFF;
                let g = (raw >> 16) & 0xFF;
                let b = (raw >> 8) & 0xFF;
                let a = raw & 0xFF;
                Color32((a << 24) | (r << 16) | (g << 8) | b)
            }
            Self::Bgra8888 => {
                let b = (raw >> 24) & 0xFF;
                let g = (raw >> 16) & 0xFF;
                let r = (raw >> 8) & 0xFF;
                let a = raw & 0xFF;
                Color32((a << 24) | (r << 16) | (g << 8) | b)
            }
            Self::Rgb888 => {
                let r = (raw >> 16) & 0xFF;
                let g = (raw >> 8) & 0xFF;
                let b = raw & 0xFF;
                Color32(0xFF000000 | (r << 16) | (g << 8) | b)
            }
            Self::Bgr888 => {
                let b = (raw >> 16) & 0xFF;
                let g = (raw >> 8) & 0xFF;
                let r = raw & 0xFF;
                Color32(0xFF000000 | (r << 16) | (g << 8) | b)
            }
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
