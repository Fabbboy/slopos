//! Test harness shutdown support
//!
//! This module provides the QEMU exit mechanism for the test harness.

use core::ffi::c_char;

use slopos_kernel_services::platform;
use slopos_ostd::io::port_consts::QEMU_DEBUG_EXIT;
use slopos_ostd::klog_info;

/// Request test harness shutdown via QEMU debug exit port.
///
/// This writes to the isa-debug-exit device to terminate QEMU with an exit code
/// indicating test success (0) or failure (1). The actual exit code seen by the
/// shell will be `(value << 1) | 1`, so 0 becomes 1 (success) and 1 becomes 3 (failure).
pub fn qemu_signal_exit(failed_tests: i32) {
    klog_info!("TESTS: Requesting shutdown (failed={})", failed_tests);
    let exit_value: u8 = if failed_tests == 0 { 0 } else { 1 };
    unsafe { QEMU_DEBUG_EXIT.write(exit_value) };
    platform::kernel_shutdown(if failed_tests == 0 {
        b"Tests completed successfully\0".as_ptr() as *const c_char
    } else {
        b"Tests failed\0".as_ptr() as *const c_char
    });
}
