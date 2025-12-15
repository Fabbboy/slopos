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
pub extern "C" fn common_exception_handler(frame: *mut slopos_lib::InterruptFrame) {
    crate::idt::common_exception_handler_impl(frame);
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
}
