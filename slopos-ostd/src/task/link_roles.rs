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

/// Role tag for a task's membership in its parent's list of children.
///
/// Each task carries one `Link<Task, SiblingRole>` slot naming its node in its
/// parent's `children` list; the parent owns the list head. A task appears in at
/// most one parent's children list, so the same single-membership invariant the
/// other roles rely on rejects a double-link. This is distinct from the
/// scheduler roles: a task can be simultaneously in a ready queue (via its ready
/// link) and in its parent's children list (via this sibling link), because the
/// two memberships are independent ownership edges.
pub enum SiblingRole {}
