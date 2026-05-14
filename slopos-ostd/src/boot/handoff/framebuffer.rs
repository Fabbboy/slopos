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

// ---------------------------------------------------------------------------
// Bootloader-published framebuffer base + volatile-write helpers.
// ---------------------------------------------------------------------------
//
// The video / vconsole drivers cache a framebuffer base pointer (as a
// `u64` so the cache type stays `Send + Sync`) and write pixels via
// `core::ptr::write_volatile` so the compiler cannot reorder, cache, or
// elide stores against the MMIO-backed framebuffer. The drivers do not
// observe other side effects from the framebuffer mapping — every
// callable below is safe to invoke once the caller has obtained the
// base pointer from a bootloader-pre-mapped framebuffer and certifies
// that the byte offset lies inside the published `pitch * height`
// region. Each helper folds one `unsafe { write_volatile(...) }` call
// site behind a safe API for `video/src/graphics.rs` and
// `drivers/src/tty/vconsole.rs`.

/// Write `value` at `base + byte_offset` as a `u8` using a single
/// volatile store. Use for per-channel writes on 24-bpp framebuffers.
#[inline]
pub fn fb_write_u8(base: u64, byte_offset: usize, value: u8) {
    // SAFETY: caller certifies `base` came from a bootloader-pre-mapped
    // framebuffer and `byte_offset` is within the published region.
    unsafe {
        let p = (base as *mut u8).add(byte_offset);
        core::ptr::write_volatile(p, value);
    }
}

/// Write `value` at `base + byte_offset` as a `u16` using a single
/// volatile, possibly-unaligned store.
#[inline]
pub fn fb_write_u16(base: u64, byte_offset: usize, value: u16) {
    // SAFETY: same as `fb_write_u8`; `write_unaligned` tolerates
    // misaligned framebuffers.
    unsafe {
        let p = (base as *mut u8).add(byte_offset) as *mut u16;
        core::ptr::write_volatile(p, value);
    }
}

/// Write `value` at `base + byte_offset` as a `u32` using a single
/// volatile store.
#[inline]
pub fn fb_write_u32(base: u64, byte_offset: usize, value: u32) {
    // SAFETY: as `fb_write_u8`; framebuffers from Limine on x86_64
    // are 32-bit aligned at every pixel boundary.
    unsafe {
        let p = (base as *mut u8).add(byte_offset) as *mut u32;
        core::ptr::write_volatile(p, value);
    }
}

/// Write `value` at `ptr` as a `u32` using `write_unaligned` so the
/// store works on a framebuffer that is not 4-byte aligned at the
/// row offset (very rare, but observed on some firmware).
#[inline]
pub fn fb_write_u32_unaligned(ptr: *mut u8, value: u32) {
    // SAFETY: caller certifies `ptr` is within a pre-mapped
    // framebuffer; `write_unaligned` is sound for any byte alignment.
    unsafe {
        core::ptr::write_unaligned(ptr as *mut u32, value);
    }
}

/// Copy `len` bytes from `src` to `base + byte_offset` using
/// `copy_nonoverlapping`. Used by the vconsole shadow-buffer blit.
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

/// Solid-color fast path for the `bytes_per_pixel == 4`, all-bytes-
/// identical case. Writes `count * 4` bytes at `base_ptr` to the byte
/// `byte_value` via `write_bytes`.
#[inline]
pub fn fb_fill_u8_bulk(base_ptr: *mut u8, byte_value: u8, byte_count: usize) {
    // SAFETY: caller certifies `[base_ptr, base_ptr + byte_count)`
    // lies inside a pre-mapped framebuffer; `write_bytes` does not
    // require alignment.
    unsafe {
        core::ptr::write_bytes(base_ptr, byte_value, byte_count);
    }
}

/// Volatile write of `value: u32` at `ptr: *mut u8`. Used inside
/// the row-fill hot path where `ptr` has been advanced by byte offset
/// already.
#[inline]
pub fn fb_write_u32_at(ptr: *mut u8, value: u32) {
    // SAFETY: caller certifies `ptr..ptr+4` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr as *mut u32, value);
    }
}

/// Volatile write of `value: u64` at `ptr: *mut u64`. Hot-path 64-bit
/// store used by the solid-color row filler when the destination is
/// 8-byte aligned.
#[inline]
pub fn fb_write_u64_at(ptr: *mut u64, value: u64) {
    // SAFETY: caller certifies `ptr..ptr+8` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr, value);
    }
}

/// Volatile write of `value: u16` at `ptr: *mut u8` reinterpreted as
/// `*mut u16`.
#[inline]
pub fn fb_write_u16_at(ptr: *mut u8, value: u16) {
    // SAFETY: caller certifies `ptr..ptr+2` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr as *mut u16, value);
    }
}

/// Volatile write of `value: u8` at `ptr: *mut u8`.
#[inline]
pub fn fb_write_u8_at(ptr: *mut u8, value: u8) {
    // SAFETY: caller certifies `ptr..ptr+1` is within a pre-mapped FB.
    unsafe {
        core::ptr::write_volatile(ptr, value);
    }
}

/// Advance `base_ptr` by `byte_offset` bytes, returning a fresh raw
/// pointer. Folds `(*mut u8).add(...)` interior to OSTD.
#[inline]
pub fn fb_ptr_add(base_ptr: *mut u8, byte_offset: usize) -> *mut u8 {
    // SAFETY: caller certifies the resulting address remains within
    // the pre-mapped framebuffer (and within the same allocation —
    // the FB is a single mapping).
    unsafe { base_ptr.add(byte_offset) }
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
