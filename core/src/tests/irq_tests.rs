//! Kernel-side IRQ surface tests.

use core::sync::atomic::{AtomicUsize, Ordering};

use slopos_ostd::irq::{IRQ_BASE_VECTOR, IrqAllocator, IrqContext, IrqError, dispatch};
use slopos_testing::TestResult;
use slopos_testing::assert_test;

use crate::irq::{
    IRQ_LINES, disable_line, enable_line, get_irq_route, get_keyboard_event_counter,
    get_timer_ticks, increment_keyboard_events, increment_timer_ticks, is_initialized, is_masked,
    mask_irq_line, set_irq_route, unmask_irq_line,
};

pub fn test_irq_route_set_get_round_trip() -> TestResult {
    set_irq_route(5, 42);
    let r = get_irq_route(5).expect("in-range route");
    assert_test!(
        r.via_ioapic,
        "via_ioapic should be true after set_irq_route"
    );
    assert_test!(r.gsi == 42, "GSI mismatch");
    TestResult::Pass
}

pub fn test_irq_route_invalid_line() -> TestResult {
    let r = get_irq_route(IRQ_LINES as u8);
    assert_test!(r.is_none(), "Out-of-range get_irq_route should return None");
    set_irq_route(IRQ_LINES as u8, 99); // must not panic
    TestResult::Pass
}

pub fn test_irq_is_masked_boundary() -> TestResult {
    assert_test!(
        is_masked(IRQ_LINES as u8),
        "Out-of-range is_masked must return true (refusal)"
    );
    assert_test!(
        is_masked(255),
        "Vector 255 is way out of range, must report masked"
    );
    TestResult::Pass
}

pub fn test_irq_mask_unmask_no_route() -> TestResult {
    // Line 7 is one the boot path never programs; PS/2 init takes 1, 4 and 12.
    unmask_irq_line(7);
    assert_test!(!is_masked(7), "Line should be unmasked");
    mask_irq_line(7);
    assert_test!(is_masked(7), "Line should be masked");
    TestResult::Pass
}

pub fn test_irq_enable_disable_invalid_line() -> TestResult {
    enable_line(IRQ_LINES as u8); // must not panic
    disable_line(IRQ_LINES as u8); // must not panic
    TestResult::Pass
}

pub fn test_irq_initialized_flag_true() -> TestResult {
    assert_test!(is_initialized(), "is_initialized should always return true");
    TestResult::Pass
}

pub fn test_irq_timer_ticks_increment() -> TestResult {
    let before = get_timer_ticks();
    increment_timer_ticks();
    increment_timer_ticks();
    increment_timer_ticks();
    let after = get_timer_ticks();
    assert_test!(after >= before + 3, "Timer tick counter must increment");
    TestResult::Pass
}

pub fn test_irq_keyboard_events_increment() -> TestResult {
    let before = get_keyboard_event_counter();
    increment_keyboard_events();
    increment_keyboard_events();
    let after = get_keyboard_event_counter();
    assert_test!(after >= before + 2, "Keyboard event counter must increment");
    TestResult::Pass
}

pub fn test_irq_timer_ticks_accessible() -> TestResult {
    let _ = get_timer_ticks();
    TestResult::Pass
}

pub fn test_irq_keyboard_events_accessible() -> TestResult {
    let _ = get_keyboard_event_counter();
    TestResult::Pass
}

pub fn test_irq_vector_calculation() -> TestResult {
    assert_test!(
        IRQ_BASE_VECTOR.wrapping_add(0) == 32,
        "IRQ0 maps to vector 32"
    );
    assert_test!(
        IRQ_BASE_VECTOR.wrapping_add(1) == 33,
        "IRQ1 (keyboard) maps to vector 33"
    );
    assert_test!(
        IRQ_BASE_VECTOR.wrapping_add(12) == 44,
        "IRQ12 (mouse) maps to vector 44"
    );
    assert_test!(
        IRQ_BASE_VECTOR.wrapping_add(15) == 47,
        "IRQ15 maps to vector 47"
    );
    TestResult::Pass
}

pub fn test_ostd_alloc_returns_in_range() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    assert_test!(v >= 32, "Allocated vector below 32");
    assert_test!(v < 224, "Allocated vector at or above 224");
    TestResult::Pass
}

pub fn test_ostd_alloc_distinct_vectors() -> TestResult {
    let a = IrqAllocator::alloc().expect("a");
    let b = IrqAllocator::alloc().expect("b");
    assert_test!(a.vector() != b.vector(), "Two allocs returned same vector");
    TestResult::Pass
}

pub fn test_ostd_alloc_drop_releases() -> TestResult {
    let v = {
        let line = IrqAllocator::alloc().expect("alloc");
        line.vector()
    };
    // Exact reuse is not asserted, only that the Drop path does not panic.
    assert_test!(v >= 32, "vector still in range");
    TestResult::Pass
}

pub fn test_ostd_reserve_specific_double_claim_refused() -> TestResult {
    // Pick a vector that's almost certainly free (high MSI range).
    let v = 200u8;
    let line = match IrqAllocator::reserve_specific(v) {
        Ok(l) => l,
        Err(_) => return TestResult::Pass, // already taken — test inert
    };
    let r = IrqAllocator::reserve_specific(v);
    assert_test!(
        matches!(r, Err(IrqError::AlreadyRegistered)),
        "Double reserve_specific must fail with AlreadyRegistered"
    );
    drop(line);
    TestResult::Pass
}

pub fn test_ostd_reserve_specific_out_of_range() -> TestResult {
    assert_test!(
        matches!(IrqAllocator::reserve_specific(31), Err(IrqError::Exhausted)),
        "vector 31 is below ALLOC_VECTOR_BASE"
    );
    assert_test!(
        matches!(
            IrqAllocator::reserve_specific(224),
            Err(IrqError::Exhausted)
        ),
        "vector 224 is at ALLOC_VECTOR_END"
    );
    TestResult::Pass
}

static DISPATCH_COUNTER: AtomicUsize = AtomicUsize::new(0);

pub fn test_ostd_register_callback_then_dispatch() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    DISPATCH_COUNTER.store(0, Ordering::SeqCst);
    let handle = line
        .register_callback(|ctx: &IrqContext<'_>| {
            assert!(ctx.error_code() == 0xCAFE);
            DISPATCH_COUNTER.fetch_add(1, Ordering::SeqCst);
        })
        .expect("register");
    dispatch(v, 0xCAFE);
    dispatch(v, 0xCAFE);
    drop(handle);
    drop(line);
    assert_test!(
        DISPATCH_COUNTER.load(Ordering::SeqCst) == 2,
        "Callback should have fired twice"
    );
    TestResult::Pass
}

pub fn test_ostd_double_register_callback_errors() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let _h = line.register_callback(|_| {}).expect("first");
    let r = line.register_callback(|_| {});
    assert_test!(
        matches!(r, Err(IrqError::AlreadyRegistered)),
        "Second register_callback on same line must fail"
    );
    TestResult::Pass
}

pub fn test_ostd_handle_drop_clears_dispatch() -> TestResult {
    let line = IrqAllocator::alloc().expect("alloc");
    let v = line.vector();
    DISPATCH_COUNTER.store(0, Ordering::SeqCst);
    {
        let _h = line
            .register_callback(|_| {
                DISPATCH_COUNTER.fetch_add(1, Ordering::SeqCst);
            })
            .expect("register");
    }
    dispatch(v, 0);
    assert_test!(
        DISPATCH_COUNTER.load(Ordering::SeqCst) == 0,
        "Dispatch slot should have been cleared on handle drop"
    );
    TestResult::Pass
}

pub fn test_ostd_dispatch_to_unregistered_vector_is_noop() -> TestResult {
    // 123 has no registered callback.
    dispatch(123, 0);
    TestResult::Pass
}

slopos_testing::stest!(name = test_irq_route_set_get_round_trip, suite = irq);
slopos_testing::stest!(name = test_irq_route_invalid_line, suite = irq);
slopos_testing::stest!(name = test_irq_is_masked_boundary, suite = irq);
slopos_testing::stest!(name = test_irq_mask_unmask_no_route, suite = irq);
slopos_testing::stest!(name = test_irq_enable_disable_invalid_line, suite = irq);
slopos_testing::stest!(name = test_irq_initialized_flag_true, suite = irq);
slopos_testing::stest!(name = test_irq_timer_ticks_increment, suite = irq);
slopos_testing::stest!(name = test_irq_keyboard_events_increment, suite = irq);
slopos_testing::stest!(name = test_irq_timer_ticks_accessible, suite = irq);
slopos_testing::stest!(name = test_irq_keyboard_events_accessible, suite = irq);
slopos_testing::stest!(name = test_irq_vector_calculation, suite = irq);
slopos_testing::stest!(name = test_ostd_alloc_returns_in_range, suite = irq);
slopos_testing::stest!(name = test_ostd_alloc_distinct_vectors, suite = irq);
slopos_testing::stest!(name = test_ostd_alloc_drop_releases, suite = irq);
slopos_testing::stest!(
    name = test_ostd_reserve_specific_double_claim_refused,
    suite = irq
);
slopos_testing::stest!(name = test_ostd_reserve_specific_out_of_range, suite = irq);
slopos_testing::stest!(
    name = test_ostd_register_callback_then_dispatch,
    suite = irq
);
slopos_testing::stest!(
    name = test_ostd_double_register_callback_errors,
    suite = irq
);
slopos_testing::stest!(name = test_ostd_handle_drop_clears_dispatch, suite = irq);
slopos_testing::stest!(
    name = test_ostd_dispatch_to_unregistered_vector_is_noop,
    suite = irq
);
