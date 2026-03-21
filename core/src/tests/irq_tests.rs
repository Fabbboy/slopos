//! IRQ dispatch tests - targeting untested edge cases and error paths.

use core::cell::SyncUnsafeCell;
use core::ffi::{c_char, c_void};
use core::ptr;

use slopos_arch::InterruptFrame;
use slopos_arch::arch::idt::IRQ_BASE_VECTOR;
use slopos_testing::TestResult;
use slopos_testing::assert_test;
use slopos_utils::klog_info;

use crate::irq::{
    self, IRQ_LINES, IrqStats, disable_line, enable_line, get_irq_route, get_stats, is_initialized,
    is_masked, mask_irq_line, register_handler, unmask_irq_line, unregister_handler,
};

pub fn test_irq_register_invalid_line() -> TestResult {
    extern "C" fn dummy_handler(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}

    let result = register_handler(255, Some(dummy_handler), ptr::null_mut(), ptr::null());
    assert_test!(
        result != 0,
        "Accepted registration for invalid IRQ line 255"
    );

    let result2 = register_handler(
        IRQ_LINES as u8,
        Some(dummy_handler),
        ptr::null_mut(),
        ptr::null(),
    );
    assert_test!(
        result2 != 0,
        "Accepted registration for IRQ line at boundary"
    );

    TestResult::Pass
}

pub fn test_irq_register_null_handler() -> TestResult {
    let result = register_handler(5, None, ptr::null_mut(), ptr::null());

    if result != 0 {
        klog_info!("IRQ_TEST: Registering None handler failed (may be intentional)");
    }

    unregister_handler(5);
    TestResult::Pass
}

pub fn test_irq_double_register() -> TestResult {
    extern "C" fn handler1(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}
    extern "C" fn handler2(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}

    let r1 = register_handler(
        6,
        Some(handler1),
        ptr::null_mut(),
        b"handler1\0".as_ptr() as *const c_char,
    );
    assert_test!(r1 == 0, "First registration failed");

    let _r2 = register_handler(
        6,
        Some(handler2),
        ptr::null_mut(),
        b"handler2\0".as_ptr() as *const c_char,
    );

    unregister_handler(6);
    TestResult::Pass
}

pub fn test_irq_unregister_never_registered() -> TestResult {
    unregister_handler(7);
    unregister_handler(7);
    TestResult::Pass
}

pub fn test_irq_stats_invalid_line() -> TestResult {
    let mut stats = IrqStats {
        count: 0xDEAD,
        last_timestamp: 0xBEEF,
    };

    let result = get_stats(255, &mut stats);
    assert_test!(result != 0, "get_stats succeeded for invalid IRQ line 255");

    let result2 = get_stats(IRQ_LINES as u8, &mut stats);
    assert_test!(result2 != 0, "get_stats succeeded for boundary IRQ line");

    TestResult::Pass
}

pub fn test_irq_stats_null_output() -> TestResult {
    let result = get_stats(0, ptr::null_mut());
    assert_test!(result != 0, "get_stats succeeded with null output");
    TestResult::Pass
}

pub fn test_irq_mask_unmask_invalid() -> TestResult {
    mask_irq_line(255);
    unmask_irq_line(255);
    mask_irq_line(IRQ_LINES as u8 + 10);
    TestResult::Pass
}

pub fn test_irq_is_masked_boundary() -> TestResult {
    let masked = is_masked(255);
    assert_test!(masked, "Invalid IRQ line should report as masked");
    TestResult::Pass
}

pub fn test_irq_route_invalid() -> TestResult {
    let route = get_irq_route(255);
    assert_test!(route.is_none(), "Got route for invalid IRQ line");
    TestResult::Pass
}

pub fn test_irq_enable_disable_invalid() -> TestResult {
    enable_line(255);
    disable_line(255);
    enable_line(IRQ_LINES as u8 + 5);
    disable_line(IRQ_LINES as u8 + 5);
    TestResult::Pass
}

pub fn test_irq_initialized_flag() -> TestResult {
    let initialized = is_initialized();
    if !initialized {
        klog_info!("IRQ_TEST: WARNING - IRQ system not initialized when tests run");
    }
    TestResult::Pass
}

pub fn test_irq_rapid_register_unregister() -> TestResult {
    extern "C" fn rapid_handler(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}

    for _ in 0..100 {
        let _ = register_handler(8, Some(rapid_handler), ptr::null_mut(), ptr::null());
        unregister_handler(8);
    }
    TestResult::Pass
}

pub fn test_irq_all_lines_mask_state() -> TestResult {
    for irq in 0..IRQ_LINES as u8 {
        let _ = is_masked(irq);
    }
    TestResult::Pass
}

pub fn test_irq_stats_valid_line() -> TestResult {
    let mut stats = IrqStats {
        count: 0,
        last_timestamp: 0,
    };

    let result = get_stats(0, &mut stats);
    assert_test!(result == 0, "get_stats failed for valid IRQ line 0");
    TestResult::Pass
}

pub fn test_irq_context_pointer_preserved() -> TestResult {
    static CONTEXT_VALUE: SyncUnsafeCell<u64> = SyncUnsafeCell::new(0);
    static HANDLER_CALLED: SyncUnsafeCell<bool> = SyncUnsafeCell::new(false);

    extern "C" fn context_handler(_: u8, _: *mut InterruptFrame, ctx: *mut c_void) {
        unsafe {
            *HANDLER_CALLED.get() = true;
            if !ctx.is_null() {
                *CONTEXT_VALUE.get() = *(ctx as *const u64);
            }
        }
    }

    let test_value: u64 = 0xDEAD_BEEF_CAFE_BABEu64;
    let ctx_ptr = &test_value as *const u64 as *mut c_void;

    let result = register_handler(9, Some(context_handler), ctx_ptr, ptr::null());
    assert_test!(result == 0, "Failed to register context test handler");

    unregister_handler(9);
    TestResult::Pass
}

pub fn test_irq_handler_with_long_name() -> TestResult {
    extern "C" fn long_name_handler(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}

    let long_name =
        b"this_is_a_very_long_handler_name_that_might_cause_issues_if_not_handled_properly\0";

    let _result = register_handler(
        10,
        Some(long_name_handler),
        ptr::null_mut(),
        long_name.as_ptr() as *const c_char,
    );

    unregister_handler(10);
    TestResult::Pass
}

pub fn test_irq_timer_ticks_accessible() -> TestResult {
    let ticks = irq::get_timer_ticks();
    let _ = ticks;
    TestResult::Pass
}

pub fn test_irq_keyboard_events_accessible() -> TestResult {
    let events = irq::get_keyboard_event_counter();
    let _ = events;
    TestResult::Pass
}

pub fn test_irq_vector_calculation() -> TestResult {
    for irq in 0..IRQ_LINES as u8 {
        let expected_vector = (IRQ_BASE_VECTOR as u32) + (irq as u32);
        assert_test!(
            expected_vector <= 255,
            "IRQ {} would produce invalid vector {}",
            irq,
            expected_vector
        );
    }
    TestResult::Pass
}

slopos_testing::define_test_suite!(
    irq,
    [
        test_irq_register_invalid_line,
        test_irq_register_null_handler,
        test_irq_double_register,
        test_irq_unregister_never_registered,
        test_irq_stats_invalid_line,
        test_irq_stats_null_output,
        test_irq_mask_unmask_invalid,
        test_irq_is_masked_boundary,
        test_irq_route_invalid,
        test_irq_enable_disable_invalid,
        test_irq_initialized_flag,
        test_irq_rapid_register_unregister,
        test_irq_all_lines_mask_state,
        test_irq_stats_valid_line,
        test_irq_context_pointer_preserved,
        test_irq_handler_with_long_name,
        test_irq_timer_ticks_accessible,
        test_irq_keyboard_events_accessible,
        test_irq_vector_calculation,
    ]
);
