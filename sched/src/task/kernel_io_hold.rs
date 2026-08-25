//! The kernel-I/O hold: the freeze's enforceable half.
//!
//! A freeze is cooperative, so a thread that never runs never parks. A hold is
//! not: it sweeps every covered thread off every scheduler container, refuses
//! every publication for as long as it is armed, and publishes each one again
//! when it releases.

use core::marker::PhantomData;

use slopos_ostd::klog_debug;
use slopos_ostd::sync::kernel_io_task::{
    KernelIoTaskIds, arm_kernel_io_hold, disarm_kernel_io_hold, kernel_io_hold_armed,
    kernel_io_hold_covers, kernel_io_task_ids, refresh_kernel_io_hold,
};
use slopos_ostd::task::SchedPlacement;

use super::{KernelIoFreeze, task_find_by_id};
use crate::per_cpu::ApPauseToken;
use crate::task_struct::Task;

/// Take `task` out of the publisher's hands for the hold's duration. `from` is
/// the placement the caller owns, so a claim can only take a task the caller
/// was itself about to publish.
#[inline]
pub(crate) fn kernel_io_hold_claim(task: &Task, from: SchedPlacement) -> bool {
    kernel_io_hold_covers(task.task_id)
        && task.sched_placement_compare_exchange(from, SchedPlacement::Held)
}

/// Registered kernel-I/O tasks a scheduler container still owns. Zero is the
/// hold's whole contract, measured rather than assumed.
pub fn kernel_io_dispatchable_count() -> usize {
    // The whole registry, never the hold's arm-time snapshot. Answering off the
    // snapshot would make a thread that registered after the arm invisible to
    // the predicate while it stayed fully queueable — the predicate would report
    // quiesced and the caller would race exactly the thread it asked about.
    // Snapshotted off-lock: the stop registry and the task registry share a lock
    // level, so the lookups below must not run under the former.
    let armed = kernel_io_hold_armed();
    let mut count = 0usize;
    for id in kernel_io_task_ids().iter() {
        let Some(task) = task_find_by_id(id) else {
            continue;
        };
        // `Waking` is a reservation, not a container: the task is on no queue
        // and cannot be dispatched from it. A *covered* publisher holding that
        // reservation is claimed at the enqueue gate before it can link
        // anything, so it is quiesced; an uncovered one really is on its way to
        // a queue and counts until the next refresh brings it into the cover.
        let owned = match task.sched_placement() {
            SchedPlacement::ReadyQueue
            | SchedPlacement::RemoteWake
            | SchedPlacement::OnCpu
            | SchedPlacement::Migrating => true,
            SchedPlacement::Waking => !armed || !kernel_io_hold_covers(id),
            _ => false,
        };
        if owned {
            count += 1;
        }
    }
    count
}

/// Every registered kernel-I/O thread is off every run queue and stays off
/// until this token is dropped.
#[must_use = "dropping the token publishes the held threads again"]
pub struct KernelIoHold {
    swept: usize,
    unsettled: usize,
    /// !Send !Sync: the release must run on the CPU that armed the hold, which
    /// is the only one not parked.
    _not_send: PhantomData<*mut ()>,
}

impl KernelIoHold {
    /// How many threads the sweep took off a run queue or inbox. A cooperative
    /// freeze that completed leaves this zero.
    #[inline]
    pub fn swept(&self) -> usize {
        self.swept
    }

    /// Registered kernel-I/O threads a container still owned when the settle
    /// loop gave up. Zero is the hold's contract; anything else means the
    /// caller's scope does not have the property it was entered for, and the
    /// caller has to be able to say so rather than discover it as a flake in
    /// whichever test runs next.
    #[inline]
    pub fn unsettled(&self) -> usize {
        self.unsettled
    }
}

/// An AP that dequeued a task before `pause_all_aps` observed it reads back as
/// parked — `is_executing_task` is set after the dequeue — and hands the task
/// back through `enqueue_from_on_cpu` once it resumes. So one sweep is not
/// enough: it is repeated until no covered task is owned by any container,
/// which the enqueue-path claim guarantees each such owner reaches within its
/// own call.
const HOLD_SETTLE_SPINS: u32 = 4_096;

/// The AP pause is a precondition, not a courtesy: `ReadyQueue::dequeue`
/// ignores its own placement CAS, so an AP still dispatching could run a task
/// this sweep had already claimed.
pub fn hold_kernel_io_all(_freeze: &KernelIoFreeze, paused: &ApPauseToken) -> KernelIoHold {
    // Armed before the sweep, or a publisher slips in between the two.
    arm_kernel_io_hold();
    let mut swept = 0usize;
    let mut settled = false;
    for _ in 0..HOLD_SETTLE_SPINS {
        // Before the sweep, so a stop that bound its id since the arm is covered
        // by the time this round's claims run.
        refresh_kernel_io_hold();
        swept += crate::per_cpu::hold_kernel_io_off_all_runqueues(paused);
        if kernel_io_dispatchable_count() == 0 {
            settled = true;
            break;
        }
        core::hint::spin_loop();
    }
    // `klog_info!`, not debug: a give-up means the scope's stated contract is
    // false, and a diagnostic the default verbosity filters out is a give-up
    // nobody sees. It reaches the raw stream, where a ratchet can parse it.
    let unsettled = if settled {
        0
    } else {
        let left = kernel_io_dispatchable_count();
        slopos_ostd::klog_info!(
            "SCHED: KERNEL_IO_HOLD_UNSETTLED left={} spins={}",
            left,
            HOLD_SETTLE_SPINS
        );
        left
    };
    if swept != 0 {
        klog_debug!(
            "SCHED: kernel-io hold took {} thread(s) off a run queue",
            swept
        );
    }
    KernelIoHold {
        swept,
        unsettled,
        _not_send: PhantomData,
    }
}

/// Publish every task the hold took, so a thread that was runnable when the
/// hold was armed is runnable again when it is released. Idempotent: a task
/// that is not `Held` is not this walk's.
pub fn republish_held_kernel_io(held: &KernelIoTaskIds) {
    for id in held.iter() {
        let Some(task) = task_find_by_id(id) else {
            continue;
        };
        let body: &Task = &task;
        if body.sched_placement() != SchedPlacement::Held {
            continue;
        }
        if !body.is_ready() {
            let _ =
                body.sched_placement_compare_exchange(SchedPlacement::Held, SchedPlacement::None);
            continue;
        }
        if !body.sched_placement_compare_exchange(SchedPlacement::Held, SchedPlacement::Waking) {
            continue;
        }
        if crate::scheduler::schedule_task(&task) != 0 {
            klog_debug!(
                "SCHED: kernel-io hold could not republish task {}",
                body.task_id
            );
        }
    }
}

impl Drop for KernelIoHold {
    fn drop(&mut self) {
        // Disarmed first, or the republish below is claimed straight back.
        if let Some(held) = disarm_kernel_io_hold() {
            republish_held_kernel_io(&held);
        }
    }
}
