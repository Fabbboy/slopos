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

/// Stand-alone framebuffer-write helpers that accept the raw base
/// pointer + offset, validate against a caller-supplied byte size,
/// and absorb the `unsafe` pointer arithmetic / write inside OSTD.
/// Used by `video/` and `drivers/` to perform framebuffer writes
/// without local `unsafe` blocks.

/// Copy `src.len()` bytes from `src` into the framebuffer mapping at
/// `base + byte_offset`. Caller must have validated
/// `byte_offset + src.len() <= fb_byte_size`.
#[inline]
pub fn fb_copy_bytes(base: *mut u8, byte_offset: usize, src: &[u8]) {
    // SAFETY: caller validates the destination range against the
    // framebuffer byte size before calling.
    unsafe {
        let dst = base.add(byte_offset);
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
    }
}

/// Copy `len` bytes from `src` to the framebuffer mapping at
/// `base + byte_offset`. Caller validates both endpoints.
#[inline]
pub fn fb_copy_bytes_raw(base: *mut u8, byte_offset: usize, src: *const u8, len: usize) {
    // SAFETY: caller validates source and destination ranges.
    unsafe {
        let dst = base.add(byte_offset);
        core::ptr::copy_nonoverlapping(src, dst, len);
    }
}

/// Write a single u32 (e.g. RGBA pixel) at `base + byte_offset` using
/// an unaligned write. Caller validates the byte offset against the
/// framebuffer size.
#[inline]
pub fn fb_write_u32_unaligned(base: *mut u8, byte_offset: usize, value: u32) {
    // SAFETY: caller validates byte offset.
    unsafe {
        let p = base.add(byte_offset) as *mut u32;
        core::ptr::write_unaligned(p, value);
    }
}

/// Write a single u16 at `base + byte_offset`. Used for RGB565 mode.
#[inline]
pub fn fb_write_u16_unaligned(base: *mut u8, byte_offset: usize, value: u16) {
    // SAFETY: caller validates byte offset.
    unsafe {
        let p = base.add(byte_offset) as *mut u16;
        core::ptr::write_unaligned(p, value);
    }
}

/// Write three consecutive bytes at `base + byte_offset .. +3`. Used
/// for 24-bit BGR mode.
#[inline]
pub fn fb_write_3bytes(base: *mut u8, byte_offset: usize, b0: u8, b1: u8, b2: u8) {
    // SAFETY: caller validates byte offset + 2 < fb_byte_size.
    unsafe {
        let p = base.add(byte_offset);
        core::ptr::write(p, b0);
        core::ptr::write(p.add(1), b1);
        core::ptr::write(p.add(2), b2);
    }
}

/// Compute a checked pointer into the framebuffer at the given byte
/// offset; returns `None` if `byte_offset + len` exceeds `fb_byte_size`.
/// Equivalent to `base.add(byte_offset)` under a length precondition.
#[inline]
pub fn fb_checked_ptr(
    base: *mut u8,
    byte_offset: usize,
    len: usize,
    fb_byte_size: usize,
) -> Option<*mut u8> {
    let end = byte_offset.checked_add(len)?;
    if end > fb_byte_size {
        return None;
    }
    if base.is_null() {
        return None;
    }
    // SAFETY: bounds-checked above; base is non-null. The returned
    // pointer is opaque to OSTD — caller uses it only with the
    // companion `fb_*` helpers (which apply their own bounds checks).
    Some(unsafe { base.add(byte_offset) })
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
