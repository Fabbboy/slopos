//! Per-test execution thunk.
//!
//! The test's intended outcome comes back through a side-channel atomic; the
//! harness is serial on CPU0, so a single global is sound.

use core::sync::atomic::{AtomicU8, Ordering};

use slopos_ostd::catch_panic;

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
