use crate::PixelFormat;

/// Canonical display description shared between kernel subsystems and userland.
/// Part of the kernel-userland ABI: field types and order are frozen.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    /// Bytes per scanline (may be > width * bytes_per_pixel due to alignment)
    pub pitch: u32,
    pub format: PixelFormat,
}

impl DisplayInfo {
    /// Maximum supported display dimension (sanity bound)
    pub const MAX_DIMENSION: u32 = 8192;

    #[inline]
    pub const fn new(width: u32, height: u32, pitch: u32, format: PixelFormat) -> Self {
        Self {
            width,
            height,
            pitch,
            format,
        }
    }

    #[inline]
    pub fn bytes_per_pixel(&self) -> u8 {
        self.format.bytes_per_pixel()
    }

    #[inline]
    pub fn buffer_size(&self) -> usize {
        self.pitch as usize * self.height as usize
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= Self::MAX_DIMENSION
            && self.height <= Self::MAX_DIMENSION
            && self.pitch >= self.width * self.bytes_per_pixel() as u32
    }

    /// Converts Limine's raw framebuffer values; format is inferred from `bpp`.
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
    /// Best-effort guess for bootloader framebuffers: UEFI GOP typically uses
    /// BGRA/XRGB at 32bpp.
    #[inline]
    pub fn from_bpp(bpp: u8) -> Self {
        match bpp {
            32 => Self::Argb8888,
            24 => Self::Rgb888,
            _ => Self::Argb8888,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FramebufferData {
    pub address: *mut u8,
    pub info: DisplayInfo,
}
