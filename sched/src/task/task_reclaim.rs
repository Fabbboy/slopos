//! Universal task-reference release, and the graveyard that makes it safe from
//! any context.
//!
//! `Task`'s destructor takes allocator locks, so running it with interrupts
//! disabled, under a lock, or on the dying task's own stack deadlocks — yet the
//! final reference legitimately drops in exactly those contexts. [`task_put`]
//! always decrements immediately, destroys inline only when the context allows,
//! and otherwise parks the allocation here and arms the bottom half so the
//! corpse is collected at the next outermost unlock rather than when a CPU next
//! runs out of work.
//!
//! The stack needs no lock: only the winner of the one-to-zero strong
//! transition pushes, so the pusher owns the node outright and can push from
//! under the registry lock. That uniqueness is carried by the `ParkedTask`
//! token, which every operation here takes by value, so a double-destroy does
//! not typecheck.

use core::ptr;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_ostd::KArc;
use slopos_ostd::task::{
    ParkedTask, task_destroy_parked, task_parked_leak, task_parked_reclaim, task_release_strong,
    with_parked, with_parked_node,
};

use super::{Task, TaskRef};
use crate::task_stack::{KernelStack, UnsafeStack};

type ParkedTaskRef = ParkedTask<KernelStack, UnsafeStack>;

/// Tasks whose final reference was released where the destructor could not run.
///
/// Global rather than per-CPU: a per-CPU stack strands its contents when that
/// CPU stops idling or is paused for teardown.
static TASK_GRAVEYARD: AtomicPtr<Task> = AtomicPtr::new(ptr::null_mut());

/// Release one strong task reference from **any** context.
///
/// The sole sanctioned way to drop a `KArc<Task>`. A final release destroys
/// inline when the context allows and otherwise parks the allocation for
/// [`task_graveyard_drain`].
#[inline]
pub fn task_put(task: TaskRef) {
    release_arc(task.into_arc());
}

/// The single release sequence, reached only from [`task_put`] and
/// [`TaskRef`]'s `Drop`.
#[inline]
pub(super) fn release_arc(arc: KArc<Task>) {
    let Some(parked) = task_release_strong(arc) else {
        return;
    };
    if destroy_context_is_safe(&parked) {
        task_destroy_parked(parked);
    } else {
        graveyard_push(parked);
    }
}

/// Whether the current context may run `Task`'s destructor.
///
/// A property of the *context*, never of a reference count: a count-based test
/// cannot be made race-free. Holding `&ParkedTaskRef` is what keeps a body whose
/// strong count is already zero alive across the read below.
fn destroy_context_is_safe(parked: &ParkedTaskRef) -> bool {
    if !slopos_ostd::task::drop_context_is_safe() {
        return false;
    }
    !with_parked(parked, crate::scheduler::task_is_dispatch_pinned)
}

/// Park a uniquely-owned dead task for later destruction.
fn graveyard_push(parked: ParkedTaskRef) {
    // Claimed outside the assert: `debug_assert!` does not evaluate its
    // expression in release builds.
    let claimed = with_parked(&parked, |task| task.reclaim_link().try_mark_linked());
    debug_assert!(
        claimed,
        "task pushed to the graveyard twice: two threads believed they won the \
         same final release"
    );
    // Past this point the address is reachable to every other drainer, so the
    // successor writes go through a raw pointer: a `&Task` live across the
    // publishing CAS would alias a task another CPU may already be destroying.
    let node = task_parked_leak(parked);
    loop {
        let head = TASK_GRAVEYARD.load(Ordering::Acquire);
        with_parked_node(node, |task| task.reclaim_link().store_relaxed(head));
        if TASK_GRAVEYARD
            .compare_exchange_weak(head, node.as_ptr(), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            // Without this the corpse waits for a CPU to run out of work,
            // which under sustained fork/exit load never happens.
            slopos_ostd::sync::bh::raise();
            return;
        }
        core::hint::spin_loop();
    }
}

#[inline]
pub fn task_graveyard_pending() -> bool {
    !TASK_GRAVEYARD.load(Ordering::Acquire).is_null()
}

/// Destroy every task parked so far.
///
/// Must be called with interrupts enabled and no lock held. Detaching the whole
/// chain with a swap rather than popping makes this ABA-free by construction,
/// so several CPUs may drain concurrently, each taking a disjoint chain.
pub fn task_graveyard_drain() {
    let mut cursor = TASK_GRAVEYARD.swap(ptr::null_mut(), Ordering::AcqRel);
    while let Some(node) = NonNull::new(cursor) {
        // The one place unique ownership rests on an argument rather than on
        // the type: the swap above detached this whole chain atomically, so no
        // other drainer can hold the same node.
        let parked = task_parked_reclaim(node);

        // Read the successor and clear membership before destroying: the
        // destructor frees the memory the link lives in.
        cursor = with_parked(&parked, |task| {
            let next = task.reclaim_link().load();
            task.reclaim_link().store_relaxed(ptr::null_mut());
            task.reclaim_link().mark_unlinked();
            next
        });

        task_destroy_parked(parked);
    }
}
