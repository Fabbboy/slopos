//! Task operations that cannot be `&TaskInner` methods: they publish an event
//! as well as touching the task, they are keyed by a task *id* rather than by
//! a task, or they need the `&mut TaskInner` that exists only before
//! publication. Everything expressible as a `&self` method is one, on
//! `TaskInner` itself.

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
/// The task has left its [`Process`](crate::process::Process): its share of
/// the process's `task_count` has been given back, and its owning
/// `KArc<Process>` reference released.
///
/// A distinct bit from `ACCOUNTED` because the two decrements answer different
/// questions: that one is the global live-task census, this one decides
/// whether this exit is the one that tears down the address space.
pub const TASK_EXIT_CLEANUP_CHARGES: u8 = 1 << 3;

/// The child-exit event for a task id. Parents blocked in `waitpid`-style
/// waits park on this; the task's exit path publishes it. Public so the
/// `slopos-pidfd` crate can subscribe a pidfd poller to the same event.
#[inline]
pub fn child_exit_event(task_id: u32) -> KernelEvent {
    KernelEvent::ChildExit {
        task: TaskSlot(task_id),
    }
}

/// The any-child-exit event for a *parent* task id. A `waitpid(-1)` waiter
/// parks on this; a child's exit path publishes it against its parent.
///
/// Distinct from [`child_exit_event`], which is keyed on the exiting task: one
/// id space for both would make "my child exited" indistinguishable from "I
/// exited".
#[inline]
pub fn any_child_exit_event(parent_task_id: u32) -> KernelEvent {
    KernelEvent::AnyChildExit {
        parent: TaskSlot(parent_task_id),
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

/// Stamp `task->cpu_affinity` and `task->last_cpu` from a single
/// boot-time idle-task install.
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

/// Wake every task currently blocked waiting for this task to exit. Caller
/// must keep the task alive long enough to resolve its id.
#[inline]
pub fn task_wake_all_waiters<K, U>(task: &TaskInner<K, U>) {
    BUS.publish(child_exit_event(task.task_id));
}

/// Raise `mask` on `task`'s pending set **and** wake any `signalfd` poller
/// registered on it. The chokepoint for in-band delivery: a masked signal
/// stays pending (so it does not interrupt a wait with EINTR) yet a poller
/// subscribed to its `SignalPending` queue is still woken to drain it.
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
/// The only writer of [`SIGNAL_KILLED`]. The two halves are fused: the bit
/// without a wake leaves a task parked in a blocking primitive that never
/// re-runs its abort probe, and a wake without the bit is spurious.
///
/// Returns `true` if this call did the marking. The wake is issued either way
/// — a redundant unblock is a no-op, a lost one is not.
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
/// The disposition-aware chokepoint every signal *send* routes through. A
/// signal that would be discarded anyway — handler is `SIG_IGN`, or `SIG_DFL`
/// with a default of [`SigDefault::Ignore`] — and is **not blocked** is
/// dropped here instead of being left pending, so it never spuriously wakes a
/// blocked task only to be consumed as a no-op at the delivery point. Blocked
/// signals always pend regardless of disposition: a `signalfd` reader or a
/// later-installed handler may still drain them after unblocking.
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

#[inline]
pub fn task_reset_fpu_state<K, U>(task: &mut TaskInner<K, U>) {
    // SAFETY: `&mut Task` gives exclusive access to the `fpu_state` slot the
    // reset routine overwrites with a fresh `FpuState`.
    unsafe {
        crate::task::kernel_task::fpu_reset_in_place(task.fpu_state.get_mut());
    }
}

/// Reset every *caught* signal (a handler other than `SIG_DFL`/`SIG_IGN`) to
/// `SIG_DFL` — the execve disposition reset. POSIX keeps ignored signals
/// ignored, so `SIG_IGN` entries are left untouched; the blocked mask and
/// pending set are preserved across exec by the caller.
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
/// handler or `SIG_IGN`. Backs POSIX_SPAWN_SETSIGDEF and the `sigdefault`
/// syscall.
#[inline]
pub fn task_default_signals_in_mask<K, U>(task: &TaskInner<K, U>, mask: SigSet) {
    for signum in 1..=NSIG {
        if mask & sig_bit(signum as u8) != 0 {
            task.signal_actions[signum - 1].reset();
        }
    }
}

/// Write a kernel-mode trampoline return-address into the slot at
/// `kernel_stack_top - 8`, seeding the first `ret` of a kernel task's switch
/// frame. Caller must hold exclusive access to the kernel stack.
#[inline]
pub fn task_kernel_stack_seed_ret(kernel_stack_top: u64, trampoline: u64) {
    // SAFETY: the slot at `top - 8` of a caller-owned kernel stack is
    // reserved for the synthetic return address.
    unsafe {
        let ret_addr_ptr = (kernel_stack_top - 8) as *mut u64;
        core::ptr::write(ret_addr_ptr, trampoline);
    }
}

/// Clone `other` into `dest` in place. Caller must hold exclusive `&mut`
/// access to `dest` and ensure `other` names a different slot.
#[inline]
pub fn task_clone_from<K, U>(dest: &mut TaskInner<K, U>, other: &TaskInner<K, U>) {
    // SAFETY: `dest` is exclusive and `other` is a distinct shared borrow;
    // `clone_from_raw` bulk-copies while preserving atomics' values.
    unsafe { dest.clone_from_raw(other) };
}

#[inline]
pub fn task_has_deliverable_signal<K, U>(task: &TaskInner<K, U>) -> bool {
    (task.signal_pending() & SIGNAL_MASK & !task.signal_blocked()) != 0
}
