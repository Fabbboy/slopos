#![allow(unsafe_op_in_unsafe_fn)]
#![allow(improper_ctypes)]

//! FFI Boundary Layer
//!
//! This module contains ONLY functions that require `extern "C"` linkage because they are:
//! 1. Called from assembly code (limine_entry.s, idt_handlers.s)
//!
//! All other Rust-to-Rust calls should use regular Rust functions without extern "C".

// ============================================================================
// Functions called FROM assembly (must be extern "C")
// ============================================================================

/// Entry point called from limine_entry.s
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main() {
    crate::early_init::kernel_main_impl();
}
#[unsafe(no_mangle)]
pub extern "C" fn common_exception_handler(frame: *mut slopos_arch::InterruptFrame) {
    crate::idt::common_exception_handler_impl(frame);
}

/// Called from ISR assembly when the IRET frame's CS field is not 0x08 or
/// 0x23.  `iret_frame` points at the 5-word IRET frame on the kernel stack:
/// [RIP, CS, RFLAGS, RSP, SS].  We log the corruption and panic instead of
/// taking a triple-fault from a bad IRETQ.
#[unsafe(no_mangle)]
pub extern "C" fn isr_iret_frame_corrupt(iret_frame: *const u64) -> ! {
    // SAFETY: called from ISR assembly which pushes the 5-word IRET
    // frame [RIP, CS, RFLAGS, RSP, SS] at this pointer.
    unsafe { crate::idt::handle_corrupt_iret_frame(iret_frame) }
}

// ============================================================================
// Linker symbols (for boot init sections)
// ============================================================================

// Linker symbols for boot init sections - these are addresses, not function calls
#[allow(improper_ctypes)]
unsafe extern "C" {
    pub static __start_boot_init_early_hw: crate::early_init::BootInitStep;
    pub static __stop_boot_init_early_hw: crate::early_init::BootInitStep;
    pub static __start_boot_init_memory: crate::early_init::BootInitStep;
    pub static __stop_boot_init_memory: crate::early_init::BootInitStep;
    pub static __start_boot_init_drivers: crate::early_init::BootInitStep;
    pub static __stop_boot_init_drivers: crate::early_init::BootInitStep;
    pub static __start_boot_init_services: crate::early_init::BootInitStep;
    pub static __stop_boot_init_services: crate::early_init::BootInitStep;
    pub static __start_boot_init_optional: crate::early_init::BootInitStep;
    pub static __stop_boot_init_optional: crate::early_init::BootInitStep;
    pub static __start_test_registry: slopos_testing::TestDesc;
    pub static __stop_test_registry: slopos_testing::TestDesc;
}
