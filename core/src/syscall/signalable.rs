//! `Signalable` — a signal target, resolved once and authorized in the same
//! step.
//!
//! A bare witness proves only that *a* check ran, not that it named this
//! target. `syscall_terminate_task` was the in-tree counterexample: it checked
//! the compositor bit and then terminated any id, and a `&Cap<ProcSignal>`
//! would have left it byte-identical. So the witness carries the object —
//! acting is a method on the target, and there is no other way to name one.

use slopos_abi::Errno;
use slopos_abi::task::INVALID_TASK_ID;
use slopos_sched::task::{TaskRef, task_find_by_id};

use crate::syscall::signal::{signal_dominates, signal_is_init, signal_may_name};

/// A live task the caller has been authorized to signal.
///
/// Owns a `TaskRef` rather than an id: the target cannot exit and be recycled
/// onto a stranger between the check and the act.
#[must_use = "a resolved signal target that is never acted on is a discarded \
              authorization; drop it explicitly if that is intended"]
pub struct Signalable {
    target: TaskRef,
    id: u32,
}

impl Signalable {
    /// The target's id, for reporting. Never a re-lookup key — the reference
    /// is what names the task.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The target itself.
    #[inline]
    pub fn task(&self) -> &TaskRef {
        &self.target
    }

    /// Consume the witness, yielding the owning reference.
    #[inline]
    pub fn into_task(self) -> TaskRef {
        self.target
    }
}

/// Resolve `target_id` and authorize `caller_flags` to signal it, in one step.
///
/// The two cannot be separated by construction, which is the point: there is
/// no `resolve` that skips the check and no `check` that takes an id.
///
/// - `ESRCH` — no such task, or a kernel task. Kernel tasks are excluded from
///   delivery, so naming one could only reach the `SIGKILL` arm and tear down a
///   driver thread holding device state and an interrupt line.
/// - `EPERM` — the caller does not dominate the target, or the target is init.
pub fn resolve_signal_target(caller_flags: u16, target_id: u32) -> Result<Signalable, Errno> {
    if target_id == INVALID_TASK_ID || target_id == 0 {
        return Err(Errno::ESRCH);
    }
    let target = task_find_by_id(target_id).ok_or(Errno::ESRCH)?;
    // A kernel task is not a process: `ESRCH` rather than `EPERM`, so the
    // refusal does not disclose that the id names something.
    if !signal_may_name(target.flags) {
        return Err(Errno::ESRCH);
    }
    // A terminating signal to init takes the system down undebuggably.
    // Dominance already covers it; the guarantee should not rest on that.
    // Audited, not tested: no test phase can name init's id.
    if signal_is_init(target_id) {
        return Err(Errno::EPERM);
    }
    if !signal_dominates(caller_flags, target.flags) {
        return Err(Errno::EPERM);
    }
    Ok(Signalable {
        target,
        id: target_id,
    })
}
