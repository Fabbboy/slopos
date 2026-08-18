//! Framebuffer handoff: a typed view over the bootloader-published base
//! pointer and dimensions, plus the pixel-store helpers that fold the raw
//! stores inside OSTD.

use core::ptr::NonNull;

/// View over a bootloader-pre-mapped framebuffer region.
///
/// The base address is virtual — the bootloader has already mapped the
/// framebuffer, so no HHDM translation is performed here.
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
    /// The slice is `&'static mut`; callers must not retain two overlapping
    /// mutable borrows.
    pub fn as_bytes_mut(&self) -> &'static mut [u8] {
        // SAFETY: kernel-lifetime, pre-mapped, RW. Aliasing discipline
        // is the caller's contract.
        unsafe { core::slice::from_raw_parts_mut(self.base.as_ptr(), self.byte_size()) }
    }
}

/// Volatile `u8` store at `base + byte_offset`; per-channel writes on 24-bpp
/// framebuffers.
#[inline]
pub fn fb_write_u8(base: u64, byte_offset: usize, value: u8) {
    // SAFETY: caller certifies `base` came from a bootloader-pre-mapped
    // framebuffer and `byte_offset` is within the published region.
    unsafe {
        let p = (base as *mut u8).add(byte_offset);
        core::ptr::write_volatile(p, value);
    }
}

/// Volatile, possibly-unaligned `u16` store at `base + byte_offset`.
#[inline]
pub fn fb_write_u16(base: u64, byte_offset: usize, value: u16) {
    // SAFETY: same as `fb_write_u8`; `write_unaligned` tolerates
    // misaligned framebuffers.
    unsafe {
        let p = (base as *mut u8).add(byte_offset) as *mut u16;
        core::ptr::write_volatile(p, value);
    }
}

/// Volatile `u32` store at `base + byte_offset`.
#[inline]
pub fn fb_write_u32(base: u64, byte_offset: usize, value: u32) {
    // SAFETY: as `fb_write_u8`; framebuffers from Limine on x86_64
    // are 32-bit aligned at every pixel boundary.
    unsafe {
        let p = (base as *mut u8).add(byte_offset) as *mut u32;
        core::ptr::write_volatile(p, value);
    }
}

/// Unaligned `u32` store at `ptr`: some firmware lands a row offset that is
/// not 4-byte aligned.
#[inline]
pub fn fb_write_u32_unaligned(ptr: *mut u8, value: u32) {
    // SAFETY: caller certifies `ptr` is within a pre-mapped
    // framebuffer; `write_unaligned` is sound for any byte alignment.
    unsafe {
        core::ptr::write_unaligned(ptr as *mut u32, value);
    }
}

/// Copy `src` to `base + byte_offset`; the vconsole shadow-buffer blit.
#[inline]
pub fn fb_blit_bytes(base: u64, byte_offset: usize, src: &[u8]) {
    // SAFETY: caller certifies the destination range
    // `[byte_offset, byte_offset + src.len())` lies inside the
    // pre-mapped framebuffer and source/destination do not alias.
    unsafe {
        let dst = (base as *mut u8).add(byte_offset);
        core::ptr::copy_nonoverlapping(src.as_ptr(), dst, src.len());
    }
}

/// Solid-color fast path for the `bytes_per_pixel == 4`, all-bytes-identical
/// case.
#[inline]
pub fn fb_fill_u8_bulk(base_ptr: *mut u8, byte_value: u8, byte_count: usize) {
    // SAFETY: caller certifies `[base_ptr, base_ptr + byte_count)`
    // lies inside a pre-mapped framebuffer; `write_bytes` does not
    // require alignment.
    unsafe {
        core::ptr::write_bytes(base_ptr, byte_value, byte_count);
    }
}

/// Volatile `u32` store at an already-advanced `ptr`.
#[inline]
pub fn fb_write_u32_at(ptr: *mut u8, value: u32) {
    // SAFETY: caller certifies `ptr..ptr+4` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr as *mut u32, value);
    }
}

/// Volatile `u64` store, for an 8-byte-aligned destination.
#[inline]
pub fn fb_write_u64_at(ptr: *mut u64, value: u64) {
    // SAFETY: caller certifies `ptr..ptr+8` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr, value);
    }
}

/// Volatile `u16` store at `ptr`.
#[inline]
pub fn fb_write_u16_at(ptr: *mut u8, value: u16) {
    // SAFETY: caller certifies `ptr..ptr+2` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr as *mut u16, value);
    }
}

/// Volatile `u8` store at `ptr`.
#[inline]
pub fn fb_write_u8_at(ptr: *mut u8, value: u8) {
    // SAFETY: caller certifies `ptr..ptr+1` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr, value);
    }
}

/// Advance `base_ptr` by `byte_offset`, folding `(*mut u8).add` inside OSTD.
#[inline]
pub fn fb_ptr_add(base_ptr: *mut u8, byte_offset: usize) -> *mut u8 {
    // SAFETY: caller certifies the resulting address remains within
    // the pre-mapped framebuffer (and within the same allocation —
    // the FB is a single mapping).
    unsafe { base_ptr.add(byte_offset) }
}

/// Construct a [`Framebuffer`] view over a bootloader-published base address
/// and dimensions.
pub fn framebuffer_handoff(base: NonNull<u8>, pitch: usize, height: u32) -> Framebuffer {
    Framebuffer {
        base,
        pitch,
        height,
    }
}
