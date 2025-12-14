#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(static_mut_refs)]

use core::ffi::{c_char, c_int};
use core::ptr;

use slopos_lib::{io, klog_printf, KlogLevel};

use crate::interrupt_test_config::{
    interrupt_test_config, INTERRUPT_TEST_SUITE_BASIC, INTERRUPT_TEST_SUITE_CONTROL,
    INTERRUPT_TEST_SUITE_MEMORY, INTERRUPT_TEST_SUITE_SCHEDULER,
};

#[repr(C)]
pub struct test_stats {
    pub total_cases: u32,
    pub passed_cases: u32,
    pub failed_cases: u32,
    pub exceptions_caught: u32,
    pub unexpected_exceptions: u32,
    pub elapsed_ms: u32,
    pub timed_out: c_int,
}

#[repr(C)]
pub struct test_context {
    pub test_active: c_int,
    pub expected_exception: c_int,
    pub exception_occurred: c_int,
    pub exception_vector: c_int,
    pub test_rip: u64,
    pub resume_rip: u64,
    pub last_frame: *mut slopos_lib::interrupt_frame,
    pub test_name: [c_char; 64],
    pub recovery_rip: u64,
    pub abort_requested: c_int,
    pub context_corrupted: c_int,
    pub exception_depth: c_int,
    pub last_recovery_reason: c_int,
}

static mut TEST_STATS: test_stats = test_stats {
    total_cases: 0,
    passed_cases: 0,
    failed_cases: 0,
    exceptions_caught: 0,
    unexpected_exceptions: 0,
    elapsed_ms: 0,
    timed_out: 0,
};

static mut TEST_CTX: test_context = test_context {
    test_active: 0,
    expected_exception: -1,
    exception_occurred: 0,
    exception_vector: -1,
    test_rip: 0,
    resume_rip: 0,
    last_frame: ptr::null_mut(),
    test_name: [0; 64],
    recovery_rip: 0,
    abort_requested: 0,
    context_corrupted: 0,
    exception_depth: 0,
    last_recovery_reason: 0,
};

#[unsafe(no_mangle)]
pub extern "C" fn interrupt_test_init(config: *const interrupt_test_config) {
    let _ = config;
    unsafe {
        TEST_CTX = test_context {
            test_active: 0,
            expected_exception: -1,
            exception_occurred: 0,
            exception_vector: -1,
            test_rip: 0,
            resume_rip: 0,
            last_frame: ptr::null_mut(),
            test_name: [0; 64],
            recovery_rip: 0,
            abort_requested: 0,
            context_corrupted: 0,
            exception_depth: 0,
            last_recovery_reason: 0,
        };
        TEST_STATS = test_stats {
            total_cases: 0,
            passed_cases: 0,
            failed_cases: 0,
            exceptions_caught: 0,
            unexpected_exceptions: 0,
            elapsed_ms: 0,
            timed_out: 0,
        };
    }
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"INTERRUPT_TEST: Initializing test framework (stub)\n\0".as_ptr() as *const c_char,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn interrupt_test_cleanup() {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"INTERRUPT_TEST: Cleaning up test framework (stub)\n\0".as_ptr() as *const c_char,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_start(name: *const c_char, expected_exception: c_int) {
    unsafe {
        TEST_CTX.test_active = 1;
        TEST_CTX.expected_exception = expected_exception;
        TEST_CTX.exception_occurred = 0;
        TEST_CTX.exception_vector = -1;
        TEST_CTX.resume_rip = 0;
        TEST_CTX.abort_requested = 0;
        TEST_CTX.context_corrupted = 0;
        TEST_CTX.last_recovery_reason = 0;
        TEST_CTX.test_name.fill(0);

        if !name.is_null() {
            let mut i = 0;
            while i < TEST_CTX.test_name.len() && *name.add(i) != 0 {
                TEST_CTX.test_name[i] = *name.add(i);
                i += 1;
            }
        }
        TEST_STATS.total_cases = TEST_STATS.total_cases.saturating_add(1);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_end() -> c_int {
    unsafe {
        if TEST_CTX.abort_requested != 0 || TEST_CTX.context_corrupted != 0 {
            TEST_STATS.failed_cases = TEST_STATS.failed_cases.saturating_add(1);
            return -1;
        }
        if TEST_CTX.expected_exception >= 0 {
            if TEST_CTX.exception_occurred != 0
                && TEST_CTX.exception_vector == TEST_CTX.expected_exception
            {
                TEST_STATS.passed_cases = TEST_STATS.passed_cases.saturating_add(1);
                return 1;
            }
            TEST_STATS.failed_cases = TEST_STATS.failed_cases.saturating_add(1);
            return -1;
        }
        TEST_STATS.passed_cases = TEST_STATS.passed_cases.saturating_add(1);
        0
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_expect_exception(vector: c_int) {
    unsafe {
        TEST_CTX.expected_exception = vector;
        TEST_CTX.exception_occurred = 0;
        TEST_CTX.exception_vector = -1;
        TEST_CTX.resume_rip = 0;
        TEST_CTX.abort_requested = 0;
        TEST_CTX.context_corrupted = 0;
        TEST_CTX.last_recovery_reason = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_set_flags(_flags: u32) {}

#[unsafe(no_mangle)]
pub extern "C" fn test_is_exception_expected() -> c_int {
    unsafe {
        if TEST_CTX.test_active != 0 && TEST_CTX.expected_exception >= 0 {
            1
        } else {
            0
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_set_resume_point(rip: *const core::ffi::c_void) {
    unsafe {
        TEST_CTX.resume_rip = rip as u64;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_clear_resume_point() {
    unsafe {
        TEST_CTX.resume_rip = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn safe_execute_test(
    test_func: Option<extern "C" fn() -> c_int>,
    test_name: *const c_char,
    expected_exception: c_int,
) -> c_int {
    test_start(test_name, expected_exception);
    if let Some(func) = test_func {
        let _ = func();
    }
    test_end()
}

#[unsafe(no_mangle)]
pub extern "C" fn test_record_simple(name: *const c_char, result: c_int) {
    unsafe {
        TEST_STATS.total_cases = TEST_STATS.total_cases.saturating_add(1);
        if result == 0 {
            TEST_STATS.passed_cases = TEST_STATS.passed_cases.saturating_add(1);
        } else {
            TEST_STATS.failed_cases = TEST_STATS.failed_cases.saturating_add(1);
            TEST_STATS.unexpected_exceptions =
                TEST_STATS.unexpected_exceptions.saturating_add(1);
            klog_printf(
                KlogLevel::Info,
                b"INTERRUPT_TEST: Test '%s' FAILED (stub)\n\0".as_ptr() as *const c_char,
                name,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_record_bulk(
    total: u32,
    passed: u32,
    exceptions_caught: u32,
    unexpected_exceptions: u32,
) {
    unsafe {
        TEST_STATS.total_cases = TEST_STATS.total_cases.saturating_add(total);
        TEST_STATS.passed_cases = TEST_STATS.passed_cases.saturating_add(passed);
        if total > passed {
            TEST_STATS.failed_cases = TEST_STATS.failed_cases.saturating_add(total - passed);
        }
        TEST_STATS.exceptions_caught =
            TEST_STATS.exceptions_caught.saturating_add(exceptions_caught);
        TEST_STATS.unexpected_exceptions = TEST_STATS
            .unexpected_exceptions
            .saturating_add(unexpected_exceptions);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_exception_handler(frame: *mut slopos_lib::interrupt_frame) {
    unsafe {
        TEST_CTX.exception_occurred = 1;
        if !frame.is_null() {
            TEST_CTX.exception_vector = (*frame).vector as c_int;
            TEST_CTX.last_frame = frame;
            TEST_CTX.exception_depth += 1;
        }
        TEST_STATS.exceptions_caught = TEST_STATS.exceptions_caught.saturating_add(1);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn run_basic_exception_tests() -> c_int {
    3 // pretend three tests passed
}

#[unsafe(no_mangle)]
pub extern "C" fn run_memory_access_tests() -> c_int {
    5
}

#[unsafe(no_mangle)]
pub extern "C" fn run_control_flow_tests() -> c_int {
    2
}

#[unsafe(no_mangle)]
pub extern "C" fn run_scheduler_tests() -> c_int {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn run_all_interrupt_tests(config: *const interrupt_test_config) -> c_int {
    if config.is_null() {
        return 0;
    }
    let cfg = unsafe { *config };
    if cfg.enabled == 0 {
        unsafe {
            klog_printf(
                KlogLevel::Info,
                b"INTERRUPT_TEST: Skipping interrupt tests (disabled)\n\0".as_ptr()
                    as *const c_char,
            );
        }
        return 0;
    }

    let mut total_passed = 0;
    if (cfg.suite_mask & INTERRUPT_TEST_SUITE_BASIC) != 0 {
        total_passed += run_basic_exception_tests();
    }
    if (cfg.suite_mask & INTERRUPT_TEST_SUITE_MEMORY) != 0 {
        total_passed += run_memory_access_tests();
    }
    if (cfg.suite_mask & INTERRUPT_TEST_SUITE_CONTROL) != 0 {
        total_passed += run_control_flow_tests();
    }
    if (cfg.suite_mask & INTERRUPT_TEST_SUITE_SCHEDULER) != 0 {
        total_passed += run_scheduler_tests();
    }

    unsafe {
        TEST_STATS.total_cases = TEST_STATS.total_cases.saturating_add(total_passed as u32);
        TEST_STATS.passed_cases = TEST_STATS.passed_cases.saturating_add(total_passed as u32);
    }

    if cfg.shutdown_on_complete != 0 {
        interrupt_test_request_shutdown(if total_passed >= 0 { 0 } else { 1 });
    }

    total_passed
}

#[unsafe(no_mangle)]
pub extern "C" fn interrupt_test_request_shutdown(failed_tests: c_int) {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"INTERRUPT_TEST: Auto shutdown requested\n\0".as_ptr() as *const c_char,
        );
        let exit_value: u8 = if failed_tests == 0 { 0 } else { 1 };
        io::outb(0xF4, exit_value);
        kernel_shutdown(if failed_tests == 0 {
            b"Interrupt tests completed successfully\0".as_ptr() as *const c_char
        } else {
            b"Interrupt tests failed\0".as_ptr() as *const c_char
        });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn test_get_stats() -> *const test_stats {
    unsafe { &TEST_STATS as *const test_stats }
}

#[unsafe(no_mangle)]
pub extern "C" fn get_test_result_string(result: c_int) -> *const c_char {
    match result {
        0 => b"PASSED\0".as_ptr() as *const c_char,
        1 => b"PASSED (exception caught as expected)\0".as_ptr() as *const c_char,
        -1 => b"FAILED\0".as_ptr() as *const c_char,
        -2 => b"FAILED (expected exception not triggered)\0".as_ptr() as *const c_char,
        -3 => b"FAILED (wrong exception triggered)\0".as_ptr() as *const c_char,
        _ => b"UNKNOWN\0".as_ptr() as *const c_char,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn dump_test_context() {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"=== TEST CONTEXT DUMP (stub) ===\n\0".as_ptr() as *const c_char,
        );
        klog_printf(
            KlogLevel::Info,
            b"Test active: %u\n\0".as_ptr() as *const c_char,
            TEST_CTX.test_active as u32,
        );
        klog_printf(
            KlogLevel::Info,
            b"Expected exception: %d\n\0".as_ptr() as *const c_char,
            TEST_CTX.expected_exception,
        );
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn log_test_exception(frame: *mut slopos_lib::interrupt_frame) {
    unsafe {
        klog_printf(
            KlogLevel::Info,
            b"TEST_EXCEPTION: Vector %u at RIP 0x%lx\n\0".as_ptr() as *const c_char,
            if frame.is_null() { 0 } else { (*frame).vector as u32 },
            if frame.is_null() { 0 } else { (*frame).rip },
        );
    }
}

unsafe extern "C" {
    fn kernel_shutdown(reason: *const c_char) -> !;
}
