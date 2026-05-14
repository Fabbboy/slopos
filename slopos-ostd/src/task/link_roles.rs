//! Role markers for the per-CPU ready queue and the global zombie list.
//!
//! These tag types parameterise the intrusive `Link<Task, Role>` slots
//! on the kernel-side `Task` struct. They live in OSTD (rather than in
//! `core::scheduler`) because the kernel `Task` body — and its
//! `LinkProvider<Role>` impls — now also lives in OSTD. The kernel-side
//! `core::scheduler::per_cpu` and `core::scheduler::task::task_table`
//! re-export these under their historical paths so existing callers
//! continue to resolve.

/// Role tag for the per-CPU `ReadyQueue` intrusive list.
pub enum ReadyQueueRole {}

/// Role tag for the global `ZombieList` intrusive list — distinct from
/// `ReadyQueueRole` so the two roles use different `Link<_>` slots on
/// the `Task` struct (enforced by the `Linked<Role>` trait's
/// "distinct field per role" rule).
pub enum ZombieListRole {}
