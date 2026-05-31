//! Hand-rolled `Waker` over a task id.
//!
//! The `RawWaker` data pointer *is* the task id (a `usize`, `Copy`), so there
//! is no refcount to manage: `clone` returns the same id, `drop` is a no-op,
//! and `wake`/`wake_by_ref` push the id onto the current thread's scheduler
//! ready-queue ([`super::executor::wake_task`]). Single-threaded by design —
//! a waker is only ever fired on the executor thread that created the task
//! (the reactor and all tasks share one thread), so encoding the id inline is
//! sound. (Cross-core wakeups never fire this `!Send` waker from another
//! thread: a cross-core sender writes the target reactor's wakeup-fd, and that
//! reactor fires the local waker on its own thread — see `super::cross_core`.)

use core::task::{RawWaker, RawWakerVTable, Waker};

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone_raw, wake_raw, wake_by_ref_raw, drop_raw);

fn clone_raw(data: *const ()) -> RawWaker {
    RawWaker::new(data, &VTABLE)
}

fn wake_raw(data: *const ()) {
    super::executor::wake_task(data as usize as u64);
}

fn wake_by_ref_raw(data: *const ()) {
    super::executor::wake_task(data as usize as u64);
}

fn drop_raw(_data: *const ()) {}

/// Build a [`Waker`] that, when woken, schedules task `id` for re-poll.
pub(super) fn waker_for(id: u64) -> Waker {
    let raw = RawWaker::new(id as usize as *const (), &VTABLE);
    // SAFETY: the vtable functions only read the data pointer as a task id
    // (never dereference it) and are sound to call from any context; the
    // contract (clone/wake/drop balance) is upheld trivially since the data
    // is a plain integer with no ownership.
    unsafe { Waker::from_raw(raw) }
}
