//! Per-principal task charge, held in a side table rather than in
//! [`Task`](crate::task::Task).
//!
//! `Task` destruction is deferred to the graveyard, so a `Drop`-refund there
//! would keep an exited process charged until the drain; release happens at the
//! `exit_cleanup_mark(TASK_EXIT_CLEANUP_ACCOUNTED)` latch instead, which fires
//! exactly once however the exit is split across CPUs.
//!
//! Each row owns a real `Charge<TaskCount>` rather than an account/amount pair:
//! taking it is a move, so only the latch that wins the take can refund it.

use slopos_abi::quota::TaskCount;
use slopos_abi::task::{INVALID_TASK_ID, MAX_TASKS};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{ChargeSlot, Reservation, try_charge};

/// Sized to `MAX_TASKS` so the id-keyed index is exact rather than a hash: two
/// tasks sharing a row would make a release refund the wrong account.
const SLOTS: usize = MAX_TASKS;

static ROWS: [ChargeSlot<TaskCount>; SLOTS] = [const { ChargeSlot::empty() }; SLOTS];

/// Reserve one task against `account`.
///
/// Returned rather than recorded, so a caller that fails later unwinds through
/// the reservation's `Drop` and the table never has to be rolled back.
pub fn reserve(account: AccountId) -> Option<Reservation<TaskCount>> {
    try_charge::<TaskCount>(account, 1).ok()
}

/// Record `reservation` as `task_id`'s charge.
///
/// An occupied row means an id was reused without its predecessor's latch
/// firing; the displaced charge is refunded rather than leaked.
pub fn commit(task_id: u32, reservation: Reservation<TaskCount>) {
    if task_id == INVALID_TASK_ID {
        return;
    }
    ROWS[(task_id as usize) % SLOTS].put(reservation);
}

/// Release `task_id`'s charge. Idempotent: the latch firing twice, or from two
/// CPUs, refunds once.
///
/// Atomics only — no lock, no allocation — so it is legal from the exit path,
/// where interrupts are off.
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
