//! Per-test execution thunk.
//!
//! `execute_test` wraps a kernel-test function in `catch_panic!` and
//! recovers the test's intended outcome through a side-channel atomic.
//! The harness is serial on CPU0, so a single global is sound.

use core::sync::atomic::{AtomicU8, Ordering};

use slopos_utils::catch_panic;

use crate::result::TestResult;

static LAST_OUTCOME: AtomicU8 = AtomicU8::new(0);

/// Run `f` under `catch_panic!`. Returns `TestResult::Panic` if `f`
/// panic-longjmps; otherwise the value `f` returned.
pub fn execute_test(f: fn() -> TestResult) -> TestResult {
    LAST_OUTCOME.store(TestResult::Panic.as_u8(), Ordering::Relaxed);
    let _rc: i32 = catch_panic!({
        let r = f();
        LAST_OUTCOME.store(r.as_u8(), Ordering::Relaxed);
        r.to_c_int()
    });
    TestResult::from_u8(LAST_OUTCOME.load(Ordering::Relaxed))
}
