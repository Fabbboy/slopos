//! Safe accessors over the anchors `link.ld` and `boot/limine_entry.s`
//! export, and the kernel's only declaration of them.
//!
//! Host builds have no kernel link script, so the `target_os = "none"` gate
//! switches to a synthetic stub backing each anchor.

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
    //! Host stub: fixed offsets into a private BSS buffer, non-null and
    //! ordered `_text_start < _text_end < _kernel_end`, with a distinct
    //! `kernel_stack_top` so non-aliasing tests hold.

    const STUB_SIZE: usize = 4096;
    static STUB: [u8; STUB_SIZE] = [0u8; STUB_SIZE];

    pub(super) fn text_start_ptr() -> *const u8 {
        STUB.as_ptr()
    }
    pub(super) fn text_end_ptr() -> *const u8 {
        STUB.as_ptr().wrapping_add(1024)
    }
    pub(super) fn kernel_start_ptr() -> *const u8 {
        STUB.as_ptr()
    }
    pub(super) fn kernel_end_ptr() -> *const u8 {
        STUB.as_ptr().wrapping_add(2048)
    }
    pub(super) fn kernel_stack_top_ptr() -> *const u8 {
        STUB.as_ptr().wrapping_add(3072)
    }
}

/// `[_text_start, _text_end)` — the kernel's `.text` section.
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

/// Address of the BSP boot stack top — the symbol `kernel_stack_top`
/// declared in `boot/limine_entry.s`, at the top of the 512 KiB BSS stack
/// buffer.
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
