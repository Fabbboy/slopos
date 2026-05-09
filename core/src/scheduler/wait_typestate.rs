//! Typestate state machine for the task-wait-for-task protocol.
//!
//! Step 4 of `plans/SAFE_BY_DESIGN.md`. The wait sequence used to be a
//! hand-written ordered list of:
//!
//! ```ignore
//! prepare_to_wait();              // state Running→WillBlock
//! task_set_waiting_on(...);       // publish waiter id
//! fence;
//! check target alive;             // catches "target exited before publish"
//! block_current_task();           // CAS WillBlock→Blocked + yield
//! finish_wait();
//! task_set_waiting_on(INVALID);
//! ```
//!
//! Any reordering or skipped step re-introduces one of the lost-wake
//! races the current branch fixed. This module turns the protocol
//! into a typestate state machine — each transition consumes its
//! input by-value, so a future caller cannot `block()` without first
//! `publish()`-ing, and cannot `publish()` without `prepare()`-ing.
//!
//! `Drop` impls panic on either typestate so accidentally dropping
//! a `PreparedWait`/`PublishedWait` mid-protocol is loud rather than
//! silently leaving the task in `WillBlock` or `Blocked`-with-stale-
//! waiter-id state.

use core::ffi::c_int;
use core::marker::PhantomData;

use slopos_ostd::cpu::x86_64::interrupts::IrqDisabled;

use super::scheduler::{
    block_current_task, finish_wait, prepare_to_wait, scheduler_get_current_task,
};
use super::task::{
    INVALID_TASK_ID, Task, task_find_by_id, task_get_info, task_id_of, task_is_invalid,
    task_is_terminated, task_set_waiting_on,
};

/// State 1: prepare_to_wait has set state→WillBlock.
///
/// Constructed by [`prepare_to_wait_for`]. Consumes by-value into
/// either [`PreparedWait::publish`] (advance) or [`PreparedWait::cancel`]
/// (abort, restore Running). Dropping without either call panics.
pub struct PreparedWait {
    current: *mut Task,
    /// `!Send + !Sync`: per-CPU state, must not migrate.
    _not_send: PhantomData<*const ()>,
}

/// State 2: waiting_on has been set and the SeqCst fence has run.
///
/// Constructed by [`PreparedWait::publish`]. Consumes into either
/// [`PublishedWait::block`] (proceed to actual block) or
/// [`PublishedWait::cancel`] (abort, finish_wait + clear waiting_on).
pub struct PublishedWait {
    current: *mut Task,
    target_id: u32,
    /// Snapshot of the target pointer at publish time. Used to detect
    /// slot reuse when re-checking.
    target_snapshot: *mut Task,
    _not_send: PhantomData<*const ()>,
}

/// Begin a typestate-checked wait sequence. Returns `None` if there
/// is no current task to put to sleep.
pub fn prepare_to_wait_for() -> Option<PreparedWait> {
    let current = scheduler_get_current_task();
    if current.is_null() {
        return None;
    }
    prepare_to_wait();
    Some(PreparedWait {
        current,
        _not_send: PhantomData,
    })
}

impl PreparedWait {
    /// Publish `target_id` as the task we are waiting on. Returns
    /// [`Err(self)`] if the target is missing — the caller should
    /// `cancel()` the prepared wait in that case.
    pub fn publish(self, target_id: u32) -> Result<PublishedWait, Self> {
        if target_id == INVALID_TASK_ID {
            return Err(self);
        }
        if task_id_of(self.current) == Some(target_id) {
            return Err(self);
        }
        let mut target_snapshot: *mut Task = core::ptr::null_mut();
        if task_get_info(target_id, &mut target_snapshot) != 0 || target_snapshot.is_null() {
            return Err(self);
        }
        task_set_waiting_on(self.current, target_id);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        let r = PublishedWait {
            current: self.current,
            target_id,
            target_snapshot,
            _not_send: PhantomData,
        };
        core::mem::forget(self);
        r.return_self_or_err()
    }

    /// Abort the prepared wait without blocking. Restores state to
    /// Running via `finish_wait`.
    pub fn cancel(self) {
        finish_wait();
        core::mem::forget(self);
    }
}

impl PublishedWait {
    /// Re-check whether the target is still alive after our publish.
    /// Catches the race where the target terminated (or had its slot
    /// reset and reused) before our `task_set_waiting_on` was visible
    /// to `release_task_dependents`. Returns `Err(self)` if the
    /// target is gone — caller should `cancel()`.
    fn return_self_or_err(self) -> Result<Self, PreparedWait> {
        let now = task_find_by_id(self.target_id);
        if now.is_null()
            || now != self.target_snapshot
            || task_is_terminated(now)
            || task_is_invalid(now)
        {
            // Roll back to PreparedWait — caller's Result handler will
            // typically `cancel()` it.
            task_set_waiting_on(self.current, INVALID_TASK_ID);
            let prep = PreparedWait {
                current: self.current,
                _not_send: PhantomData,
            };
            core::mem::forget(self);
            Err(prep)
        } else {
            Ok(self)
        }
    }

    /// Commit the wait: CAS WillBlock→Blocked, unschedule, yield.
    /// Takes an [`IrqDisabled<'_>`] capability — the cli scope is
    /// part of `block_current_task`'s contract (see `step 3`). Returns
    /// after the wake has fired and the scheduler dispatched us back.
    pub fn block(self, _irq: &IrqDisabled<'_>) -> c_int {
        block_current_task();
        finish_wait();
        task_set_waiting_on(self.current, INVALID_TASK_ID);
        core::mem::forget(self);
        0
    }

    /// Abort the published wait without blocking. Restores state to
    /// Running and clears `waiting_on`.
    pub fn cancel(self) {
        finish_wait();
        task_set_waiting_on(self.current, INVALID_TASK_ID);
        core::mem::forget(self);
    }
}

impl Drop for PreparedWait {
    fn drop(&mut self) {
        // Dropping a PreparedWait without publish/cancel leaves the
        // task in WillBlock with no path back to Running. Loud panic
        // here is better than a silent hang.
        panic!("PreparedWait dropped without publish() or cancel() — task left in WillBlock");
    }
}

impl Drop for PublishedWait {
    fn drop(&mut self) {
        panic!(
            "PublishedWait dropped without block() or cancel() — task left in WillBlock with stale waiting_on"
        );
    }
}
