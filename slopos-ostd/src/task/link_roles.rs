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
