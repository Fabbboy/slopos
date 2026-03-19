use super::*;

// ===========================================================================
// TTY Table tests
// ===========================================================================

/// After tty_table_init, TTY 0 and TTY 1 are allocated.
pub fn test_table_init_allocates_tty0_and_tty1() -> TestResult {
    // Ensure init has been called (it's idempotent — re-calling overwrites).
    tty::table::tty_table_init();

    let slot0 = TTY_SLOTS[0].lock();
    if slot0.is_none() {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated after init");
        return TestResult::Fail;
    }
    drop(slot0);
    let slot1 = TTY_SLOTS[1].lock();
    if slot1.is_none() {
        klog_info!("TTY_TEST: BUG - TTY 1 not allocated after init");
        return TestResult::Fail;
    }
    drop(slot1);
    // Slots 2..MAX_TTYS should be None.
    for i in 2..tty::MAX_TTYS {
        let slot = TTY_SLOTS[i].lock();
        if slot.is_some() {
            klog_info!("TTY_TEST: BUG - TTY {} unexpectedly allocated", i);
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// TTY 0 has the correct index.
pub fn test_table_tty0_has_index_zero() -> TestResult {
    tty::table::tty_table_init();

    let guard = TTY_SLOTS[0].lock();
    if let Some(tty) = guard.as_ref() {
        if tty.index != TtyIndex(0) {
            klog_info!("TTY_TEST: BUG - TTY 0 has wrong index {:?}", tty.index);
            return TestResult::Fail;
        }
    } else {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// TTY 0 is allocated by default (slot being Some is the active state).
pub fn test_table_tty0_active() -> TestResult {
    tty::table::tty_table_init();

    let guard = TTY_SLOTS[0].lock();
    if guard.is_some() {
        // Slot is allocated — active.
    } else {
        klog_info!("TTY_TEST: BUG - TTY 0 not allocated");
        return TestResult::Fail;
    }
    TestResult::Pass
}

/// with_tty helper works for existing TTY.
pub fn test_table_with_tty_exists() -> TestResult {
    tty::table::tty_table_init();

    let result = tty::table::with_tty(TtyIndex(0), |tty| tty.index);
    match result {
        Some(idx) => {
            if idx != TtyIndex(0) {
                klog_info!("TTY_TEST: BUG - with_tty returned wrong index");
                return TestResult::Fail;
            }
        }
        None => {
            klog_info!("TTY_TEST: BUG - with_tty returned None for TTY 0");
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

/// with_tty helper returns None for empty slot.
pub fn test_table_with_tty_empty() -> TestResult {
    tty::table::tty_table_init();

    let result = tty::table::with_tty(TtyIndex(5), |_tty| ());
    if result.is_some() {
        klog_info!("TTY_TEST: BUG - with_tty returned Some for empty slot 5");
        return TestResult::Fail;
    }
    TestResult::Pass
}
