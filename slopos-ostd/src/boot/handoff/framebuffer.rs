//! Framebuffer handoff.
//!
//! Wraps a bootloader-published framebuffer base pointer + dimensions
//! in a typed view that exposes a byte-slice accessor.

use core::ptr::NonNull;

/// View over a bootloader-pre-mapped framebuffer region.
///
/// The base address is virtual — the bootloader has already mapped the
/// framebuffer into the kernel's address space, so no HHDM translation
/// is performed here.
#[derive(Debug)]
pub struct Framebuffer {
    base: NonNull<u8>,
    pitch: usize,
    height: u32,
}

// SAFETY: the framebuffer is a kernel-lifetime resource; its base
// pointer is opaque to consumers (no aliasing rules to violate).
// Marking `Send + Sync` lets the kernel hand the view across CPUs
// (e.g. video driver + compositor coordination).
unsafe impl Send for Framebuffer {}
unsafe impl Sync for Framebuffer {}

impl Framebuffer {
    /// Bootloader-provided base virtual address of the framebuffer.
    #[inline]
    pub fn base(&self) -> NonNull<u8> {
        self.base
    }

    /// Bytes per scanline. May exceed `width * bytes_per_pixel` when
    /// the firmware aligns each row to a hardware-friendly stride.
    #[inline]
    pub fn pitch(&self) -> usize {
        self.pitch
    }

    /// Height in scanlines.
    #[inline]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Total framebuffer size in bytes (`pitch * height`).
    #[inline]
    pub fn byte_size(&self) -> usize {
        self.pitch.saturating_mul(self.height as usize)
    }

    /// Mutable byte slice covering the framebuffer's `pitch * height`
    /// bytes.
    ///
    /// # Safety (interior)
    ///
    /// The bootloader pre-maps the framebuffer with read/write
    /// permission for the kernel's lifetime. The returned slice is
    /// `&'static mut` — callers must not retain two overlapping
    /// mutable borrows. Production callers route every framebuffer
    /// write through the video subsystem's serialised state.
    pub fn as_bytes_mut(&self) -> &'static mut [u8] {
        // SAFETY: kernel-lifetime, pre-mapped, RW. Aliasing discipline
        // is the caller's contract.
        unsafe { core::slice::from_raw_parts_mut(self.base.as_ptr(), self.byte_size()) }
    }
}

/// Construct a [`Framebuffer`] view over a bootloader-published base
/// address + dimensions. The `base` pointer is the pre-mapped virtual
/// address Limine publishes — no HHDM translation is performed.
pub fn framebuffer_handoff(base: NonNull<u8>, pitch: usize, height: u32) -> Framebuffer {
    Framebuffer {
        base,
        pitch,
        height,
    }
}
