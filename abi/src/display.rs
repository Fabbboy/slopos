//! Display information - the canonical type for all framebuffer layers.
//!
//! This is the single source of truth for display properties shared between
//! kernel subsystems and userland. All other display-related types should
//! either use this directly or implement `From` conversions.
//!
//! # ABI Stability
//!
//! This type is `#[repr(C)]` and forms part of the kernel-userland ABI.
//! Field types and order must not change without careful consideration
//! of backward compatibility.

use crate::PixelFormat;

/// Display information - the canonical type for all layers.
///
/// This is the single source of truth for display properties shared
/// between kernel subsystems and userland. All other display-related
/// types should either use this directly or implement `From` conversions.
///
/// # ABI Stability
///
/// This type is `#[repr(C)]` and forms part of the kernel-userland ABI.
/// Field types and order must not change without careful consideration
/// of backward compatibility.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayInfo {
    /// Display width in pixels
    pub width: u32,
    /// Display height in pixels
    pub height: u32,
    /// Bytes per scanline (may be > width * bytes_per_pixel due to alignment)
    pub pitch: u32,
    /// Pixel format (determines bytes per pixel and channel layout)
    pub format: PixelFormat,
}

impl DisplayInfo {
    /// Maximum supported display dimension (sanity bound)
    pub const MAX_DIMENSION: u32 = 8192;

    /// Create a new DisplayInfo with the given parameters.
    #[inline]
    pub const fn new(width: u32, height: u32, pitch: u32, format: PixelFormat) -> Self {
        Self {
            width,
            height,
            pitch,
            format,
        }
    }

    /// Returns bytes per pixel for this display's format.
    #[inline]
    pub fn bytes_per_pixel(&self) -> u8 {
        self.format.bytes_per_pixel()
    }

    /// Returns the total buffer size in bytes.
    #[inline]
    pub fn buffer_size(&self) -> usize {
        self.pitch as usize * self.height as usize
    }

    /// Check if dimensions are valid (non-zero, reasonable bounds).
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= Self::MAX_DIMENSION
            && self.height <= Self::MAX_DIMENSION
            && self.pitch >= self.width * self.bytes_per_pixel() as u32
    }

    /// Create DisplayInfo from raw bootloader values.
    ///
    /// This is used during boot to convert from Limine's framebuffer info.
    /// The pixel format is inferred from bits-per-pixel.
    #[inline]
    pub fn from_raw(width: u64, height: u64, pitch: u64, bpp: u16) -> Self {
        let format = PixelFormat::from_bpp(bpp as u8);
        Self {
            width: width as u32,
            height: height as u32,
            pitch: pitch as u32,
            format,
        }
    }
}

impl PixelFormat {
    /// Infer pixel format from bits-per-pixel (bootloader compatibility).
    /// Best-effort guess: UEFI GOP typically uses BGRA/XRGB for 32bpp.
    #[inline]
    pub fn from_bpp(bpp: u8) -> Self {
        match bpp {
            32 => Self::Argb8888,
            24 => Self::Rgb888,
            _ => Self::Argb8888,
        }
    }
}
