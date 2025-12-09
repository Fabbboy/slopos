#![no_std]
#![feature(lang_items)]
#![allow(internal_features)]

//! SlopOS Graphics Subsystem - Rust Implementation
//!
//! This crate provides the graphics rendering stack for SlopOS,
//! replacing the C graphics implementation with a safer Rust alternative.

use core::panic::PanicInfo;

// Import C bindings (kernel functions we can call from Rust)
use bindings as c;

/// Test function to verify FFI works - callable from C
///
/// This adds two numbers together and returns the result.
/// Use this to verify the Rust<->C FFI boundary is working correctly.
#[unsafe(no_mangle)]
pub extern "C" fn rust_graphics_test_add(a: u32, b: u32) -> u32 {
    a.wrapping_add(b)
}

/// Initialize the Rust graphics subsystem
///
/// Call this from C during boot to initialize the Rust graphics stack.
/// Returns 0 on success, -1 on failure.
#[unsafe(no_mangle)]
pub extern "C" fn rust_graphics_init() -> i32 {
    // TODO: Implement graphics initialization
    0
}

/// Get the version of the Rust graphics subsystem
///
/// Returns a version number that can be checked from C.
#[unsafe(no_mangle)]
pub extern "C" fn rust_graphics_version() -> u32 {
    1 // Version 0.0.1
}

/// Test log function that can trigger a panic
///
/// This demonstrates calling from C -> Rust -> log -> panic! -> kernel_panic
/// Pass should_panic=1 to trigger a panic for testing.
#[unsafe(no_mangle)]
pub extern "C" fn rust_graphics_test_log(level: u32, should_panic: u32) {
    // Log message using C klog through FFI
    unsafe {
        let msg = b"Rust graphics test log called\0";
        c::klog_printf(level, msg.as_ptr() as *const i8);
    }

    // Trigger panic if requested (for testing panic handler)
    if should_panic != 0 {
        panic!("Rust graphics test panic triggered!");
    }
}

/// Panic handler for no_std environment
///
/// In a freestanding environment, we need to provide our own panic handler.
/// This calls into the kernel's panic mechanism through FFI.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // Call kernel_panic through FFI bindings
    unsafe {
        let msg = b"Rust graphics panic\0";
        c::kernel_panic(msg.as_ptr() as *const i8);
    }
    // kernel_panic never returns, but we need this for type checking
    loop {
        core::hint::spin_loop();
    }
}

/// Language item for eh_personality (required for panic handling)
#[lang = "eh_personality"]
extern "C" fn eh_personality() {}
