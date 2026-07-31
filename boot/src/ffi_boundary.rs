#![allow(unsafe_op_in_unsafe_fn)]
#![allow(improper_ctypes)]

//! FFI Boundary Layer
//!
//! This module contains ONLY functions that require `extern "C"` linkage because they are:
//! 1. Called from assembly code (limine_entry.s)
//!
//! All other Rust-to-Rust calls should use regular Rust functions without extern "C".

// ============================================================================
// Functions called FROM assembly (must be extern "C")
// ============================================================================

slopos_ostd::extern_c_entry! {
    /// Entry point called from limine_entry.s
    pub fn kernel_main() {
        crate::early_init::kernel_main_impl();
    }
}

slopos_ostd::extern_c_entry! {
    pub fn common_exception_handler(frame: *mut slopos_arch::InterruptFrame) {
        crate::idt::common_exception_handler_impl(frame);
    }
}

slopos_ostd::extern_c_entry! {
    /// Called from ISR assembly when the IRET frame's CS field is not
    /// 0x08 or 0x23. `iret_frame` points at the 5-word IRET frame on
    /// the kernel stack: [RIP, CS, RFLAGS, RSP, SS]. We log the
    /// corruption and panic instead of taking a triple-fault from a
    /// bad IRETQ.
    pub fn isr_iret_frame_corrupt(iret_frame: *const u64) -> ! {
        crate::idt::handle_corrupt_iret_frame(iret_frame)
    }
}

// ============================================================================
