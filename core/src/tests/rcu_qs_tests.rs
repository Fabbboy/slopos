//! Kernel-side tests for the RCU quiescent-state report guard.
//!
//! `rcu_note_qs` is only sound from a site that is a quiescent state *by
//! construction* — a context switch or the idle loop, where the outgoing task
//! provably holds no read-side section. The two interrupt-context sites (the
//! LAPIC timer tick and the RCU QS IPI) are not such sites: a read-side section
//! holds a `PreemptGuard`, which disables preemption but **not** interrupts, so
//! either can land in the middle of one.
//!
//! Reporting from inside a reader tells `synchronize_rcu` that reader has
//! finished, and the object it is still dereferencing is then freed underneath
//! it. `rcu_note_qs_from_interrupt` is the guarded variant those two sites use;
//! these tests are what say it is actually guarding.

use slopos_ostd::sync::{rcu_note_qs_from_interrupt, rcu_qs_counter, rcu_read_lock};
use slopos_testing::TestResult;
use slopos_testing::assert_test;

/// A report from inside a read-side critical section is declined, and — the
/// half that matters — leaves the counter alone.
///
/// Both assertions are load-bearing. Checking only the `false` return would
/// pass identically against a variant that always declined and never reported
/// anything, which would stall every grace period rather than corrupt memory,
/// but is still a bug this test should be able to see. The positive control
/// below is the other side of that.
pub fn test_rcu_interrupt_qs_declines_inside_a_reader() -> TestResult {
    let cpu = slopos_arch::pcr::get_current_cpu();

    let guard = rcu_read_lock();
    let before = rcu_qs_counter(cpu);
    let reported = rcu_note_qs_from_interrupt();
    let after = rcu_qs_counter(cpu);
    drop(guard);

    assert_test!(
        !reported,
        "rcu_note_qs_from_interrupt reported a quiescent state from inside a \
         read-side critical section"
    );
    assert_test!(
        before == after,
        "a declined report still advanced this CPU's quiescent-state counter — \
         synchronize_rcu would free an object a live reader is dereferencing"
    );
    TestResult::Pass
}

/// Positive control: with no reader held, the same call reports and the counter
/// advances.
///
/// This is what stops the guard from being satisfiable by never reporting.
/// Liveness depends on it: if the tick could never report, a grace period would
/// have to wait for a context switch on every CPU.
pub fn test_rcu_interrupt_qs_reports_outside_a_reader() -> TestResult {
    let cpu = slopos_arch::pcr::get_current_cpu();

    let before = rcu_qs_counter(cpu);
    let reported = rcu_note_qs_from_interrupt();
    let after = rcu_qs_counter(cpu);

    assert_test!(
        reported,
        "rcu_note_qs_from_interrupt declined outside any read-side critical \
         section — a tick that can never report stalls every grace period"
    );
    assert_test!(
        after == before.wrapping_add(1),
        "a successful report did not advance this CPU's quiescent-state counter \
         by exactly one"
    );
    TestResult::Pass
}

/// The guard tracks the section rather than latching: a CPU that declines while
/// a reader is held reports again once it is dropped.
///
/// Without this, a single unbalanced decline could wedge a CPU's reporting for
/// good and the first test would not notice.
pub fn test_rcu_interrupt_qs_recovers_after_the_reader_ends() -> TestResult {
    let cpu = slopos_arch::pcr::get_current_cpu();

    let guard = rcu_read_lock();
    let declined = rcu_note_qs_from_interrupt();
    drop(guard);

    let before = rcu_qs_counter(cpu);
    let reported = rcu_note_qs_from_interrupt();
    let after = rcu_qs_counter(cpu);

    assert_test!(
        !declined,
        "report was not declined while the reader was held"
    );
    assert_test!(
        reported && after == before.wrapping_add(1),
        "this CPU did not resume reporting once its read-side section ended"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_rcu_interrupt_qs_declines_inside_a_reader,
    suite = rcu
);
slopos_testing::stest!(
    name = test_rcu_interrupt_qs_reports_outside_a_reader,
    suite = rcu
);
slopos_testing::stest!(
    name = test_rcu_interrupt_qs_recovers_after_the_reader_ends,
    suite = rcu
);
