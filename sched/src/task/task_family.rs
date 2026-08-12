//! Parent/child task ownership.
//!
//! A parent owns each of its children — live and zombie — through the intrusive
//! `children` list on the parent `Task`. Membership in that list is one strong
//! reference parked exactly like ready-queue placement: linking a child pairs a
//! list push with [`task_placement_retain`], unlinking pairs a list removal with
//! [`TaskRef::from_placement`]. So a zombie is simply a dead child still parked in
//! its parent's list; `waitpid` reaps it by unlinking (dropping the parked
//! reference off-lock), and a dying parent reaps or reparents by draining the
//! list. The child→parent direction stays a plain id (`parent_task_id`) resolved
//! through the registry — the registry is the single liveness index.
//!
//! Every list mutation runs under the registry lock ([`with_task_manager`]): the
//! intrusive ops and the strong-count park/reclaim are allocation-free, so they
//! are safe under the cli-spinlock, and the heavy destructor is never run under
//! the lock — these helpers hand the reclaimed guard back for an off-lock drop.
//! That drop is never final while the child is live: a task holds its own
//! existence reference from registration until it is reaped.

use core::ptr::NonNull;

use slopos_ostd::klog_info;
use slopos_ostd::task::{task_placement_retain, with_parked_node};

use super::task_table::{TaskRef, task_find_by_id, with_task_manager};
use super::{INVALID_TASK_ID, Task, TaskStatus};

/// How many unreaped zombies one parent may hold.
///
/// A zombie is retained so a live parent can still read its exit status, which
/// makes retention a promise the *parent* is supposed to redeem. A parent that
/// never calls `waitpid` and never exits never redeems it, and each held
/// receipt pins a `Task` (≤ 8 KiB), a 32 KiB kernel stack, a 16 KiB data stack
/// and one of `MAX_TASKS` registry slots — so an interactive loop of
/// spawn-and-close walks the machine to spawn failure. `SIG_IGN` and
/// `waitpid(-1)` let a supervisor avoid that; this is what makes avoiding it
/// not the supervisor's decision.
///
/// Linux bounds the same quantity with `RLIMIT_NPROC`, per-uid rather than
/// per-parent. SlopOS has no uid, and the parent is the principal that
/// actually owes the reap, so the cap lands there.
pub const MAX_ZOMBIES_PER_PARENT: usize = 64;

/// Whether a task in `status` may still acquire children — i.e. it has not begun
/// tearing down. Mirrors the "parent alive" predicate teardown keys on.
#[inline]
fn status_can_parent(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Ready | TaskStatus::Running | TaskStatus::Blocked
    )
}

/// Publish `child` as a child of `parent`: record the parent id on the child and
/// park one owning reference in the parent's children list.
///
/// If `parent` is already tearing down (its children drain may have run), the
/// child is orphaned instead — its parent id is cleared so its own exit skips
/// the Zombie state and it is never stranded in a dead parent's list. The status
/// check and the list push are one registry-lock critical section, so a push
/// either lands before the parent's teardown drain (which then reaps/orphans the
/// child) or observes the dying status and orphans here.
pub fn link_child(parent: &Task, child: NonNull<Task>) {
    with_task_manager(|_mgr| {
        // The parent arrives as a borrow now: it is only ever read here, and a
        // raw parameter would have been a task handle with no owner for a
        // status check and an id copy.
        // The caller holds the child's reference across this call — it is
        // about to be parked in the list below — so the node borrows through
        // the sanctioned surface rather than through a raw pointer.
        if !status_can_parent(parent.status()) {
            with_parked_node(child, |child| child.set_parent_task_id(INVALID_TASK_ID));
            return;
        }
        with_parked_node(child, |c| c.set_parent_task_id(parent.task_id));
        // Push first, park the owning reference only on success: retain pairs
        // one-to-one with membership. A fresh/forked child is unlinked, so the
        // push cannot fail in practice; if it did, no retain is leaked.
        if parent.children.push_back(child).is_ok() {
            task_placement_retain(child);
        }
    });
}

/// Detach one child from `parent`'s list and hand back the owning reference the
/// list held. `None` when the list is empty. The returned guard must be dropped
/// off-lock; it is never the last reference, because the child holds its own
/// existence reference until it is reaped, so the drop is a bare decrement.
pub fn take_one_child(parent: &Task) -> Option<TaskRef> {
    let child_nn = with_task_manager(|_mgr| parent.children_pop())?;
    Some(TaskRef::from_placement(child_nn))
}

/// The oldest zombie in `parent`'s children list once it holds more than
/// [`MAX_ZOMBIES_PER_PARENT`], or `None` while it is within budget.
///
/// Oldest-first because the list is push-back ordered, so the head zombie is
/// the one whose status has gone unclaimed longest — the one a parent that was
/// ever going to reap would have reaped already. Dropping the newest instead
/// would discard the status most likely still to be waited on.
///
/// Runs under the registry lock: the walk is allocation-free and touches only
/// the intrusive links and each child's status word.
fn overflowing_zombie(parent: &Task) -> Option<NonNull<Task>> {
    let mut zombies = 0usize;
    let mut oldest = None;
    for child in parent.children.iter() {
        let is_zombie = with_parked_node(child, |c| c.status() == TaskStatus::Zombie);
        if !is_zombie {
            continue;
        }
        zombies += 1;
        if oldest.is_none() {
            oldest = Some(child);
        }
    }
    if zombies > MAX_ZOMBIES_PER_PARENT {
        oldest
    } else {
        None
    }
}

/// Enforce [`MAX_ZOMBIES_PER_PARENT`] on `parent`, force-reaping the oldest
/// zombie when the budget is exceeded.
///
/// Called from the exit path after a child has been stamped `Zombie`, so at
/// most one child is over budget per call and one eviction restores it.
///
/// The evicted child's exit status is dropped, which is the whole cost: a
/// parent that has accumulated this many unreaped children is not reading exit
/// codes. Losing one status beats losing the ability to spawn.
pub fn enforce_zombie_budget(parent: &Task) {
    let Some(victim) = with_task_manager(|_mgr| {
        let victim = overflowing_zombie(parent)?;
        // Transition and unlink in the same critical section that chose the
        // victim: off-lock, `waitpid` could reap it first and this would then
        // unlink a node the parent's list no longer owns.
        let demoted = with_parked_node(victim, |c| c.try_transition_to(TaskStatus::Terminated));
        if !demoted {
            return None;
        }
        parent.children_remove(victim).ok()?;
        Some(TaskRef::from_placement(victim))
    }) else {
        return;
    };

    let victim_id = victim.task_id;
    klog_info!(
        "task {} exceeded {} unreaped children; dropping exit status of task {}",
        parent.task_id,
        MAX_ZOMBIES_PER_PARENT,
        victim_id
    );
    victim.set_parent_task_id(INVALID_TASK_ID);
    // Off-lock, and never the last reference: the child holds its own
    // existence reference until the reap below retires its registration.
    super::task_put(victim);
    let _ = super::task_table::task_reap(victim_id);
}

/// The id of `parent_id`'s first exited-but-unreaped child, or `None`.
///
/// Backs `waitpid(-1)`. Head-first, so the child whose status has gone
/// unclaimed longest is reaped first and a busy parent cannot starve one.
pub fn task_first_exited_child(parent_id: u32) -> Option<u32> {
    let parent = task_find_by_id(parent_id)?;
    with_task_manager(|_mgr| {
        for child in parent.children.iter() {
            let found = with_parked_node(child, |c| {
                (c.status() == TaskStatus::Zombie).then_some(c.task_id)
            });
            if found.is_some() {
                return found;
            }
        }
        None
    })
}

/// Whether `parent_id` owns any child at all.
///
/// `waitpid(-1)` needs this to tell "no child has exited yet" (block) from "no
/// children exist" (`ECHILD`).
pub fn task_has_children(parent_id: u32) -> bool {
    let Some(parent) = task_find_by_id(parent_id) else {
        return false;
    };
    with_task_manager(|_mgr| !parent.children_is_empty())
}

/// Block until one of `parent_id`'s children exits.
///
/// Interruptible: a signal aborts with `EINTR`, matching `waitpid`'s
/// documented behaviour, and a kill aborts the same way so the waiter unwinds
/// on its own stack rather than being torn down inside the wait.
///
/// The predicate re-scans rather than trusting the wake, so a bucket collision
/// on the event queue costs a re-scan and cannot report a stranger's child.
pub fn task_wait_any_child(parent_id: u32) -> Result<(), slopos_abi::Errno> {
    let waited = slopos_ostd::sync::BUS
        .subscribe(slopos_ostd::task::ops::any_child_exit_event(parent_id))
        .wait_event_interruptible(|| task_first_exited_child(parent_id).is_some());
    if waited.is_err() {
        return Err(slopos_abi::Errno::EINTR);
    }
    Ok(())
}

/// Detach `child` from its parent's children list and hand back the owning
/// reference the list held. `None` when the child has no parent, the parent is
/// already gone, or the list did not hold it.
///
/// Takes the guard rather than a pointer because that is what makes the child
/// addressable here: the caller has already made it reapable, so a peer CPU's
/// deferred-reap drain may be retiring its registration concurrently, and the
/// guard is the only thing holding the allocation.
pub fn unlink_child(child: &TaskRef) -> Option<TaskRef> {
    let parent_id = child.parent_task_id();
    if parent_id == INVALID_TASK_ID {
        return None;
    }
    let parent = task_find_by_id(parent_id)?;
    let child_nn = child.node();
    let removed = with_task_manager(|_mgr| parent.children_remove(child_nn).is_ok());
    if removed {
        Some(TaskRef::from_placement(child_nn))
    } else {
        None
    }
}
