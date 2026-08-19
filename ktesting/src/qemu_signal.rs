//! QEMU exit mechanism for the test harness.

use core::ffi::c_char;

use slopos_ostd::klog_info;

/// Request test harness shutdown via QEMU debug exit port.
///
/// isa-debug-exit turns the written value into a shell exit code of
/// `(value << 1) | 1`, so success exits 1 and failure exits 3.
pub fn qemu_signal_exit(failed_tests: i32) {
    klog_info!("TESTS: Requesting shutdown (failed={})", failed_tests);
    let exit_value: u8 = if failed_tests == 0 { 0 } else { 1 };
    slopos_ostd::io::qemu_debug_exit(exit_value);
    // Kernel-initiated: the harness is the kernel deciding the run is over,
    // with no syscall caller and no credential to check.
    let cap = slopos_ostd::platform::power::kernel_authority();
    slopos_ostd::platform::power::shutdown(
        &cap,
        if failed_tests == 0 {
            b"Tests completed successfully\0".as_ptr() as *const c_char
        } else {
            b"Tests failed\0".as_ptr() as *const c_char
        },
    );
}
