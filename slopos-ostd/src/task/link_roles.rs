//! Role markers for intrusive scheduler containers.
//!
//! These tag types parameterise the intrusive `Link<Task, Role>` slots
//! on the kernel-side `Task` struct. They live in OSTD (rather than in
//! `core::scheduler`) because the kernel `Task` body and its
//! `LinkProvider<Role>` impls also live in OSTD.

/// Role tag for the per-CPU `ReadyQueue` intrusive list.
pub enum ReadyQueueRole {}

/// Role tag for a per-CPU remote wake inbox entry.
///
/// The inbox is implemented as a lock-free Treiber stack rather than an
/// `IntrusiveLinkedList`, but it still needs the same single-membership
/// invariant as the ready and zombie lists. Giving it its own role-typed
/// `Link<Task, RemoteWakeRole>` means a task cannot accidentally reuse its
/// ready-queue link as a remote-wake link, and duplicate pushes are rejected by
/// the link slot itself instead of by ad-hoc parallel state.
pub enum RemoteWakeRole {}

/// Role tag for a task's membership in the one *owner list* holding it.
///
/// Each task carries one `DLink<Task, SiblingRole>` slot naming its node in
/// either its parent's `children` list or the global list of parentless tasks;
/// exactly one of those holds it for its whole registered lifetime, and that
/// membership is the task's owning reference. A task appears in at most one, so
/// the single-membership invariant the other roles rely on rejects a
/// double-link. This is distinct from the scheduler roles: a task can be
/// simultaneously in a ready queue (via its ready link) and in its owner list
/// (via this slot), because the two memberships are independent ownership
/// edges.
///
/// The slot is doubly linked so that removal is O(1) and so that a task can be
/// unlinked without the caller first deciding *which* owner list holds it.
pub enum SiblingRole {}

/// Role tag for the task graveyard: the lock-free stack of tasks whose last
/// strong reference was released in a context that could not run the
/// allocator-heavy destructor.
///
/// Deliberately its own role rather than a reuse of an existing slot. Every
/// other role obeys "linked implies owned" — membership carries one parked
/// strong reference. A graveyard node is the single linked-but-*not*-owned
/// state in the system: its strong count is already zero and the pusher owns
/// the allocation outright. Keeping that state in a distinct slot means the
/// two universes cannot be confused at the type level, and lets `Task::drop`
/// assert it is not running on a still-parked node.
pub enum ReclaimRole {}
