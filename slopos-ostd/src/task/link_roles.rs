//! Role markers parameterising the intrusive `Link<Task, Role>` slots on
//! `Task`. Each role is a distinct slot, so single membership is enforced
//! per container rather than globally.

/// Role tag for the per-CPU `ReadyQueue` intrusive list.
pub enum ReadyQueueRole {}

/// Role tag for a per-CPU remote wake inbox entry.
///
/// The inbox is a lock-free Treiber stack; the role-typed slot is what rejects
/// a duplicate push, instead of ad-hoc parallel state.
pub enum RemoteWakeRole {}

/// Role tag for a task's membership in the one *owner list* holding it.
///
/// The slot names the task's node in either its parent's `children` list or the
/// global parentless list; exactly one holds it for its whole registered
/// lifetime, and that membership is the task's owning reference. Independent of
/// the scheduler roles, so a task can be ready-queued and owner-listed at once.
/// Doubly linked so removal is O(1) and needs no prior decision about which
/// owner list holds it.
pub enum SiblingRole {}

/// Role tag for the task graveyard: the lock-free stack of tasks whose last
/// strong reference was released where the allocator-heavy destructor could not
/// run.
///
/// Its own role because it is the one linked-but-*not*-owned state: the strong
/// count is already zero and the pusher owns the allocation outright, whereas
/// every other role obeys "linked implies owned".
pub enum ReclaimRole {}
