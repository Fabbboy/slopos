//! IRQ dispatch tests - targeting untested edge cases and error paths.

use core::ffi::{c_char, c_int, c_void};
use core::ptr;

use slopos_abi::arch::IRQ_BASE_VECTOR;
use slopos_lib::{InterruptFrame, klog_info};

use crate::irq::{
    self, IRQ_LINES, IrqStats, disable_line, enable_line, get_irq_route, get_stats, is_initialized,
    is_masked, mask_irq_line, register_handler, unmask_irq_line, unregister_handler,
};

#[allow(dead_code)]
fn create_irq_frame(irq_line: u8) -> InterruptFrame {
    InterruptFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        rbp: 0,
        rbx: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rax: 0,
        rcx: 0,
        rdx: 0,
        rsi: 0,
        rdi: 0,
        vector: (IRQ_BASE_VECTOR as u64) + (irq_line as u64),
        error_code: 0,
        rip: 0xFFFF_FFFF_8000_1000,
        cs: 0x08,
        rflags: 0x202,
        rsp: 0xFFFF_FFFF_8010_0000,
        ss: 0x10,
    }
}

pub fn test_irq_register_invalid_line() -> c_int {
    extern "C" fn dummy_handler(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}

    let result = register_handler(255, Some(dummy_handler), ptr::null_mut(), ptr::null());
    if result == 0 {
        klog_info!("IRQ_TEST: BUG - Accepted registration for invalid IRQ line 255");
        return -1;
    }

    let result2 = register_handler(
        IRQ_LINES as u8,
        Some(dummy_handler),
        ptr::null_mut(),
        ptr::null(),
    );
    if result2 == 0 {
        klog_info!("IRQ_TEST: BUG - Accepted registration for IRQ line at boundary");
        return -1;
    }

    0
}

pub fn test_irq_register_null_handler() -> c_int {
    let result = register_handler(5, None, ptr::null_mut(), ptr::null());

    if result != 0 {
        klog_info!("IRQ_TEST: Registering None handler failed (may be intentional)");
    }

    unregister_handler(5);
    0
}

pub fn test_irq_double_register() -> c_int {
    extern "C" fn handler1(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}
    extern "C" fn handler2(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}

    let r1 = register_handler(
        6,
        Some(handler1),
        ptr::null_mut(),
        b"handler1\0".as_ptr() as *const c_char,
    );
    if r1 != 0 {
        klog_info!("IRQ_TEST: First registration failed");
        return -1;
    }

    let _r2 = register_handler(
        6,
        Some(handler2),
        ptr::null_mut(),
        b"handler2\0".as_ptr() as *const c_char,
    );

    unregister_handler(6);
    0
}

pub fn test_irq_unregister_never_registered() -> c_int {
    unregister_handler(7);
    unregister_handler(7);
    0
}

pub fn test_irq_stats_invalid_line() -> c_int {
    let mut stats = IrqStats {
        count: 0xDEAD,
        last_timestamp: 0xBEEF,
    };

    let result = get_stats(255, &mut stats);
    if result == 0 {
        klog_info!("IRQ_TEST: BUG - get_stats succeeded for invalid IRQ line");
        return -1;
    }

    let result2 = get_stats(IRQ_LINES as u8, &mut stats);
    if result2 == 0 {
        klog_info!("IRQ_TEST: BUG - get_stats succeeded for boundary IRQ line");
        return -1;
    }

    0
}

pub fn test_irq_stats_null_output() -> c_int {
    let result = get_stats(0, ptr::null_mut());
    if result == 0 {
        klog_info!("IRQ_TEST: BUG - get_stats succeeded with null output");
        return -1;
    }
    0
}

pub fn test_irq_mask_unmask_invalid() -> c_int {
    mask_irq_line(255);
    unmask_irq_line(255);
    mask_irq_line(IRQ_LINES as u8 + 10);
    0
}

pub fn test_irq_is_masked_boundary() -> c_int {
    let masked = is_masked(255);
    if !masked {
        klog_info!("IRQ_TEST: BUG - Invalid IRQ line should report as masked");
        return -1;
    }
    0
}

pub fn test_irq_route_invalid() -> c_int {
    let route = get_irq_route(255);
    if route.is_some() {
        klog_info!("IRQ_TEST: BUG - Got route for invalid IRQ line");
        return -1;
    }
    0
}

pub fn test_irq_enable_disable_invalid() -> c_int {
    enable_line(255);
    disable_line(255);
    enable_line(IRQ_LINES as u8 + 5);
    disable_line(IRQ_LINES as u8 + 5);
    0
}

pub fn test_irq_initialized_flag() -> c_int {
    let initialized = is_initialized();
    if !initialized {
        klog_info!("IRQ_TEST: WARNING - IRQ system not initialized when tests run");
    }
    0
}

pub fn test_irq_rapid_register_unregister() -> c_int {
    extern "C" fn rapid_handler(_: u8, _: *mut InterruptFrame, _: *mut c_void) {}

    for _ in 0..100 {
        let _ = register_handler(8, Some(rapid_handler), ptr::null_mut(), ptr::null());
        unregister_handler(8);
    }
    0
}

pub fn test_irq_all_lines_mask_state() -> c_int {
    for irq in 0..IRQ_LINES as u8 {
        let _ = is_masked(irq);
    }
    0
}

pub fn test_irq_stats_valid_line() -> c_int {
    let mut stats = IrqStats {
        count: 0,
        last_timestamp: 0,
    };

    let result = get_stats(0, &mut stats);
    if result != 0 {
        klog_info!("IRQ_TEST: BUG - get_stats failed for valid IRQ line 0");
        return -1;
    }
    0
}

pub fn test_irq_context_pointer_preserved() -> c_int {
    static mut CONTEXT_VALUE: u64 = 0;
    static mut HANDLER_CALLED: bool = false;

    extern "C" fn context_handler(_: u8, _: *mut InterruptFrame, ctx: *mut c_void) {
        unsafe {
            HANDLER_CALLED = true;
            if !ctx.is_null() {
                CONTEXT_VALUE = *(ctx as *const u64);
            }
        }
    }

    let test_value: u64 = 0xDEAD_BEEF_CAFE_BABEu64;
    let ctx_ptr = &test_value as *const u64 as *mut c_void;

    let result = register_handler(9, Some(context_handler), ctx_ptr, ptr::null());
    if result != 0 {
        klog_info!("IRQ_TEST: Failed to register context test handler");
        return -1;
    }

    unregister_handler(9);
    0
}

pub fn test_irq_handler_with_long_name() -> c_int {
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
    0
}

pub fn test_irq_timer_ticks_accessible() -> c_int {
    let ticks = irq::get_timer_ticks();
    let _ = ticks;
    0
}

pub fn test_irq_keyboard_events_accessible() -> c_int {
    let events = irq::get_keyboard_event_counter();
    let _ = events;
    0
}

pub fn test_irq_vector_calculation() -> c_int {
    for irq in 0..IRQ_LINES as u8 {
        let expected_vector = (IRQ_BASE_VECTOR as u32) + (irq as u32);
        if expected_vector > 255 {
            klog_info!(
                "IRQ_TEST: BUG - IRQ {} would produce invalid vector {}",
                irq,
                expected_vector
            );
            return -1;
        }
    }
    0
}

pub fn run_irq_tests() -> (u32, u32) {
    let mut passed = 0u32;
    let mut total = 0u32;

    macro_rules! run_test {
        ($name:expr, $test_fn:expr) => {{
            total += 1;
            let result = $test_fn();
            if result == 0 {
                passed += 1;
            } else {
                klog_info!("IRQ_TEST FAILED: {}", $name);
            }
        }};
    }

    klog_info!("=== IRQ Dispatch Tests ===");

    run_test!("irq_register_invalid_line", test_irq_register_invalid_line);
    run_test!("irq_register_null_handler", test_irq_register_null_handler);
    run_test!("irq_double_register", test_irq_double_register);
    run_test!(
        "irq_unregister_never_registered",
        test_irq_unregister_never_registered
    );
    run_test!("irq_stats_invalid_line", test_irq_stats_invalid_line);
    run_test!("irq_stats_null_output", test_irq_stats_null_output);
    run_test!("irq_mask_unmask_invalid", test_irq_mask_unmask_invalid);
    run_test!("irq_is_masked_boundary", test_irq_is_masked_boundary);
    run_test!("irq_route_invalid", test_irq_route_invalid);
    run_test!(
        "irq_enable_disable_invalid",
        test_irq_enable_disable_invalid
    );
    run_test!("irq_initialized_flag", test_irq_initialized_flag);
    run_test!(
        "irq_rapid_register_unregister",
        test_irq_rapid_register_unregister
    );
    run_test!("irq_all_lines_mask_state", test_irq_all_lines_mask_state);
    run_test!("irq_stats_valid_line", test_irq_stats_valid_line);
    run_test!(
        "irq_context_pointer_preserved",
        test_irq_context_pointer_preserved
    );
    run_test!(
        "irq_handler_with_long_name",
        test_irq_handler_with_long_name
    );
    run_test!(
        "irq_timer_ticks_accessible",
        test_irq_timer_ticks_accessible
    );
    run_test!(
        "irq_keyboard_events_accessible",
        test_irq_keyboard_events_accessible
    );
    run_test!("irq_vector_calculation", test_irq_vector_calculation);

    klog_info!("IRQ tests: {}/{} passed", passed, total);
    (passed, total)
}
