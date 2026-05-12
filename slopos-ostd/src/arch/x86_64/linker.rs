//! Safe accessors over linker-exported anchors.
//!
//! The kernel link script (`link.ld`) exports five anchors that the
//! kernel reads at runtime: `_text_start`, `_text_end`, `_kernel_start`,
//! `_kernel_end`, and `kernel_stack_top` (the latter declared in
//! `boot/limine_entry.s`). Consumers across `boot/`, `core/`, `mm/`,
//! and `hermetic/` historically each spelled out their own
//! `unsafe extern "C" { static _text_start: u8; }` block. This module
//! centralises that into a single `unsafe extern` interior to OSTD;
//! callers go through three safe `pub fn` accessors.
//!
//! Host-side tests cannot resolve the linker symbols (no kernel link
//! script during `cargo test`), so the `target_os = "none"` gate
//! switches to a synthetic stub: a private `static [u8; N]` backs each
//! "linker symbol" so range arithmetic and non-null invariants are
//! exercise-able without the kernel ELF.

use core::ops::Range;

#[cfg(target_os = "none")]
mod kernel_syms {
    unsafe extern "C" {
        pub(super) static _text_start: u8;
        pub(super) static _text_end: u8;
        pub(super) static _kernel_start: u8;
        pub(super) static _kernel_end: u8;
        #[link_name = "kernel_stack_top"]
        pub(super) static kernel_stack_top_impl: u8;
    }
}

#[cfg(not(target_os = "none"))]
mod kernel_syms {
    //! Host-side stub.  Each "linker symbol" is a fixed offset into
    //! a private BSS buffer, giving the accessors stable non-null
    //! addresses ordered as `_text_start < _text_end < _kernel_end`
    //! and a distinct `kernel_stack_top` for non-aliasing tests.

    const STUB_SIZE: usize = 4096;
    static STUB: [u8; STUB_SIZE] = [0u8; STUB_SIZE];

    pub(super) fn text_start_ptr() -> *const u8 {
        STUB.as_ptr()
    }
    pub(super) fn text_end_ptr() -> *const u8 {
        // 1 KiB into the buffer — keeps `text_start < text_end`.
        STUB.as_ptr().wrapping_add(1024)
    }
    pub(super) fn kernel_start_ptr() -> *const u8 {
        STUB.as_ptr()
    }
    pub(super) fn kernel_end_ptr() -> *const u8 {
        // 2 KiB into the buffer — kernel range is a superset of text.
        STUB.as_ptr().wrapping_add(2048)
    }
    pub(super) fn kernel_stack_top_ptr() -> *const u8 {
        // 3 KiB into the buffer — distinct from text/image anchors.
        STUB.as_ptr().wrapping_add(3072)
    }
}

/// `[_text_start, _text_end)` — the kernel's `.text` section.
///
/// Used by the scheduler to validate that a task's RIP falls inside
/// kernel code before dispatching it. The returned pointers are stable
/// for the lifetime of the kernel (BSS-resident link anchors).
pub fn text_range() -> Range<*const u8> {
    #[cfg(target_os = "none")]
    {
        let start: *const u8 = &raw const kernel_syms::_text_start;
        let end: *const u8 = &raw const kernel_syms::_text_end;
        start..end
    }
    #[cfg(not(target_os = "none"))]
    {
        kernel_syms::text_start_ptr()..kernel_syms::text_end_ptr()
    }
}

/// `[_kernel_start, _kernel_end)` — the entire kernel image (`.text`
/// through the early page-table region).
///
/// Used by `mm/` to seed bounds checks for the kernel virtual range.
pub fn kernel_image_range() -> Range<*const u8> {
    #[cfg(target_os = "none")]
    {
        let start: *const u8 = &raw const kernel_syms::_kernel_start;
        let end: *const u8 = &raw const kernel_syms::_kernel_end;
        start..end
    }
    #[cfg(not(target_os = "none"))]
    {
        kernel_syms::kernel_start_ptr()..kernel_syms::kernel_end_ptr()
    }
}

/// Address of the BSP boot stack top (the symbol `kernel_stack_top`
/// declared in `boot/limine_entry.s` at the top of the 512 KiB BSS
/// stack buffer).
///
/// Used by `boot/gdt.rs` to seed `TSS.RSP0` and by the scheduler to
/// fall back to the BSP stack when dispatching kernel-mode tasks that
/// don't own a per-task kernel stack.
pub fn kernel_stack_top() -> *const u8 {
    #[cfg(target_os = "none")]
    {
        &raw const kernel_syms::kernel_stack_top_impl
    }
    #[cfg(not(target_os = "none"))]
    {
        kernel_syms::kernel_stack_top_ptr()
    }
}
