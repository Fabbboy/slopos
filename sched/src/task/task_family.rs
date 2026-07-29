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

use slopos_ostd::task::{task_placement_retain, with_parked_node};

use super::task_table::{TaskRef, task_find_by_id, with_task_manager};
use super::{INVALID_TASK_ID, Task, TaskStatus};

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
