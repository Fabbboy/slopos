//! Shared memory surface for compositor-managed windows.
//!
//! `Surface` encapsulates the full lifecycle of a window's backing store:
//! display info query, pixel format negotiation, SHM allocation, and
//! compositor attachment. Applications use `Surface::frame()` to obtain
//! a `DrawBuffer` for rendering and `Surface::present_full()` /
//! `Surface::present_region()` to push completed frames to the compositor.

use crate::gfx::{DrawBuffer, PixelFormat};
use crate::syscall::{DisplayInfo, ShmBuffer, window};

#[derive(Debug, Clone, Copy)]
pub enum SurfaceError {
    NoDisplay,
    BadSize,
    ShmFailed,
    AttachFailed,
}

/// A compositor-managed shared memory surface.
///
/// Owns the `ShmBuffer` and all associated metadata (dimensions, pitch,
/// pixel format). Created once per window via `Surface::new()`.
pub struct Surface {
    shm: ShmBuffer,
    width: u32,
    height: u32,
    pitch: usize,
    bytes_pp: u8,
    pixel_format: PixelFormat,
}

impl Surface {
    /// Create a new surface and attach it to the compositor.
    ///
    /// Queries the display for pixel format information, allocates a
    /// shared memory buffer of the appropriate size, and registers it
    /// as this task's surface.
    pub fn new(width: u32, height: u32) -> Result<Self, SurfaceError> {
        if width == 0 || height == 0 {
            return Err(SurfaceError::BadSize);
        }

        let mut fb_info = DisplayInfo::default();
        if window::fb_info(&mut fb_info) < 0 {
            return Err(SurfaceError::NoDisplay);
        }
        if fb_info.width == 0 || fb_info.height == 0 {
            return Err(SurfaceError::NoDisplay);
        }

        let pixel_format = fb_info.format;
        let bytes_pp = pixel_format.bytes_per_pixel();
        let pitch = (width as usize)
            .checked_mul(bytes_pp as usize)
            .ok_or(SurfaceError::BadSize)?;
        let buffer_size = pitch
            .checked_mul(height as usize)
            .ok_or(SurfaceError::BadSize)?;

        let shm = ShmBuffer::create(buffer_size).map_err(|_| SurfaceError::ShmFailed)?;
        shm.attach_surface(width, height)
            .map_err(|_| SurfaceError::AttachFailed)?;

        Ok(Self {
            shm,
            width,
            height,
            pitch,
            bytes_pp,
            pixel_format,
        })
    }

    /// Borrow a `DrawBuffer` for the current frame.
    ///
    /// The returned buffer has the correct pixel format already set.
    /// Returns `None` only if the internal dimensions are inconsistent
    /// (should not happen after successful construction).
    pub fn frame(&mut self) -> Option<DrawBuffer<'_>> {
        let mut buf = DrawBuffer::new(
            self.shm.as_mut_slice(),
            self.width,
            self.height,
            self.pitch,
            self.bytes_pp,
        )?;
        buf.set_pixel_format(self.pixel_format);
        Some(buf)
    }

    /// Mark the full surface as damaged and commit to the compositor.
    pub fn present_full(&self) {
        let _ = window::surface_damage(0, 0, self.width as i32, self.height as i32);
        let _ = window::surface_commit();
    }

    /// Mark a sub-region as damaged and commit to the compositor.
    pub fn present_region(&self, x: i32, y: i32, w: i32, h: i32) {
        let _ = window::surface_damage(x, y, w, h);
        let _ = window::surface_commit();
    }

    #[inline]
    pub fn width(&self) -> u32 {
        self.width
    }

    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    #[inline]
    pub fn pixel_format(&self) -> PixelFormat {
        self.pixel_format
    }

    #[inline]
    pub fn bytes_pp(&self) -> u8 {
        self.bytes_pp
    }

    #[inline]
    pub fn pitch(&self) -> usize {
        self.pitch
    }
}
