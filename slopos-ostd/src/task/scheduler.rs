//! Scheduler / run-queue trait surface.
//!
//! The production preemptive scheduler lives in the out-of-OSTD
//! `slopos-sched` crate. OSTD only defines the trait surface and
//! reference type so consumers can hand tasks to whichever scheduler
//! the kernel boots with; registration happens through
//! [`crate::task::scheduler_registry::register_scheduler`].

use crate::mm::KArc;
use crate::task::task::Task;

/// Reference-counted handle to a [`Task`] usable by the scheduler.
pub type TaskRef = KArc<Task>;

/// The scheduler trait.
///
/// Implementations decide how tasks are distributed across CPUs. The
/// trait keeps the surface intentionally narrow so alternative
/// implementations can be swapped in without touching the kernel.
pub trait Scheduler: Send + Sync {
    /// Make `task` runnable. Implementations may push it onto the
    /// caller's CPU run-queue or onto whichever CPU's queue minimises
    /// imbalance.
    fn enqueue(&self, task: TaskRef);

    /// Borrow the local CPU's run queue mutably for the duration of
    /// `f`. The dyn-trait shape lets implementations encapsulate
    /// per-CPU locking; callers don't need to know whether the run
    /// queue is per-CPU or shared.
    fn local_rq_with(&self, f: &mut dyn FnMut(&mut dyn RunQueue));
}

/// The local run-queue trait.
pub trait RunQueue {
    /// Bookkeeping called every timer tick (account vruntime, etc.).
    fn update_curr(&mut self);

    /// Pick the next task to dispatch. Returns `None` if the queue is
    /// empty (the caller dispatches the idle task).
    fn pick_next(&mut self) -> Option<TaskRef>;

    /// Remove the currently-running task from the queue (e.g., it
    /// blocked or terminated). Returns the removed task if any.
    fn dequeue_curr(&mut self) -> Option<TaskRef>;
}
