//! The per-process task charge, and why it does not live in `Task`.
//!
//! `MAX_TASKS` is 8192 global with no per-process bound, so one process can
//! spend the whole table. The bound this adds is per-principal.
//!
//! # Placement
//!
//! The charge is **not** a field of [`Task`](crate::task::Task). A `Task`'s
//! destruction is deferred to the graveyard — `task_put` parks the final
//! release rather than running it — so a `Drop`-refund there would keep a
//! process that exited a thousand threads charged for all thousand until the
//! drain. That produces spurious `EAGAIN` on the next fork under exactly the
//! load the quota exists to bound.
//!
//! The tree already avoids this for its own count: `num_tasks` is adjusted at
//! `exit_cleanup_mark(TASK_EXIT_CLEANUP_ACCOUNTED)`, the latch that fires
//! exactly once however the exit is split between `task_terminate` and the
//! owning CPU's post-switch path. The charge is released at that same latch,
//! out of this side table, keyed on the task id.
//!
//! # Why the slot holds the token itself
//!
//! Each row is a [`ChargeSlot`], which owns a real `Charge<TaskCount>` and
//! hands it out exactly once. The token is never decomposed into an account
//! and an amount: a charge stored as *data* would be a charge nothing refunds
//! if the row were overwritten, which is the failure mode the linear token
//! exists to prevent. Taking it out of the slot is a move, so the latch that
//! wins the take is the only caller that can refund it, and the second latch
//! finds an empty slot.

use slopos_abi::quota::TaskCount;
use slopos_abi::task::{INVALID_TASK_ID, MAX_TASKS};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{ChargeSlot, Reservation, try_charge};

/// One entry per live task. Sized to `MAX_TASKS` so the id-keyed index is
/// exact rather than a hash: two tasks sharing a row would make a release
/// refund the wrong account.
const SLOTS: usize = MAX_TASKS;

static ROWS: [ChargeSlot<TaskCount>; SLOTS] = [const { ChargeSlot::empty() }; SLOTS];

/// Reserve one task against `account`.
///
/// Returns the reservation rather than recording it, so a caller that fails
/// later unwinds through the ordinary reservation `Drop` and the table never
/// has to be rolled back.
pub fn reserve(account: AccountId) -> Option<Reservation<TaskCount>> {
    try_charge::<TaskCount>(account, 1).ok()
}

/// Record `reservation` as `task_id`'s charge.
///
/// A row already occupied means an id was reused without its predecessor's
/// latch firing; the displaced charge is refunded here rather than dropped on
/// the floor, so the accounting error is bounded at zero rather than at one
/// leaked charge per recycle.
pub fn commit(task_id: u32, reservation: Reservation<TaskCount>) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    ROWS[(task_id as usize) % SLOTS].put(reservation);
}

/// Release `task_id`'s charge. Idempotent: the latch firing twice, or from two
/// CPUs, refunds once.
///
/// Atomics only — no lock, no allocation, no counted reference — which is what
/// makes it legal from the exit path, where interrupts are off.
pub fn release(task_id: u32) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    ROWS[(task_id as usize) % SLOTS].take();
}

/// Drop every recorded charge. Test-fixture only.
pub fn reset_for_test() {
    for row in ROWS.iter() {
        row.take();
    }
}
