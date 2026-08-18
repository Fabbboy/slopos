//! Kernel-side tests for the RCU quiescent-state report guard.
//!
//! `rcu_note_qs` is only sound from a site that is a quiescent state *by
//! construction* — a context switch or the idle loop. The two interrupt-context
//! sites (the LAPIC timer tick and the RCU QS IPI) are not: a read-side section
//! holds a `PreemptGuard`, which disables preemption but **not** interrupts, so
//! either can land in the middle of one, and a report from inside a reader frees
//! the object that reader is still dereferencing.
//! `rcu_note_qs_from_interrupt` is the guarded variant those two sites use.

use slopos_ostd::sync::{rcu_note_qs_from_interrupt, rcu_qs_counter, rcu_read_lock};
use slopos_testing::TestResult;
use slopos_testing::assert_test;

/// A report from inside a read-side critical section is declined and leaves the
/// counter alone. Checking only the `false` return would pass identically
/// against a variant that always declined; the positive control below is the
/// other side of that.
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

/// Positive control: what stops the guard from being satisfiable by never
/// reporting. If the tick could never report, a grace period would have to wait
/// for a context switch on every CPU.
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

/// The guard tracks the section rather than latching. Without this, a single
/// unbalanced decline could wedge a CPU's reporting for good and the first test
/// would not notice.
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
