//! Kernel-side tests for RCU deferred-callback reclamation.
//!
//! `call_rcu` only queues, so the reachability test never calls the drain by
//! hand: a queue whose consumer is unreachable looks exactly like a working one
//! from the producer's side.

use core::sync::atomic::{AtomicU32, Ordering};

use slopos_kernel_services::platform::timer_poll_delay_ms;
use slopos_ostd::KArc;
use slopos_ostd::sync::RcuArcSlot;
use slopos_sched::test_fixture::KernelTestScope;
use slopos_testing::TestResult;
use slopos_testing::fail;

/// Quiescent-state reports to allow before calling the drain unreachable.
///
/// The budget is guest progress, not wall clock: this CPU's timer tick raises
/// the RCU softirq and reports a quiescent state unless a preemption guard is
/// active, so a descheduled vCPU delivers no tick and spends none of the
/// budget. A report declined across the whole poll spends none either, which is
/// what leaves [`TICK_WATCHDOG_MS`] as the only bound in that case.
const RECLAIM_DEADLINE_REPORTS: u64 = 64;

/// Backstop on the budget above, and the one wall-clock quantity here.
///
/// Reaching it means this CPU reported fewer than [`RECLAIM_DEADLINE_REPORTS`]
/// times in five real seconds. At the 10 ms tick that is roughly an eighth of
/// the reports a live tick delivers, so it is a collapsed report rate rather
/// than a slow one — a dead tick, or a report declined every time. A host slow
/// enough to starve the guest of eight-ninths of its ticks would reach it too;
/// that is the residue of having a wall clock in the loop at all, and it is why
/// the report count is the budget and this is only the backstop.
const TICK_WATCHDOG_MS: u32 = 5_000;

/// Polled rather than slept: the kernel test phase runs on the BSP before it
/// enters the scheduler, so this task cannot block.
const POLL_INTERVAL_MS: u32 = 10;

/// Drive queued callbacks to invocation until `done`, and report whether it
/// came true.
///
/// Drives the drain itself rather than calling `rcu_barrier`, which has no
/// escape: a grace period that cannot advance — a peer wedged in a reader, a
/// holdout that never reports — spins there forever, the KTAP stream goes
/// silent and the harness kills the whole run. One test failing is strictly
/// better than every later test being lost.
///
/// The budget is guest progress for the reason above the constant, with the
/// wall clock only as a backstop for a tick that stopped entirely.
pub fn drain_until(done: impl Fn() -> bool) -> bool {
    let cpu = slopos_arch::pcr::get_current_cpu();
    let reports_before = slopos_ostd::sync::rcu_qs_counter(cpu);
    let mut waited = 0;
    loop {
        slopos_ostd::sync::rcu_raise_softirq();
        slopos_ostd::sync::rcu_process_callbacks();
        // No `rcu_note_qs` of its own: the budget below counts this CPU's
        // reports, and a loop that reports spends its own budget whether or not
        // the guest advanced. The tick reports for it when it lands outside a
        // read-side section.
        slopos_ostd::sync::rcu_gp_poll();
        if done() {
            return true;
        }
        let reports = slopos_ostd::sync::rcu_qs_counter(cpu).wrapping_sub(reports_before);
        if reports >= RECLAIM_DEADLINE_REPORTS || waited >= TICK_WATCHDOG_MS {
            return false;
        }
        timer_poll_delay_ms(POLL_INTERVAL_MS);
        waited += POLL_INTERVAL_MS;
    }
}

static DROPPED: AtomicU32 = AtomicU32::new(0);

struct DropCounted;

impl Drop for DropCounted {
    fn drop(&mut self) {
        DROPPED.fetch_add(1, Ordering::Release);
    }
}

/// `RcuArcSlot::store` defers the displaced reference through `call_rcu`, so
/// the payload's `Drop` runs only if the callback does — and only if something
/// other than this test reaches the drain.
pub fn test_rcu_callbacks_are_invoked_without_a_manual_drain() -> TestResult {
    let cpu = slopos_arch::pcr::get_current_cpu();
    let before = DROPPED.load(Ordering::Acquire);
    let reports_before = slopos_ostd::sync::rcu_qs_counter(cpu);

    let slot = RcuArcSlot::<DropCounted>::empty();
    let Ok(value) = KArc::try_new(DropCounted) else {
        return fail!("KArc allocation failed");
    };
    slot.store(Some(value));
    // Displaces the only reference, which is what reaches `call_rcu`.
    slot.store(None);

    let mut waited = 0;
    loop {
        if DROPPED.load(Ordering::Acquire) != before {
            return TestResult::Pass;
        }

        let reports = slopos_ostd::sync::rcu_qs_counter(cpu).wrapping_sub(reports_before);
        if reports >= RECLAIM_DEADLINE_REPORTS {
            return fail!(
                "no RCU callback ran across {} quiescent-state reports on CPU {} — every \
                 deferred free since boot is leaking",
                reports,
                cpu
            );
        }
        if waited >= TICK_WATCHDOG_MS {
            return fail!(
                "CPU {} reported {} quiescent states in {}ms — its timer tick has stopped, so \
                 nothing drives the drain",
                cpu,
                reports,
                TICK_WATCHDOG_MS
            );
        }

        timer_poll_delay_ms(POLL_INTERVAL_MS);
        waited += POLL_INTERVAL_MS;
    }
}

/// Called from the reclaim path, including `call_rcu`'s own out-of-memory
/// fallback, so an allocation here fails exactly when there is nothing to
/// allocate from.
///
/// The heap counter is kernel-wide, so the scope is what makes it name this
/// caller: with the APs parked and the kernel-I/O threads frozen, no other
/// allocator is running to be mistaken for `synchronize_rcu`.
pub fn test_synchronize_rcu_allocates_nothing() -> TestResult {
    let _scope = KernelTestScope::enter();

    let before = slopos_mm::slab::get_heap_stats_owned();
    slopos_ostd::sync::synchronize_rcu();
    let after = slopos_mm::slab::get_heap_stats_owned();

    if after.allocation_count != before.allocation_count {
        return fail!(
            "synchronize_rcu allocated {} time(s)",
            after.allocation_count.wrapping_sub(before.allocation_count)
        );
    }
    TestResult::Pass
}

pub fn test_synchronize_rcu_completes_a_grace_period() -> TestResult {
    let before = slopos_ostd::sync::rcu_gp_seq();
    slopos_ostd::sync::synchronize_rcu();
    let after = slopos_ostd::sync::rcu_gp_seq();

    if after.wrapping_sub(before) < 2 {
        return fail!(
            "grace-period sequence advanced {} (want >= 2)",
            after.wrapping_sub(before)
        );
    }
    TestResult::Pass
}

/// A drain pass never takes a grace period inline. Putting the wait back would
/// still pass every correctness test, so the passes are counted at the wait
/// itself: this CPU's entries into `synchronize_rcu`, which only the drain can
/// have made here, must not move.
///
/// That count only sees a wait routed through `synchronize_rcu`. An open-coded
/// one is caught by the two checks after it rather than by a clock, which on a
/// host slow enough would have to be loose enough to miss the wait anyway.
///
/// A wait for a grace period to *complete* has to reach `rcu_gp_poll` — the
/// only operation that runs the completing compare-exchange — and a wait that
/// never reaches it cannot terminate, so it takes the run down loudly instead
/// of passing. Nothing on the correct drain path calls it, so the expected
/// delta is exactly zero rather than a budget. `rcu_note_qs` is the other way
/// to drive a period from in here, hence the third check.
pub fn test_rcu_drain_never_waits_for_a_grace_period() -> TestResult {
    const PASSES: u32 = 32;

    // Queue work so the passes have something to tag and retire rather than
    // early-returning on an empty backlog.
    for _ in 0..8 {
        let slot = RcuArcSlot::<DropCounted>::empty();
        let Ok(value) = KArc::try_new(DropCounted) else {
            return fail!("KArc allocation failed");
        };
        slot.store(Some(value));
        slot.store(None);
    }

    // Sampled inside one masked window with the reads it indexes: `rcu_note_qs`
    // increments the slot of whichever CPU is live, so a `cpu` taken before the
    // window names a slot the reports need not have touched.
    let (sync_before, sync_after, qs_before, qs_after, polls_before, polls_after) =
        slopos_ostd::cpu::x86_64::interrupts::IrqDisabled::with(|_irq| {
            let cpu = slopos_arch::pcr::get_current_cpu();
            let sync_before = slopos_ostd::sync::rcu_sync_entry_count(cpu);
            let qs_before = slopos_ostd::sync::rcu_qs_counter(cpu);
            let polls_before = slopos_ostd::sync::rcu_gp_poll_count(cpu);
            for _ in 0..PASSES {
                slopos_ostd::sync::rcu_raise_softirq();
                slopos_ostd::sync::rcu_process_callbacks();
            }
            (
                sync_before,
                slopos_ostd::sync::rcu_sync_entry_count(cpu),
                qs_before,
                slopos_ostd::sync::rcu_qs_counter(cpu),
                polls_before,
                slopos_ostd::sync::rcu_gp_poll_count(cpu),
            )
        });

    if polls_after != polls_before {
        return fail!(
            "{} drain passes polled the grace period {} time(s) with interrupts masked — \
             nothing on the drain's path polls, so the invoke step is driving a period it \
             means to wait for",
            PASSES,
            polls_after.wrapping_sub(polls_before)
        );
    }

    if sync_after != sync_before {
        return fail!(
            "{} drain passes entered the grace-period wait {} time(s) — the invoke step is \
             waiting again",
            PASSES,
            sync_after.wrapping_sub(sync_before)
        );
    }
    if qs_after != qs_before {
        return fail!(
            "{} drain passes reported {} quiescent state(s) with interrupts masked — nothing \
             on the drain's path reports, so the invoke step is waiting for a grace period \
             it open-coded",
            PASSES,
            qs_after.wrapping_sub(qs_before)
        );
    }
    TestResult::Pass
}

/// `rcu_barrier` waits for invocation, not merely for a grace period — once
/// the drain is asynchronous those stop being the same fact, and a caller
/// tearing down what a callback will touch needs the first.
pub fn test_rcu_barrier_waits_for_invocation() -> TestResult {
    let before = DROPPED.load(Ordering::Acquire);

    let slot = RcuArcSlot::<DropCounted>::empty();
    let Ok(value) = KArc::try_new(DropCounted) else {
        return fail!("KArc allocation failed");
    };
    slot.store(Some(value));
    slot.store(None);

    slopos_ostd::sync::rcu_barrier();

    if DROPPED.load(Ordering::Acquire) == before {
        return fail!("rcu_barrier returned with a callback still pending");
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_rcu_callbacks_are_invoked_without_a_manual_drain,
    suite = rcu_cb
);
slopos_testing::stest!(
    name = test_rcu_drain_never_waits_for_a_grace_period,
    suite = rcu_cb
);
slopos_testing::stest!(name = test_rcu_barrier_waits_for_invocation, suite = rcu_cb);
slopos_testing::stest!(
    name = test_synchronize_rcu_allocates_nothing,
    suite = rcu_cb
);
slopos_testing::stest!(
    name = test_synchronize_rcu_completes_a_grace_period,
    suite = rcu_cb
);
