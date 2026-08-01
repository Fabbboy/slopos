//! Task operations that compose more than one field.
//!
//! What a `&TaskInner` method cannot be, and why each of these is a free
//! function instead:
//!
//! - it publishes an event as well as touching the task, so it needs the event
//!   bus rather than only the task body (`task_signal_raise`,
//!   `task_signal_post`, `task_wake_all_waiters`, `task_waiter_count`);
//! - it is keyed by a task *id* rather than by a task (`child_exit_event`,
//!   `signal_pending_event`, `task_kernel_stack_seed_ret`), so there is no
//!   receiver to hang it on;
//! - it needs `&mut TaskInner`, which exists only before publication
//!   (`task_reset_fpu_state`, `task_clone_from`).
//!
//! Everything that *was* expressible as a field read or a `&self` method now
//! is one, on `TaskInner` itself. Nothing here takes a pointer.

use crate::sync::BUS;
use slopos_abi::event::{KernelEvent, TaskSlot};
use slopos_abi::signal::{NSIG, SIG_DFL, SIG_IGN, SIGNAL_KILLED, SIGNAL_MASK, SigSet, sig_bit};

use crate::task::kernel_task::TaskInner;

pub const TASK_EXIT_CLEANUP_RESOURCES: u8 = 1 << 0;
pub const TASK_EXIT_CLEANUP_VM: u8 = 1 << 1;
/// The task's `num_tasks` accounting decrement has been applied. Exit cleanup
/// may run from both an external `task_terminate` and the owning CPU's
/// post-switch path; this bit keeps the decrement exactly-once.
pub const TASK_EXIT_CLEANUP_ACCOUNTED: u8 = 1 << 2;

/// The child-exit event for a task id. Parents blocked in `waitpid`-style
/// waits park on this; the task's exit path publishes it. Public so the
/// `slopos-pidfd` crate can subscribe a pidfd poller to the same event.
#[inline]
pub fn child_exit_event(task_id: u32) -> KernelEvent {
    KernelEvent::ChildExit {
        task: TaskSlot(task_id),
    }
}

/// The signal-pending event for a task id. A `signalfd` poller subscribes
/// here (via the fd's `poll_wait`); every signal-raise site publishes it so a
/// raised signal becomes an in-band ring/poll wakeup instead of relying on the
/// out-of-band interrupt path.
#[inline]
pub fn signal_pending_event(task_id: u32) -> KernelEvent {
    KernelEvent::SignalPending {
        task: TaskSlot(task_id),
    }
}

// ===========================================================================
// Generated accessors.
//
// Each of these is a thin shim over a plain field access or a `&self` method
// call, kept only because its call sites have not moved to the method yet.
// Each disappears with its last caller.
// ===========================================================================

// ---------------------------------------------------------------------------
// Owner-list mechanism.
//
// A parent's `children` list and each task's `sibling_link` are the intrusive
// membership machinery; these accessors are the safe surface over it. They are
// pure mechanism — the strong-reference ownership (park on link, reclaim on
// unlink) and the serialising registry lock are the scheduler crate's policy.
// All list operations are `&self`, so a shared borrow suffices.
// ---------------------------------------------------------------------------

/// Stamp `task->cpu_affinity` and `task->last_cpu` from a single
/// boot-time idle-task install. Wraps two field writes to keep the
/// caller's site free of `unsafe`.
#[inline]
pub fn task_install_idle_affinity<K, U>(task: &TaskInner<K, U>, mask: u32, last_cpu: u8) {
    task.set_cpu_affinity(mask);
    task.set_last_cpu(last_cpu);
}

/// Number of tasks blocked waiting for this task to exit.
#[inline]
pub fn task_waiter_count<K, U>(task: &TaskInner<K, U>) -> usize {
    BUS.waiter_count(child_exit_event(task.task_id))
}

// ---------------------------------------------------------------------------
// Scheduler / driver / signal / stats hot paths. Each absorbs one
// `task.<field>` deref so its call sites stay in safe Rust.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Reclaim-link mechanism (the task graveyard).
//
// Same Treiber-stack shape as the remote-wake inbox, but the invariant is
// inverted: a node here has a strong count of zero and the pusher owns the
// allocation outright, having won the final release. Nothing else may touch a
// parked node, which is why the single-membership claim below can never
// contend.
// ---------------------------------------------------------------------------

/// Wake every task currently blocked waiting for this task to exit.
/// Caller must hold the task pointer stable (e.g. via the task-manager
/// lock or an owning `KArc`) long enough to resolve its id; the event
/// bus queue's internal SpinLock makes the publish interrupt-safe and
/// serialises against any concurrent waiter registration.
#[inline]
pub fn task_wake_all_waiters<K, U>(task: &TaskInner<K, U>) {
    BUS.publish(child_exit_event(task.task_id));
}

/// Raise `mask` on `task`'s pending set **and** wake any `signalfd` poller
/// registered on it (publishes [`signal_pending_event`]). This is the
/// signal-raise chokepoint for in-band (signalfd / ring) delivery: a masked
/// signal stays pending (so it does not interrupt a wait with EINTR) yet a
/// poller subscribed to its `SignalPending` queue is still woken to drain it.
/// Returns the previous pending bitmask.
pub fn task_signal_raise<K, U>(task: &TaskInner<K, U>, mask: u64) -> u64 {
    let prev = task
        .signal_pending
        .fetch_or(mask & SIGNAL_MASK, core::sync::atomic::Ordering::AcqRel);
    BUS.publish(signal_pending_event(task.task_id));
    prev
}

/// Mark `task` for death and make it observe that fact.
///
/// The only writer of [`SIGNAL_KILLED`]. The two halves are fused
/// deliberately: setting the bit without a wake leaves a task parked in a
/// blocking primitive that will never re-run its abort probe, and waking
/// without the bit is a spurious wake. Every other path through this file goes
/// via `sig_bit`, which cannot produce the bit.
///
/// Returns `true` if this call did the marking, `false` if the task was
/// already marked. The wake is issued either way — a redundant unblock on a
/// task that is not blocked is a documented no-op, a lost one is not.
///
/// Does not preempt a *running* victim: the wake is a blocked-to-ready
/// transition, so a killed task spinning in userland keeps running until its
/// next return-to-user boundary.
pub fn task_kill_and_wake<K, U>(task: &TaskInner<K, U>) -> bool {
    let prev = task
        .signal_pending
        .fetch_or(SIGNAL_KILLED, core::sync::atomic::Ordering::AcqRel);
    BUS.publish(signal_pending_event(task.task_id));
    let _ = crate::sync::wait_queue::unblock_task_by_id(task.task_id);
    (prev & SIGNAL_KILLED) == 0
}

/// Post `signum` to `task`, honouring its disposition at the send site.
///
/// This is the disposition-aware raise chokepoint every signal *send*
/// (kill, process-group, session, parent-notify) routes through. A
/// signal that would be discarded anyway — handler is `SIG_IGN`, or
/// `SIG_DFL` with a default of [`SigDefault::Ignore`] — and is **not
/// blocked** is dropped here instead of being left pending, so it
/// never spuriously wakes a blocked task only to be consumed as a
/// no-op at the delivery point. Blocked signals always pend
/// regardless of disposition: a `signalfd` reader or a later-installed
/// handler may still drain them after unblocking.
///
/// Returns `true` when the signal was made pending (the caller should
/// then wake/unblock the target); `false` when it was dropped or the
/// arguments were invalid.
pub fn task_signal_post<K, U>(task: &TaskInner<K, U>, signum: u8) -> bool {
    let bit = slopos_abi::signal::sig_bit(signum);
    if bit == 0 {
        return false;
    }
    if task.signal_pending() & bit != 0 {
        return false;
    }
    let blocked = task.signal_blocked();
    if (blocked & bit) == 0 {
        let handler = task.signal_handler((signum - 1) as usize);
        let ignored = match handler {
            Some(h) if h == slopos_abi::signal::SIG_IGN => true,
            Some(h) if h == slopos_abi::signal::SIG_DFL => {
                slopos_abi::signal::sig_default_ignores(signum)
            }
            _ => false,
        };
        if ignored {
            return false;
        }
    }
    task_signal_raise(task, bit);
    true
}

/// Reset `task->fpu_state` in place via the OSTD-side
/// `fpu_reset_in_place` routine. Caller holds exclusive `&mut Task`
/// access through `task` borrow / fresh slot.
#[inline]
pub fn task_reset_fpu_state<K, U>(task: &mut TaskInner<K, U>) {
    // SAFETY: `&mut Task` gives exclusive access to the in-Task
    // `fpu_state` field; the OSTD reset routine writes a fresh
    // `FpuState` value into the slot.
    unsafe {
        crate::task::kernel_task::fpu_reset_in_place(task.fpu_state.get_mut());
    }
}

/// Reset every *caught* signal (a handler other than `SIG_DFL`/`SIG_IGN`) to
/// `SIG_DFL`. This is the execve disposition reset: a stale handler pointer
/// must never survive into the new image, but POSIX keeps ignored signals
/// ignored, so `SIG_IGN` (and `SIG_DFL`) entries are left untouched. The
/// blocked mask and pending set are preserved across exec by the caller.
#[inline]
pub fn task_reset_caught_handlers<K, U>(task: &TaskInner<K, U>) {
    for action in task.signal_actions.iter() {
        let handler = action.handler();
        if handler != SIG_DFL && handler != SIG_IGN {
            action.reset();
        }
    }
}

/// Force every signal named in `mask` to `SIG_DFL`, overriding a caught
/// handler or `SIG_IGN`. Backs POSIX_SPAWN_SETSIGDEF (spawn) and the
/// `sigdefault` syscall (a forked child installing job-control defaults).
#[inline]
pub fn task_default_signals_in_mask<K, U>(task: &TaskInner<K, U>, mask: SigSet) {
    for signum in 1..=NSIG {
        if mask & sig_bit(signum as u8) != 0 {
            task.signal_actions[signum - 1].reset();
        }
    }
}

/// Write a kernel-mode trampoline return-address into the slot at
/// `kernel_stack_top - 8`. Used by `init_task_context` to seed the
/// first `ret` of a kernel task's switch frame. Caller must hold
/// exclusive access to the (just-allocated) kernel stack.
#[inline]
pub fn task_kernel_stack_seed_ret(kernel_stack_top: u64, trampoline: u64) {
    // SAFETY: `kernel_stack_top` points at the top of a kernel stack
    // the caller just allocated; the slot at `top - 8` is reserved
    // for the synthetic return address.
    unsafe {
        let ret_addr_ptr = (kernel_stack_top - 8) as *mut u64;
        core::ptr::write(ret_addr_ptr, trampoline);
    }
}

/// Clone `other` into `dest` in place via [`Task::clone_from_raw`].
/// Caller must hold exclusive `&mut Task` access to `dest` (e.g.
/// just-reserved slot) and ensure `other` aliases a different slot.
#[inline]
pub fn task_clone_from<K, U>(dest: &mut TaskInner<K, U>, other: &TaskInner<K, U>) {
    // SAFETY: caller's `&mut Task` is exclusive; `other` is a
    // distinct shared borrow; `clone_from_raw` is a bulk-copy
    // routine that maintains atomics' values.
    unsafe { dest.clone_from_raw(other) };
}

/// Test whether `task->signal_pending & !task->signal_blocked` is non-zero,
/// i.e. there is at least one deliverable signal.
#[inline]
pub fn task_has_deliverable_signal<K, U>(task: &TaskInner<K, U>) -> bool {
    (task.signal_pending() & SIGNAL_MASK & !task.signal_blocked()) != 0
}
