//! Test harness shutdown support
//!
//! This module provides the QEMU exit mechanism for the test harness.
//! The actual interrupt/exception testing infrastructure was removed as it
//! contained only stub implementations. When real interrupt tests are needed,
//! they should be implemented properly rather than returning fake results.

use core::ffi::c_char;

use slopos_lib::klog_info;
use slopos_lib::ports::QEMU_DEBUG_EXIT;

use slopos_core::platform;

/// Request test harness shutdown via QEMU debug exit port.
///
/// This writes to the isa-debug-exit device to terminate QEMU with an exit code
/// indicating test success (0) or failure (1). The actual exit code seen by the
/// shell will be `(value << 1) | 1`, so 0 becomes 1 (success) and 1 becomes 3 (failure).
pub fn interrupt_test_request_shutdown(failed_tests: i32) {
    klog_info!("TEST: Requesting shutdown (failed={})", failed_tests);
    let exit_value: u8 = if failed_tests == 0 { 0 } else { 1 };
    unsafe { QEMU_DEBUG_EXIT.write(exit_value) };
    platform::kernel_shutdown(if failed_tests == 0 {
        b"Tests completed successfully\0".as_ptr() as *const c_char
    } else {
        b"Tests failed\0".as_ptr() as *const c_char
    });
}
