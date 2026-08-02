//! Fault-injection tests for the task-scoped panic-recovery contract.
//!
//! A caught panic must run `Drop` cleanups during the unwind, restore the
//! recovery depth, leave held locks reacquirable, and surface the panic
//! reason through [`OopsInfo`]. The oops ledger's counting/limit arithmetic
//! is pinned here as well; the escalation path itself (limit-crossing panic
//! becomes fatal) halts the machine, so it is covered by the
//! `panic.oops_limit=1 panic.recover_smoke=on` boot-log check instead.

use core::sync::atomic::{AtomicBool, Ordering};
use slopos_ostd::lock_class;

use slopos_ostd::panic_recovery;
use slopos_ostd::sync::{LOCK_LEVEL_UNORDERED, SpinLock};
use slopos_testing::{TestResult, assert_test};

static RECOVERY_LOCK: SpinLock<u32> =
    SpinLock::new(0, lock_class!("test.recovery_lock", LOCK_LEVEL_UNORDERED));
static CANARY_DROPPED: AtomicBool = AtomicBool::new(false);

struct UnwindCanary;

impl Drop for UnwindCanary {
    fn drop(&mut self) {
        CANARY_DROPPED.store(true, Ordering::SeqCst);
    }
}

/// A panic inside `run_recoverable` is caught, runs `Drop` during the
/// unwind (releasing the held lock), restores the recovery depth, and
/// reports the panic reason.
pub fn test_run_recoverable_cleanup() -> TestResult {
    CANARY_DROPPED.store(false, Ordering::SeqCst);
    let depth_before = panic_recovery::recovery_depth();

    let result = panic_recovery::run_recoverable(|| {
        let mut counter = RECOVERY_LOCK.lock();
        *counter += 1;
        let _canary = UnwindCanary;
        panic!("test_run_recoverable_cleanup: deliberate recoverable panic");
    });

    let Err(oops) = result else {
        slopos_ostd::klog_info!("ASSERT: deliberate panic was not caught");
        return TestResult::Fail;
    };
    assert_test!(
        oops.reason
            .as_str()
            .contains("deliberate recoverable panic"),
        "oops reason does not carry the panic message"
    );
    assert_test!(
        CANARY_DROPPED.load(Ordering::SeqCst),
        "Drop did not run during the caught unwind"
    );
    assert_test!(
        panic_recovery::recovery_depth() == depth_before,
        "recovery depth not restored after catch"
    );
    // The lock guard's Drop ran during the unwind, so the lock must be
    // free again.
    {
        let counter = RECOVERY_LOCK.lock();
        assert_test!(*counter == 1, "locked increment lost across recovery");
    }
    TestResult::Pass
}

/// Ledger arithmetic: `0` disables the limit, the boundary count reports
/// limit-reached, and the hermetic restore returns the exact snapshot.
pub fn test_oops_ledger_accessors() -> TestResult {
    let (count0, limit0) = (panic_recovery::oops_count(), panic_recovery::oops_limit());

    panic_recovery::set_oops_limit(0);
    let (c1, reached1) = panic_recovery::oops_record();
    assert_test!(c1 == count0 + 1, "count did not increment");
    assert_test!(!reached1, "limit 0 must mean unlimited");

    panic_recovery::set_oops_limit(c1 + 1);
    let (c2, reached2) = panic_recovery::oops_record();
    assert_test!(c2 == c1 + 1, "second record did not increment");
    assert_test!(reached2, "boundary count did not report limit-reached");

    panic_recovery::restore_oops_ledger(count0, limit0);
    assert_test!(
        panic_recovery::oops_count() == count0 && panic_recovery::oops_limit() == limit0,
        "ledger restore did not return the snapshot"
    );
    TestResult::Pass
}

slopos_testing::stest!(name = test_run_recoverable_cleanup, suite = panic_recovery);
slopos_testing::stest!(name = test_oops_ledger_accessors, suite = panic_recovery);
