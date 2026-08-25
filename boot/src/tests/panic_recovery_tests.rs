//! Fault injection for the task-scoped panic-recovery contract. The escalation
//! path (a limit-crossing panic turning fatal) halts the machine, so it is
//! covered by the `panic.oops_limit=1 panic.recover_smoke=on` boot-log check.

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
    // Re-locking proves the guard's Drop ran during the unwind.
    {
        let counter = RECOVERY_LOCK.lock();
        assert_test!(*counter == 1, "locked increment lost across recovery");
    }
    TestResult::Pass
}

/// A private ledger: recording against the machine's own would spend the budget
/// whose exhaustion makes the next recovered panic fatal.
pub fn test_oops_ledger_accessors() -> TestResult {
    // Budget-neutral: the count is put back exactly as it was found.
    let (live_count, live_limit) = (panic_recovery::oops_count(), panic_recovery::oops_limit());
    const SENTINEL: u64 = 0xFEED_5EED;
    panic_recovery::set_oops_limit(SENTINEL);
    let observed_limit = panic_recovery::oops_limit();
    panic_recovery::restore_oops_ledger(live_count, live_limit);
    assert_test!(
        observed_limit == SENTINEL,
        "oops_limit() did not observe what set_oops_limit() wrote — the accessors \
         name different ledgers"
    );
    assert_test!(
        panic_recovery::oops_limit() == live_limit && panic_recovery::oops_count() == live_count,
        "restore_oops_ledger did not put the machine's ledger back"
    );

    let ledger = panic_recovery::OopsLedger::new(0);
    assert_test!(ledger.count() == 0, "a fresh ledger has recorded nothing");

    let (c1, reached1) = ledger.record();
    assert_test!(c1 == 1, "count did not increment");
    assert_test!(!reached1, "limit 0 must mean unlimited");

    ledger.set_limit(c1 + 1);
    let (c2, reached2) = ledger.record();
    assert_test!(c2 == c1 + 1, "second record did not increment");
    assert_test!(reached2, "boundary count did not report limit-reached");

    let (c3, reached3) = ledger.record();
    assert_test!(c3 == c2 + 1, "third record did not increment");
    assert_test!(reached3, "a count past the limit is still limit-reached");

    ledger.restore(c1, 0);
    assert_test!(
        ledger.count() == c1 && ledger.limit() == 0,
        "ledger restore did not return the snapshot"
    );
    let (c4, reached4) = ledger.record();
    assert_test!(c4 == c1 + 1, "a restored ledger did not resume counting");
    assert_test!(!reached4, "a restored limit of 0 must mean unlimited");
    TestResult::Pass
}

slopos_testing::stest!(name = test_run_recoverable_cleanup, suite = panic_recovery);
slopos_testing::stest!(name = test_oops_ledger_accessors, suite = panic_recovery);
